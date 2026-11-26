//! Data Dragon's static data for every champion (items, champions, runes, summoners), cached per version under
//! `<data>/ddragon/<version>/`. From irelia-companion's `ddragon.rs`, without its one-champion parts: a champion's
//! spells are read on demand (`load_champion_spells`).

use crate::config::config;
use crate::js;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

const CDN: &str = "https://ddragon.leagueoflegends.com";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemMeta {
    pub id: i64,
    pub name: String,
    pub tags: Vec<String>,
    pub into: Vec<i64>,
    pub from: Vec<i64>,
    pub special_recipe: Option<i64>,
    pub depth: i64,
    pub gold_total: f64,
    pub purchasable: bool,
    pub on_summoners_rift: bool,
    pub in_store: bool,
    pub passives: Vec<String>,
    pub armor: f64,
    pub magic_resist: f64,
    pub anti_heal: bool,
    /// Every stat the shop shows (1.3.0), indexed by `STAT_*`: read from the description's `<stats>` block, which
    /// lists all of them (the `stats` object lacks penetration, lethality, haste, omnivamp…), else from `stats`.
    /// Percentages as fractions (35% armor penetration = 0.35).
    pub stats: [f64; ITEM_STATS],
    /// Effect flags (`EFFECT_*`) from the tags and the description.
    pub effects: u16,
}

pub const ITEM_STATS: usize = 20;
pub const STAT_AD: usize = 0;
pub const STAT_AP: usize = 1;
pub const STAT_AS: usize = 2;
pub const STAT_CRIT: usize = 3;
pub const STAT_CRIT_DMG: usize = 4;
pub const STAT_LETHALITY: usize = 5;
pub const STAT_ARMOR_PEN: usize = 6;
pub const STAT_MAGIC_PEN: usize = 7;
pub const STAT_MAGIC_PEN_PCT: usize = 8;
pub const STAT_HASTE: usize = 9;
pub const STAT_HP: usize = 10;
pub const STAT_ARMOR: usize = 11;
pub const STAT_MR: usize = 12;
pub const STAT_LIFE_STEAL: usize = 13;
pub const STAT_OMNIVAMP: usize = 14;
pub const STAT_HEAL_SHIELD: usize = 15;
pub const STAT_MS: usize = 16;
pub const STAT_MS_PCT: usize = 17;
pub const STAT_MANA: usize = 18;
pub const STAT_TENACITY: usize = 19;
pub const STAT_NAMES: [&str; ITEM_STATS] = [
    "attack damage",
    "ability power",
    "attack speed",
    "critical strike chance",
    "critical strike damage",
    "lethality",
    "armor penetration",
    "magic penetration",
    "magic penetration %",
    "ability haste",
    "health",
    "armor",
    "magic resist",
    "life steal",
    "omnivamp",
    "heal and shield power",
    "move speed",
    "move speed %",
    "mana",
    "tenacity",
];

pub const EFFECT_ON_HIT: u16 = 1;
pub const EFFECT_ACTIVE: u16 = 2;
pub const EFFECT_AURA: u16 = 4;
pub const EFFECT_SLOW: u16 = 8;
pub const EFFECT_WOUNDS: u16 = 16;
pub const EFFECT_SHIELD: u16 = 32;
/// Damage based on the target's health (max, current or missing).
pub const EFFECT_HEALTH_DAMAGE: u16 = 64;
pub const EFFECT_SPELLBLADE: u16 = 128;

/// A champion's base stats and their growth per level (Data Dragon `stats`), for the combat math (1.3.0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChampionBase {
    pub hp: f64,
    pub hp_per_level: f64,
    pub armor: f64,
    pub armor_per_level: f64,
    pub mr: f64,
    pub mr_per_level: f64,
    pub ad: f64,
    pub ad_per_level: f64,
    pub attack_speed: f64,
    /// Percent per level (2.5 = +2.5% attack speed per level).
    pub attack_speed_per_level: f64,
    pub move_speed: f64,
}

