//! Build Fetcher's settings, typed, over `bf_core::app_config` (settings.json in %APPDATA%\build-fetcher).
//!
//! Every field has a default and is validated on its own: a bad or missing value falls back to its default (with a
//! warning) instead of failing the whole file, so one hand-edited mistake never loses the rest. Writes merge only
//! Build Fetcher's keys into the file (keys this version doesn't know are kept) and replace it atomically.
//!
//! The API key lives in the same file (`riotApiKey`) but never in `Settings`: `Settings` is what the dashboard
//! shows and edits, and the key must never reach the dashboard's JSON. It is read with `api_key` and shown only as
//! `KeyStatus::masked` (`bf_core::config::mask_key`).

use anyhow::{bail, Result};
use bf_core::build::{Platform, RankBand};
use bf_core::config::mask_key;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub const GB: u64 = 1024 * 1024 * 1024;
pub const DEFAULT_DISK_BUDGET: u64 = 2 * GB;
/// Below this the store can't hold one patch of counts plus the validation sample; above, it is surely a typo.
pub const MIN_DISK_BUDGET: u64 = 256 * 1024 * 1024;
pub const MAX_DISK_BUDGET: u64 = 1024 * GB;
pub const MAX_TIER_A: usize = 20;
/// The share of tier B games whose timeline is fetched (tier A games always get theirs). 0.25 since 1.2.0 (0.1
/// before): the model learns item choices and the enemy's items only from games with a timeline. A settings file that
/// already holds a value keeps it; Settings → Regions and rank changes it.
pub const DEFAULT_TIMELINE_SAMPLE: f64 = 0.25;

/// The settings file's key for the API key.
pub const KEY_FIELD: &str = "riotApiKey";
/// The settings file's key for the extra API keys (a list; proof of concept for testing a larger budget).
pub const EXTRA_KEYS_FIELD: &str = "riotExtraApiKeys";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// Platforms games are sourced from (EUN1, EUW1, KR by default).
    pub regions: Vec<Platform>,
    /// The rank games are sampled from first: MasterPlus (default) or Diamond. Never `Mine`.
    pub rank_floor: RankBand,
    /// Widen thin champion-role pairs from Master+ to Diamond automatically.
    pub widen_to_diamond: bool,
    pub disk_budget_bytes: u64,
    /// Tier A champions the user added (detected ones are added by the fetcher at run time, not saved here).
    pub tier_a: Vec<i64>,
    /// Training, evaluation and similarity sums on an NVIDIA GPU. Off by default (the user's choice): the GPU path
    /// is checked against the processor and falls back to it, but was not run on real hardware before 1.0.
    pub gpu: bool,
    pub learning: bool,
    /// Fetching on (the default) or paused by the user (Fetching → Pause). Kept across restarts.
    #[serde(default = "yes")]
    pub fetching: bool,
    /// "Name#TAG", as the user typed it.
    pub riot_id: Option<String>,
    pub puuid: Option<String>,
    /// The platform the user plays on (their own games are fetched from it).
    pub my_platform: Platform,
    /// Share of tier B games with a timeline (0-1).
    pub timeline_sample: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            regions: Platform::ALL.to_vec(),
            rank_floor: RankBand::MasterPlus,
            widen_to_diamond: true,
            disk_budget_bytes: DEFAULT_DISK_BUDGET,
            tier_a: vec![],
            gpu: false,
            learning: true,
            fetching: true,
            riot_id: None,
            puuid: None,
            my_platform: Platform::Eun1,
            timeline_sample: DEFAULT_TIMELINE_SAMPLE,
        }
    }
}

/// (json key, check) per field, in the order the file lists them.
const FIELDS: [&str; 12] =
    ["regions", "rankFloor", "widenToDiamond", "diskBudgetBytes", "tierA", "gpu", "learning", "fetching", "riotId", "puuid", "myPlatform", "timelineSample"];

fn yes() -> bool {
    true
}

