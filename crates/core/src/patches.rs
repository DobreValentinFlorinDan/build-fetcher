//! Port of `server/patches.ts`: the current patch per region, when it started there, and the hotfix builds.

use crate::ddragon::compare_patch;
use crate::scope::ManualHotfix;

#[derive(Clone, Debug, PartialEq)]
pub struct VersionRow {
    pub platform: String,
    pub game_version: String,
    pub patch: String,
    pub first_seen: i64,
    pub last_seen: i64,
    pub games: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Build {
    pub game_version: String,
    pub first_seen: i64,
    pub games: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RegionPatch {
    pub platform: String,
    pub patch: String,
    pub previous_patch: String,
    pub start: i64,
    pub start_known: bool,
    pub builds: Vec<Build>,
    pub from_data_dragon: bool,
}

/// A patch lasts two weeks; without a previous-patch game, the window reaches this far back from the oldest game seen.
pub const PATCH_CYCLE_MS: i64 = 14 * 24 * 3600_000;

#[derive(Clone, Debug)]
pub struct Fallback {
    pub patch: String,
    pub previous_patch: String,
}

pub fn region_patch(platform: &str, versions: &[VersionRow], fallback: &Fallback, now: i64) -> RegionPatch {
    let mine: Vec<&VersionRow> = versions.iter().filter(|v| v.platform == platform).collect();
    if mine.is_empty() {
        return RegionPatch {
            platform: platform.into(),
            patch: fallback.patch.clone(),
            previous_patch: fallback.previous_patch.clone(),
            start: now - PATCH_CYCLE_MS,
            start_known: false,
            builds: vec![],
            from_data_dragon: true,
        };
    }
    let mut patch = mine[0].patch.clone();
    for v in &mine[1..] {
        if compare_patch(&v.patch, &patch) > 0.0 {
            patch = v.patch.clone();
        }
    }
    let previous_patch = previous_of(&patch, fallback);
    let mut current: Vec<&VersionRow> = mine.iter().copied().filter(|v| v.patch == patch).collect();
    let older: Vec<&VersionRow> = mine.iter().copied().filter(|v| compare_patch(&v.patch, &patch) < 0.0).collect();
    let first_seen = current.iter().map(|v| v.first_seen).min().unwrap();
    let newest_older = older.iter().map(|v| v.last_seen).max();
    current.sort_by_key(|v| v.first_seen); // stable, as Array.prototype.sort
    RegionPatch {
        platform: platform.into(),
        patch,
        previous_patch,
        start: newest_older.unwrap_or(first_seen - PATCH_CYCLE_MS),
        start_known: newest_older.is_some(),
        builds: current
            .iter()
            .map(|v| Build { game_version: v.game_version.clone(), first_seen: v.first_seen, games: v.games })
            .collect(),
        from_data_dragon: false,
    }
}

/// The patch before `patch`: Data Dragon's pair when it names the same patch, else the previous minor number.
pub fn previous_of(patch: &str, fallback: &Fallback) -> String {
    if fallback.patch == patch {
        return fallback.previous_patch.clone();
    }
    let mut it = patch.split('.').map(|x| x.parse::<f64>().unwrap_or(f64::NAN));
    let major = it.next().unwrap_or(f64::NAN);
    let minor = it.next().unwrap_or(f64::NAN);
    if minor > 1.0 {
        return format!("{}.{}", crate::js::num_to_string(major), crate::js::num_to_string(minor - 1.0));
    }
    if compare_patch(&fallback.previous_patch, patch) < 0.0 {
        fallback.previous_patch.clone()
    } else {
        patch.to_string()
    }
}

/// Every platform that has games, plus `platforms` (scope), in first-seen order (a JS Map's order).
pub fn region_patches(versions: &[VersionRow], fallback: &Fallback, platforms: &[String], now: i64) -> Vec<(String, RegionPatch)> {
    let mut all: Vec<String> = Vec::new();
    for p in versions.iter().map(|v| v.platform.clone()).chain(platforms.iter().cloned()) {
        if !all.contains(&p) {
            all.push(p);
        }
    }
    all.into_iter().map(|p| { let r = region_patch(&p, versions, fallback, now); (p, r) }).collect()
}

/// Is a game on `patch` older than its platform's current patch?
pub fn is_stale(patch: &str, region_patch: &str) -> bool {
    compare_patch(patch, region_patch) < 0.0
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hotfix {
    pub patch: String,
    pub at: f64,
    pub source: &'static str,
    pub label: String,
}

/// Hotfix moments within `patch` on one platform: every build after the first one seen, plus the manual dates.
pub fn hotfixes(region: &RegionPatch, manual: &[ManualHotfix]) -> Vec<Hotfix> {
    let mut out: Vec<Hotfix> = region
        .builds
        .iter()
        .skip(1)
        .map(|b| Hotfix { patch: region.patch.clone(), at: b.first_seen as f64, source: "build", label: b.game_version.clone() })
        .collect();
    for h in manual.iter().filter(|h| h.patch == region.patch) {
        if let Some(at) = crate::js::date_parse(&h.date) {
            out.push(Hotfix {
                patch: h.patch.clone(),
                at,
                source: "manual",
                label: h.note.clone().unwrap_or_else(|| "manual hotfix".into()),
            });
        }
    }
    out.sort_by(|a, b| a.at.partial_cmp(&b.at).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// RegionPatch as JSON (server/patches.ts key order).
pub fn region_json(r: &RegionPatch) -> serde_json::Value {
    serde_json::json!({
        "platform": r.platform, "patch": r.patch, "previousPatch": r.previous_patch, "start": r.start, "startKnown": r.start_known,
        "builds": r.builds.iter().map(|b| serde_json::json!({ "gameVersion": b.game_version, "firstSeen": b.first_seen, "games": b.games })).collect::<Vec<_>>(),
        "fromDataDragon": r.from_data_dragon,
    })
}

pub fn hotfix_json(h: &Hotfix) -> serde_json::Value {
    serde_json::json!({ "patch": h.patch, "at": crate::js::num(h.at), "source": h.source, "label": h.label })
}