impl ChampionBase {
    /// League's stat growth: base + growth × (level − 1) × (0.7025 + 0.0175 × (level − 1)).
    pub fn grown(base: f64, per_level: f64, level: f64) -> f64 {
        let n = (level.clamp(1.0, 18.0)) - 1.0;
        base + per_level * n * (0.7025 + 0.0175 * n)
    }
    pub fn hp_at(&self, level: f64) -> f64 {
        Self::grown(self.hp, self.hp_per_level, level)
    }
    pub fn armor_at(&self, level: f64) -> f64 {
        Self::grown(self.armor, self.armor_per_level, level)
    }
    pub fn mr_at(&self, level: f64) -> f64 {
        Self::grown(self.mr, self.mr_per_level, level)
    }
    pub fn ad_at(&self, level: f64) -> f64 {
        Self::grown(self.ad, self.ad_per_level, level)
    }
    /// Bonus attack speed from levels, as a fraction (0.30 = +30%).
    pub fn bonus_as_at(&self, level: f64) -> f64 {
        Self::grown(0.0, self.attack_speed_per_level, level) / 100.0
    }
}

/// Reads an item's stat lines: `<stats><attention>35%</attention> Armor Penetration<br>…</stats>`.
pub fn parse_item_stats(desc: &str, stats_obj: Option<&Value>) -> [f64; ITEM_STATS] {
    let mut out = [0.0; ITEM_STATS];
    let block = desc.find("<stats>").and_then(|i| desc[i + 7..].find("</stats>").map(|j| &desc[i + 7..i + 7 + j]));
    if let Some(block) = block {
        for line in block.split("<br>") {
            let text = strip_tags(line);
            let text = text.trim();
            let Some(sp) = text.find(' ') else { continue };
            let (num, name) = (&text[..sp], text[sp + 1..].trim());
            let pct = num.ends_with('%');
            let Ok(v) = num.trim_end_matches('%').parse::<f64>() else { continue };
            let v = if pct { v / 100.0 } else { v };
            let k = match (name, pct) {
                ("Attack Damage", _) => STAT_AD,
                ("Ability Power", _) => STAT_AP,
                ("Attack Speed", _) => STAT_AS,
                ("Critical Strike Chance", _) => STAT_CRIT,
                ("Critical Strike Damage", _) => STAT_CRIT_DMG,
                ("Lethality", _) => STAT_LETHALITY,
                ("Armor Penetration", _) => STAT_ARMOR_PEN,
                ("Magic Penetration", false) => STAT_MAGIC_PEN,
                ("Magic Penetration", true) => STAT_MAGIC_PEN_PCT,
                ("Ability Haste", _) => STAT_HASTE,
                ("Health", _) => STAT_HP,
                ("Armor", _) => STAT_ARMOR,
                ("Magic Resist", _) => STAT_MR,
                ("Life Steal", _) => STAT_LIFE_STEAL,
                ("Omnivamp", _) => STAT_OMNIVAMP,
                ("Heal and Shield Power", _) => STAT_HEAL_SHIELD,
                ("Move Speed", false) => STAT_MS,
                ("Move Speed", true) => STAT_MS_PCT,
                ("Mana", _) => STAT_MANA,
                ("Tenacity", _) => STAT_TENACITY,
                _ => continue,
            };
            out[k] += v;
        }
        return out;
    }
    let stat = |k: &str| stats_obj.and_then(|s| s.get(k)).and_then(|v| v.as_f64()).unwrap_or(0.0);
    out[STAT_AD] = stat("FlatPhysicalDamageMod");
    out[STAT_AP] = stat("FlatMagicDamageMod");
    out[STAT_AS] = stat("PercentAttackSpeedMod");
    out[STAT_CRIT] = stat("FlatCritChanceMod");
    out[STAT_HP] = stat("FlatHPPoolMod");
    out[STAT_ARMOR] = stat("FlatArmorMod");
    out[STAT_MR] = stat("FlatSpellBlockMod");
    out[STAT_LIFE_STEAL] = stat("PercentLifeStealMod");
    out[STAT_MS] = stat("FlatMovementSpeedMod");
    out[STAT_MS_PCT] = stat("PercentMovementSpeedMod");
    out[STAT_MANA] = stat("FlatMPPoolMod");
    out
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut inside = false;
    for c in s.chars() {
        match c {
            '<' => inside = true,
            '>' => inside = false,
            _ if !inside => out.push(c),
            _ => {}
        }
    }
    out
}