impl Settings {
    /// Every problem with these settings (empty when valid).
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.regions.is_empty() {
            out.push("regions: choose at least one region".into());
        }
        let mut r = self.regions.clone();
        r.sort();
        r.dedup();
        if r.len() != self.regions.len() {
            out.push("regions: listed twice".into());
        }
        if self.rank_floor == RankBand::Mine {
            out.push("rankFloor: must be masterPlus or diamond".into());
        }
        if !(MIN_DISK_BUDGET..=MAX_DISK_BUDGET).contains(&self.disk_budget_bytes) {
            out.push(format!("diskBudgetBytes: must be between {} MB and {} GB", MIN_DISK_BUDGET >> 20, MAX_DISK_BUDGET / GB));
        }
        if self.tier_a.len() > MAX_TIER_A {
            out.push(format!("tierA: at most {MAX_TIER_A} champions"));
        }
        if self.tier_a.iter().any(|c| *c <= 0) {
            out.push("tierA: champion ids are positive".into());
        }
        let mut t = self.tier_a.clone();
        t.sort();
        t.dedup();
        if t.len() != self.tier_a.len() {
            out.push("tierA: listed twice".into());
        }
        if let Some(id) = &self.riot_id {
            if !valid_riot_id(id) {
                out.push("riotId: expected Name#TAG".into());
            }
        }
        if let Some(p) = &self.puuid {
            if p.is_empty() || p.len() > 100 || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                out.push("puuid: not a puuid".into());
            }
        }
        if !(0.0..=1.0).contains(&self.timeline_sample) || self.timeline_sample.is_nan() {
            out.push("timelineSample: must be between 0 and 1".into());
        }
        out
    }

    pub fn validate(&self) -> Result<()> {
        let p = self.problems();
        if p.is_empty() {
            Ok(())
        } else {
            bail!("Invalid settings: {}", p.join("; "))
        }
    }

    /// Settings from a settings.json map: each field read on its own, falling back to its default when missing or
    /// invalid. Returns the warnings for invalid (not missing) fields.
    pub fn from_map(map: &Map<String, Value>) -> (Settings, Vec<String>) {
        let d = Settings::default();
        let mut s = d.clone();
        let mut warnings = Vec::new();
        for key in FIELDS {
            let Some(v) = map.get(key).filter(|v| !v.is_null()) else { continue };
            let mut probe = serde_json::to_value(&s).expect("settings serialize");
            probe[key] = v.clone();
            match serde_json::from_value::<Settings>(probe) {
                Ok(next) => {
                    // A field that parses but breaks a rule falls back alone.
                    let before = s.problems().len();
                    if next.problems().len() > before {
                        warnings.push(format!("{key}: invalid value ignored ({})", next.problems().join("; ")));
                    } else {
                        s = next;
                    }
                }
                Err(_) => warnings.push(format!("{key}: unreadable value ignored")),
            }
        }
        (s, warnings)
    }

    /// The (key, value) pairs Build Fetcher writes (never the API key).
    pub fn to_patch(&self) -> Vec<(&'static str, Option<Value>)> {
        let v = serde_json::to_value(self).expect("settings serialize");
        FIELDS.iter().map(|k| (*k, Some(v[*k].clone()).filter(|x| !x.is_null()))).collect()
    }
}

fn valid_riot_id(id: &str) -> bool {
    match id.split_once('#') {
        Some((name, tag)) => {
            let (n, t) = (name.trim().chars().count(), tag.trim().chars().count());
            (3..=16).contains(&n) && (2..=5).contains(&t) && !tag.contains('#')
        }
        None => false,
    }
}

/// settings.json: %APPDATA%\build-fetcher\settings.json, or BF_CONFIG_DIR's.
pub fn settings_file() -> PathBuf {
    bf_core::app_config::app_config_file()
}

pub fn load(file: &Path) -> (Settings, Vec<String>) {
    Settings::from_map(&bf_core::app_config::read_config_at(file))
}

/// Validates and writes. Other keys in the file (the API key, keys of other versions) are kept.
pub fn save(file: &Path, s: &Settings) -> Result<()> {
    s.validate()?;
    bf_core::app_config::update_config_at(file, s.to_patch()).map_err(|(_, e)| anyhow::anyhow!("Could not write settings: {e}"))?;
    Ok(())
}

// ------------------------------------------------------------------------------------------------ the API key