/// Effect flags from an item's tags and description text.
pub fn parse_item_effects(tags: &[String], desc: &str, anti_heal: bool) -> u16 {
    let has = |t: &str| tags.iter().any(|x| x == t);
    let text = strip_tags(desc).to_lowercase();
    let mut e = 0;
    if has("OnHit") || text.contains("on-hit") {
        e |= EFFECT_ON_HIT;
    }
    if has("Active") {
        e |= EFFECT_ACTIVE;
    }
    if has("Aura") {
        e |= EFFECT_AURA;
    }
    if has("Slow") || text.contains("slows") {
        e |= EFFECT_SLOW;
    }
    if anti_heal || text.contains("wounds") || text.contains("grievous") {
        e |= EFFECT_WOUNDS;
    }
    if text.contains("shield") {
        e |= EFFECT_SHIELD;
    }
    if text.contains("max health") || text.contains("maximum health") || text.contains("current health") || text.contains("missing health") || text.contains("bonus health") {
        e |= EFFECT_HEALTH_DAMAGE;
    }
    if text.contains("spellblade") {
        e |= EFFECT_SPELLBLADE;
    }
    e
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ChampionInfo {
    pub attack: f64,
    pub defense: f64,
    pub magic: f64,
    pub difficulty: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChampionMeta {
    pub id: i64,
    pub key: String,
    pub name: String,
    pub tags: Vec<String>,
    pub info: ChampionInfo,
    pub attack_range: f64,
    /// Base stats and growth (1.3.0).
    pub base: ChampionBase,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rune {
    pub id: i64,
    pub key: String,
    pub name: String,
    pub icon: String,
    #[serde(default)]
    pub short_desc: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuneSlot {
    pub runes: Vec<Rune>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuneTree {
    pub id: i64,
    pub key: String,
    pub name: String,
    pub icon: String,
    pub slots: Vec<RuneSlot>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Summoner {
    pub name: String,
    pub image: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Spell {
    pub key: String,
    pub name: String,
    pub image: String,
}

#[derive(Clone, Debug)]
pub struct StaticData {
    pub version: String,
    pub patch: String,
    pub previous_patch: String,
    /// By id, ascending (JS Map built from Object.entries of integer keys).
    pub items: BTreeMap<i64, ItemMeta>,
    /// In champion.json's order (Object.values of non-index keys: insertion order).
    pub champions: indexmap::IndexMap<i64, ChampionMeta>,
    pub champions_by_key: HashMap<String, i64>,
    pub runes: Vec<RuneTree>,
    pub rune_ids: HashSet<i64>,
    pub summoners: indexmap::IndexMap<i64, Summoner>,
    pub versions: Option<Vec<String>>,
}

pub type Static = Arc<StaticData>;

impl StaticData {
    pub fn item(&self, id: i64) -> Option<&ItemMeta> {
        self.items.get(&id)
    }
    pub fn champion(&self, id: i64) -> Option<&ChampionMeta> {
        self.champions.get(&id)
    }
    pub fn champion_by_key(&self, key: &str) -> Option<&ChampionMeta> {
        self.champions_by_key.get(key).and_then(|id| self.champions.get(id))
    }
}

/// "16.19.1" or "16.19.712.4471" -> "16.19"
pub fn to_patch(version: &str) -> String {
    let mut it = version.split('.');
    let major = it.next().unwrap_or("undefined");
    let minor = it.next().unwrap_or("undefined");
    format!("{major}.{minor}")
}

fn patch_parts(p: &str) -> (f64, f64) {
    let mut it = p.split('.').map(|x| if x.is_empty() { 0.0 } else { x.parse::<f64>().unwrap_or(f64::NAN) });
    (it.next().unwrap_or(f64::NAN), it.next().unwrap_or(f64::NAN))
}

/// `a1 - b1 || a2 - b2` (as a number: NaN for unparseable patches, which compares false both ways).
pub fn compare_patch_num(a: &str, b: &str) -> f64 {
    let (a1, a2) = patch_parts(a);
    let (b1, b2) = patch_parts(b);
    let d = a1 - b1;
    if d != 0.0 && !d.is_nan() {
        d
    } else {
        a2 - b2
    }
}

/// The sign of compare_patch_num as an i32-like number for `> 0` / `< 0` tests (NaN → 0 but both tests false).
pub fn compare_patch(a: &str, b: &str) -> f64 {
    compare_patch_num(a, b)
}

/// A sort comparator from compare_patch (NaN treated as equal, as Array.prototype.sort does).
pub fn patch_ordering(a: &str, b: &str) -> std::cmp::Ordering {
    let d = compare_patch_num(a, b);
    if d < 0.0 {
        std::cmp::Ordering::Less
    } else if d > 0.0 {
        std::cmp::Ordering::Greater
    } else {
        std::cmp::Ordering::Equal
    }
}

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build()?)
}

async fn fetch_json(url: &str) -> Result<Value> {
    let res = http()?.get(url).send().await?;
    if !res.status().is_success() {
        return Err(anyhow!("Data Dragon {} for {}", res.status().as_u16(), url));
    }
    Ok(res.json().await?)
}

async fn cached(dir: &Path, version: &str, file: &str, url: &str) -> Result<Value> {
    let target = dir.join(version).join(file);
    if let Ok(t) = std::fs::read_to_string(&target) {
        if let Ok(v) = serde_json::from_str(&t) {
            return Ok(v);
        }
    }
    let data = fetch_json(url).await?;
    if let Some(p) = target.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&target, js::stringify(&data))?;
    Ok(data)
}

/// The newest Data Dragon version and its files (cached on disk). `known` skips asking for the version list again.
pub async fn load_static_data(known: Option<&[String]>) -> Result<StaticData> {
    let dir = config().ddragon_cache_dir;
    let versions: Vec<String> = match known.filter(|k| !k.is_empty()) {
        Some(k) => k.to_vec(),
        None => match fetch_json(&format!("{CDN}/api/versions.json")).await {
            Ok(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            _ => {
                let mut dirs: Vec<String> = std::fs::read_dir(&dir)
                    .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
                    .unwrap_or_default();
                if dirs.is_empty() {
                    return Err(anyhow!("Data Dragon unreachable and no cache present"));
                }
                dirs.sort_by(|a, b| patch_ordering(b, a));
                dirs
            }
        },
    };
    let version = versions[0].clone();
    let patch = to_patch(&version);
    let previous_patch = versions.iter().map(|v| to_patch(v)).find(|p| *p != patch).unwrap_or_else(|| patch.clone());
    let base = format!("{CDN}/cdn/{version}/data/en_US");
    let raw = RawStaticData {
        item: cached(&dir, &version, "item.json", &format!("{base}/item.json")).await?,
        champion: cached(&dir, &version, "champion.json", &format!("{base}/champion.json")).await?,
        runes_reforged: cached(&dir, &version, "runesReforged.json", &format!("{base}/runesReforged.json")).await?,
        summoner: cached(&dir, &version, "summoner.json", &format!("{base}/summoner.json")).await?,
    };
    let mut data = build_static_data(&version, &previous_patch, &raw)?;
    data.versions = Some(versions.into_iter().take(40).collect());
    Ok(data)
}

/// The Data Dragon files the app reads for one version.
pub struct RawStaticData {
    pub item: Value,
    pub champion: Value,
    pub runes_reforged: Value,
    pub summoner: Value,
}

fn passives(desc: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = desc;
    while let Some(i) = rest.find("<passive>") {
        let after = &rest[i + 9..];
        // [^<]+ then </passive>
        let end = after.find('<').unwrap_or(after.len());
        if end > 0 && after[end..].starts_with("</passive>") {
            let name = after[..end].trim().to_string();
            if !out.contains(&name) {
                out.push(name);
            }
            rest = &after[end + 10..];
        } else {
            rest = after;
        }
    }
    out
}

fn anti_heal(desc: &str) -> bool {
    // /<passive>\s*Grievous Wounds\s*<\/passive>/
    let mut rest = desc;
    while let Some(i) = rest.find("<passive>") {
        let after = &rest[i + 9..];
        let t = after.trim_start();
        if let Some(t2) = t.strip_prefix("Grievous Wounds") {
            if t2.trim_start().starts_with("</passive>") {
                return true;
            }
        }
        rest = after;
    }
    false
}

/// StaticData from raw Data Dragon files: the live loader above, and the tests' committed snapshot.
pub fn build_static_data(
    version: &str,
    previous_patch: &str,
    raw: &RawStaticData,
) -> Result<StaticData> {
    let patch = to_patch(version);
    let mut items = BTreeMap::new();
    let item_data = raw.item.get("data").and_then(|d| d.as_object()).ok_or_else(|| anyhow!("item.json has no data"))?;
    for (id, it) in js::entries(item_data) {
        let nid: i64 = id.parse().unwrap_or(0);
        let strs = |k: &str| -> Vec<String> {
            it.get(k).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
        };
        let nums = |k: &str| -> Vec<i64> {
            it.get(k).and_then(|v| v.as_array()).map(|a| a.iter().map(|x| js::to_number(x) as i64).collect()).unwrap_or_default()
        };
        let desc = it.get("description").and_then(|d| d.as_str()).unwrap_or("");
        let stat = |k: &str| it.get("stats").and_then(|s| s.get(k)).and_then(|v| v.as_f64()).unwrap_or(0.0);
        items.insert(
            nid,
            ItemMeta {
                id: nid,
                name: it.get("name").and_then(|v| v.as_str()).unwrap_or("").into(),
                tags: strs("tags"),
                into: nums("into"),
                from: nums("from"),
                special_recipe: it.get("specialRecipe").filter(|v| js::truthy(v)).map(|v| js::to_number(v) as i64),
                depth: it.get("depth").and_then(|v| v.as_i64()).unwrap_or(1),
                gold_total: it.get("gold").and_then(|g| g.get("total")).and_then(|v| v.as_f64()).unwrap_or(0.0),
                purchasable: it.get("gold").and_then(|g| g.get("purchasable")).and_then(|v| v.as_bool()).unwrap_or(false),
                on_summoners_rift: it.get("maps").and_then(|m| m.get("11")).and_then(|v| v.as_bool()) == Some(true),
                in_store: it.get("inStore").and_then(|v| v.as_bool()) != Some(false),
                passives: passives(desc),
                armor: stat("FlatArmorMod"),
                magic_resist: stat("FlatSpellBlockMod"),
                anti_heal: anti_heal(desc),
                stats: parse_item_stats(desc, it.get("stats")),
                effects: parse_item_effects(&strs("tags"), desc, anti_heal(desc)),
            },
        );
    }

    let mut champions = indexmap::IndexMap::new();
    let mut champions_by_key = HashMap::new();
    if let Some(data) = raw.champion.get("data").and_then(|d| d.as_object()) {
        for (_, c) in js::entries(data) {
            let id = js::to_number(c.get("key").unwrap_or(&Value::Null)) as i64;
            let meta = ChampionMeta {
                id,
                key: c.get("id").and_then(|v| v.as_str()).unwrap_or("").into(),
                name: c.get("name").and_then(|v| v.as_str()).unwrap_or("").into(),
                tags: c
                    .get("tags")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_default(),
                info: c.get("info").and_then(|i| serde_json::from_value(i.clone()).ok()).unwrap_or(ChampionInfo {
                    attack: 0.0,
                    defense: 0.0,
                    magic: 0.0,
                    difficulty: 0.0,
                }),
                attack_range: c.get("stats").and_then(|s| s.get("attackrange")).and_then(|v| v.as_f64()).unwrap_or(0.0),
                base: {
                    let st = |k: &str| c.get("stats").and_then(|s| s.get(k)).and_then(|v| v.as_f64()).unwrap_or(0.0);
                    ChampionBase {
                        hp: st("hp"),
                        hp_per_level: st("hpperlevel"),
                        armor: st("armor"),
                        armor_per_level: st("armorperlevel"),
                        mr: st("spellblock"),
                        mr_per_level: st("spellblockperlevel"),
                        ad: st("attackdamage"),
                        ad_per_level: st("attackdamageperlevel"),
                        attack_speed: st("attackspeed"),
                        attack_speed_per_level: st("attackspeedperlevel"),
                        move_speed: st("movespeed"),
                    }
                },
            };
            champions_by_key.insert(meta.key.clone(), id);
            champions_by_key.insert(meta.name.clone(), id);
            champions.insert(id, meta);
        }
    }

    let runes: Vec<RuneTree> = serde_json::from_value(raw.runes_reforged.clone())?;
    let mut rune_ids = HashSet::new();
    for tree in &runes {
        rune_ids.insert(tree.id);
        for slot in &tree.slots {
            for r in &slot.runes {
                rune_ids.insert(r.id);
            }
        }
    }

    let mut summoners = indexmap::IndexMap::new();
    if let Some(data) = raw.summoner.get("data").and_then(|d| d.as_object()) {
        for (_, s) in js::entries(data) {
            summoners.insert(
                js::to_number(s.get("key").unwrap_or(&Value::Null)) as i64,
                Summoner {
                    name: s.get("name").and_then(|v| v.as_str()).unwrap_or("").into(),
                    image: s.get("image").and_then(|i| i.get("full")).and_then(|v| v.as_str()).unwrap_or("").into(),
                },
            );
        }
    }

    Ok(StaticData {
        version: version.into(),
        patch,
        previous_patch: previous_patch.into(),
        items,
        champions,
        champions_by_key,
        runes,
        rune_ids,
        summoners,
        versions: None,
    })
}

/// A champion's Q/W/E/R (names and icons), from `champion/<key>.json` of `version` (cached on disk).
pub async fn load_champion_spells(version: &str, champion_key: &str) -> Result<Vec<Spell>> {
    if champion_key.is_empty() || !champion_key.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(anyhow!("not a champion key: {champion_key:?}"));
    }
    let dir = config().ddragon_cache_dir;
    let url = format!("{CDN}/cdn/{version}/data/en_US/champion/{champion_key}.json");
    let raw = cached(&dir, version, &format!("champion-{champion_key}.json"), &url).await?;
    spells_of(&raw, champion_key)
}

/// The spells of `champion/<key>.json` as Data Dragon serves it.
pub fn spells_of(raw: &Value, champion_key: &str) -> Result<Vec<Spell>> {
    let keys = ["Q", "W", "E", "R"];
    Ok(raw
        .get("data")
        .and_then(|d| d.get(champion_key))
        .and_then(|c| c.get("spells"))
        .and_then(|s| s.as_array())
        .ok_or_else(|| anyhow!("{champion_key}.json has no spells"))?
        .iter()
        .enumerate()
        .map(|(i, s)| Spell {
            key: keys.get(i).copied().unwrap_or("?").into(),
            name: s.get("name").and_then(|v| v.as_str()).unwrap_or("").into(),
            image: s.get("image").and_then(|i| i.get("full")).and_then(|v| v.as_str()).unwrap_or("").into(),
        })
        .collect())
}

#[cfg(test)]
mod stat_tests {
    use super::*;

    /// 1.3.0: every shop stat comes from the description's stat lines (Data Dragon 16.19.1 snapshot), and champions
    /// carry their base stats.
    #[test]
    fn item_stats_effects_and_champion_bases() {
        let d = crate::testkit::load_test_static_data();
        let s = |id: i64| d.item(id).unwrap().stats;
        // Lord Dominik's: 35 AD, 35% armor pen, 25% crit (armor pen isn't in Data Dragon's `stats` object).
        assert_eq!((s(3036)[STAT_AD], s(3036)[STAT_ARMOR_PEN], s(3036)[STAT_CRIT]), (35.0, 0.35, 0.25));
        // Void Staff: 40% magic pen; Sorcerer's Shoes: 12 flat magic pen and 45 move speed.
        assert_eq!((s(3135)[STAT_AP], s(3135)[STAT_MAGIC_PEN_PCT]), (95.0, 0.4));
        assert_eq!((s(3020)[STAT_MAGIC_PEN], s(3020)[STAT_MS]), (12.0, 45.0));
        // Infinity Edge's crit damage, Black Cleaver's health and haste.
        assert_eq!(s(3031)[STAT_CRIT_DMG], 0.3);
        assert_eq!((s(3071)[STAT_HP], s(3071)[STAT_HASTE]), (400.0, 20.0));
        let e = |id: i64| d.item(id).unwrap().effects;
        assert!(e(3153) & EFFECT_ON_HIT != 0 && e(3153) & EFFECT_HEALTH_DAMAGE != 0, "Blade of the Ruined King");
        assert!(e(3033) & EFFECT_WOUNDS != 0, "Mortal Reminder");
        let irelia = d.champion(39).unwrap().base;
        assert!(irelia.hp > 500.0 && irelia.armor > 20.0 && irelia.attack_speed > 0.5, "{irelia:?}");
        // Growth: level 1 is the base, level 18 the base + 17 levels of growth.
        assert_eq!(irelia.armor_at(1.0), irelia.armor);
        assert!((irelia.armor_at(18.0) - (irelia.armor + irelia.armor_per_level * 17.0)).abs() < 1e-9);
    }
}