/// What the dashboard may know about the key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyStatus {
    pub set: bool,
    /// "RGAPI-…1234" or "".
    pub masked: String,
}

/// The stored key (trimmed), None when none is set. Never log or display this: use `key_status`.
pub fn api_key(file: &Path) -> Option<String> {
    bf_core::app_config::read_config_at(file).get(KEY_FIELD).and_then(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn key_status(file: &Path) -> KeyStatus {
    let k = api_key(file).unwrap_or_default();
    KeyStatus { set: !k.is_empty(), masked: mask_key(&k) }
}

/// Checks a key's shape without saying anything about its value (errors never contain it).
pub fn check_key(key: &str) -> Result<String> {
    let k = key.trim();
    if k.is_empty() {
        bail!("The key is empty");
    }
    if k.len() > 128 || k.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("That doesn't look like a Riot API key (it should be one line, like RGAPI-…)");
    }
    Ok(k.to_string())
}

/// Stores a key (pasted or imported) in settings.json. Returns its masked form.
pub fn set_key_at(file: &Path, key: &str) -> Result<String> {
    let k = check_key(key)?;
    bf_core::app_config::update_config_at(file, vec![(KEY_FIELD, Some(json!(k)))]).map_err(|(_, e)| anyhow::anyhow!("Could not write settings: {e}"))?;
    bf_core::config::set_riot_api_key(&k);
    Ok(mask_key(&k))
}

// ------------------------------------------------------------------------------------------------ extra keys

/// The stored extra keys (trimmed, blanks skipped), in the order they were added. Never log or display these.
pub fn extra_keys(file: &Path) -> Vec<String> {
    bf_core::app_config::read_config_at(file)
        .get(EXTRA_KEYS_FIELD)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

/// Every key the client should use: the main key first, then the extra keys, without repeats.
pub fn all_keys(file: &Path) -> Vec<String> {
    let mut out: Vec<String> = api_key(file).into_iter().collect();
    for k in extra_keys(file) {
        if !out.contains(&k) {
            out.push(k);
        }
    }
    out
}

/// The extra keys as the dashboard may see them (masked), in order.
pub fn extra_keys_masked(file: &Path) -> Vec<String> {
    extra_keys(file).iter().map(|k| mask_key(k)).collect()
}

fn write_extra_keys(file: &Path, keys: &[String]) -> Result<()> {
    let v = if keys.is_empty() { None } else { Some(json!(keys)) };
    bf_core::app_config::update_config_at(file, vec![(EXTRA_KEYS_FIELD, v)]).map_err(|(_, e)| anyhow::anyhow!("Could not write settings: {e}"))?;
    Ok(())
}

/// Adds an extra key. There is no limit to how many. Returns its masked form; errors never contain the key.
pub fn add_extra_key_at(file: &Path, key: &str) -> Result<String> {
    let k = check_key(key)?;
    if api_key(file).as_deref() == Some(k.as_str()) {
        bail!("That key is already the main key");
    }
    let mut keys = extra_keys(file);
    if keys.contains(&k) {
        bail!("That key is already in the list");
    }
    keys.push(k.clone());
    write_extra_keys(file, &keys)?;
    Ok(mask_key(&k))
}

/// Removes the extra key at `index` (as `extra_keys_masked` lists them).
pub fn remove_extra_key_at(file: &Path, index: usize) -> Result<()> {
    let mut keys = extra_keys(file);
    if index >= keys.len() {
        bail!("No extra key {}", index + 1);
    }
    keys.remove(index);
    write_extra_keys(file, &keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_round_trip() {
        let d = Settings::default();
        assert!(d.problems().is_empty());
        assert_eq!(d.disk_budget_bytes, 2 * GB);
        assert_eq!(d.regions, vec![Platform::Eun1, Platform::Euw1, Platform::Kr]);
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("settings.json");
        let s = Settings { tier_a: vec![39, 103], riot_id: Some("Someone#EUNE".into()), disk_budget_bytes: 5 * GB, ..d.clone() };
        save(&f, &s).unwrap();
        let (back, w) = load(&f);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(back, s);
        // Missing file: defaults, no warnings.
        assert_eq!(load(&dir.path().join("none.json")), (Settings::default(), vec![]));
    }

    #[test]
    fn invalid_fields_fall_back_alone() {
        let map: Map<String, Value> = serde_json::from_value(json!({
            "regions": ["EUW1", "XX9"], "diskBudgetBytes": 10, "tierA": [39, 39], "gpu": true, "timelineSample": 2,
            "rankFloor": "mine", "riotId": "no-tag", "learning": "yes", "someOtherKey": 1
        }))
        .unwrap();
        let (s, w) = Settings::from_map(&map);
        assert_eq!(s.regions, Settings::default().regions);
        assert_eq!(s.disk_budget_bytes, DEFAULT_DISK_BUDGET);
        assert!(s.tier_a.is_empty());
        assert!(s.gpu, "valid fields are kept (true is not the default)");
        assert_eq!(s.timeline_sample, DEFAULT_TIMELINE_SAMPLE);
        assert_eq!(s.rank_floor, RankBand::MasterPlus);
        assert!(s.riot_id.is_none());
        assert!(s.learning);
        assert_eq!(w.len(), 7, "{w:?}");
        assert!(Settings { regions: vec![], ..Default::default() }.validate().is_err());
    }

    #[test]
    fn save_keeps_other_keys_and_never_writes_the_key_into_settings_json_output() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("settings.json");
        std::fs::write(&f, r#"{"riotApiKey":"RGAPI-00000000-test","futureKey":[1,2]}"#).unwrap();
        save(&f, &Settings::default()).unwrap();
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(raw["riotApiKey"], "RGAPI-00000000-test");
        assert_eq!(raw["futureKey"], json!([1, 2]));
        let shown = serde_json::to_string(&load(&f).0).unwrap();
        assert!(!shown.contains("RGAPI"), "the dashboard's settings JSON never carries the key");
        assert_eq!(key_status(&f), KeyStatus { set: true, masked: "RGAPI-…test".into() });
    }

    #[test]
    fn pasted_keys_are_checked_and_errors_never_echo_them() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("settings.json");
        assert_eq!(set_key_at(&f, "  RGAPI-00000000-test \n").unwrap(), "RGAPI-…test");
        assert_eq!(api_key(&f).as_deref(), Some("RGAPI-00000000-test"));
        let e = set_key_at(&f, "RGAPI-0000 0000-test").unwrap_err().to_string();
        assert!(!e.contains("0000"), "{e}");
        assert!(set_key_at(&f, "   ").is_err());
        assert_eq!(api_key(&f).as_deref(), Some("RGAPI-00000000-test"), "a rejected key leaves the old one");
    }

    #[test]
    fn extra_keys_add_list_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("settings.json");
        set_key_at(&f, "RGAPI-00000000-main").unwrap();
        assert!(add_extra_key_at(&f, "RGAPI-00000000-main").is_err(), "the main key is not an extra key");
        assert_eq!(add_extra_key_at(&f, " RGAPI-11111111-aaaa ").unwrap(), "RGAPI-…aaaa");
        assert_eq!(add_extra_key_at(&f, "RGAPI-22222222-bbbb").unwrap(), "RGAPI-…bbbb");
        assert!(add_extra_key_at(&f, "RGAPI-22222222-bbbb").is_err(), "no repeats");
        assert_eq!(extra_keys_masked(&f), vec!["RGAPI-…aaaa", "RGAPI-…bbbb"]);
        assert_eq!(all_keys(&f), vec!["RGAPI-00000000-main", "RGAPI-11111111-aaaa", "RGAPI-22222222-bbbb"]);
        let shown = serde_json::to_string(&load(&f).0).unwrap();
        assert!(!shown.contains("RGAPI"), "the dashboard's settings JSON never carries a key");
        remove_extra_key_at(&f, 0).unwrap();
        assert_eq!(extra_keys_masked(&f), vec!["RGAPI-…bbbb"]);
        assert!(remove_extra_key_at(&f, 5).is_err());
        remove_extra_key_at(&f, 0).unwrap();
        assert!(extra_keys(&f).is_empty());
        assert_eq!(api_key(&f).as_deref(), Some("RGAPI-00000000-main"), "the main key is untouched");
    }
}
