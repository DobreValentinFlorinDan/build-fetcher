//! Port of `server/scrapeScope.ts`: which regions, tiers, pilots and sources are fetched (scrape.config.json).

use crate::app_config;
use crate::riot::routing::{is_platform, is_tier, DIVISIONS};
use crate::util;
use anyhow::{bail, Result};
use serde_json::{json, Map, Value};
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct ManualHotfix {
    pub patch: String,
    pub date: String,
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScrapeScope {
    pub platforms: Vec<String>,
    pub tiers: Vec<String>,
    pub divisions: Vec<String>,
    pub max_league_pages_per_division: f64,
    pub otp_min_mastery_points: f64,
    pub main_min_mastery_points: f64,
    pub mastery_top_count: f64,
    pub pilot_classes: Vec<String>,
    pub harvest_irelias: bool,
    pub history_window: String,
    pub match_ids_per_player: f64,
    pub max_matches_per_pilot: f64,
    pub build_sources: Vec<String>,
    pub lookup_harvested_ranks: bool,
    pub own_games: bool,
    pub hotfixes: Vec<ManualHotfix>,
    pub draft_sources: Vec<String>,
    /// Kept in the order given (JS object order for non-index keys).
    pub region_weights: Vec<(String, f64)>,
    pub own_region_boost: f64,
    pub patch_weights: Vec<(String, f64)>,
}

impl ScrapeScope {
    pub fn region_weight(&self, platform: &str) -> Option<f64> {
        self.region_weights.iter().find(|(k, _)| k == platform).map(|(_, w)| *w)
    }
    pub fn patch_weight(&self, patch: &str) -> Option<f64> {
        self.patch_weights.iter().find(|(k, _)| k == patch).map(|(_, w)| *w)
    }

    /// The scope as JSON, in the TS object's shape (settings and the evaluation's key read it).
    pub fn to_json(&self) -> Value {
        let weights = |w: &Vec<(String, f64)>| {
            let mut m = Map::new();
            for (k, v) in w {
                m.insert(k.clone(), crate::js::num(*v));
            }
            Value::Object(m)
        };
        json!({
            "platforms": self.platforms, "tiers": self.tiers, "divisions": self.divisions,
            "maxLeaguePagesPerDivision": crate::js::num(self.max_league_pages_per_division),
            "otpMinMasteryPoints": crate::js::num(self.otp_min_mastery_points),
            "mainMinMasteryPoints": crate::js::num(self.main_min_mastery_points),
            "masteryTopCount": crate::js::num(self.mastery_top_count),
            "pilotClasses": self.pilot_classes, "harvestIrelias": self.harvest_irelias,
            "historyWindow": self.history_window,
            "matchIdsPerPlayer": crate::js::num(self.match_ids_per_player),
            "maxMatchesPerPilot": crate::js::num(self.max_matches_per_pilot),
            "buildSources": self.build_sources, "lookupHarvestedRanks": self.lookup_harvested_ranks,
            "ownGames": self.own_games,
            "hotfixes": self.hotfixes.iter().map(|h| {
                let mut m = Map::new();
                m.insert("patch".into(), json!(h.patch));
                m.insert("date".into(), json!(h.date));
                if let Some(n) = &h.note { m.insert("note".into(), json!(n)); }
                Value::Object(m)
            }).collect::<Vec<_>>(),
            "draftSources": self.draft_sources,
            "regionWeights": weights(&self.region_weights),
            "ownRegionBoost": crate::js::num(self.own_region_boost),
            "patchWeights": weights(&self.patch_weights),
        })
    }
}

pub fn default_scope() -> ScrapeScope {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    ScrapeScope {
        platforms: s(&["KR", "EUW1", "EUN1", "NA1", "JP1"]),
        tiers: s(&["CHALLENGER", "GRANDMASTER", "MASTER"]),
        divisions: s(&["I", "II", "III", "IV"]),
        max_league_pages_per_division: 1.0,
        otp_min_mastery_points: 150_000.0,
        main_min_mastery_points: 100_000.0,
        mastery_top_count: 3.0,
        pilot_classes: s(&["otp", "main"]),
        harvest_irelias: true,
        history_window: "patch".into(),
        match_ids_per_player: 20.0,
        max_matches_per_pilot: 300.0,
        build_sources: s(&["otp"]),
        lookup_harvested_ranks: true,
        own_games: true,
        hotfixes: vec![],
        draft_sources: s(&["otp", "main", "harvested"]),
        region_weights: vec![],
        own_region_boost: 1.5,
        patch_weights: vec![],
    }
}

fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => crate::js::num_to_string(n.as_f64().unwrap_or(f64::NAN)),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".into(),
        other => crate::js::stringify(other),
    }
}

fn dedupe(v: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for x in v {
        if !out.contains(&x) {
            out.push(x);
        }
    }
    out
}

/// Validates a partial scope (from scrape.config.json, env or a request) and fills defaults.
pub fn parse_scope(input: &Map<String, Value>, base: &ScrapeScope) -> Result<ScrapeScope> {
    let strings = |k: &str, upper: bool| -> Option<Vec<String>> {
        input.get(k).and_then(|v| v.as_array()).map(|a| {
            a.iter().map(|x| if upper { js_string(x).to_uppercase() } else { js_string(x).to_lowercase() }).collect()
        })
    };
    let platforms = strings("platforms", true).unwrap_or_else(|| base.platforms.clone());
    let tiers = strings("tiers", true).unwrap_or_else(|| base.tiers.clone());
    let divisions = strings("divisions", true).unwrap_or_else(|| base.divisions.clone());
    let classes = strings("pilotClasses", false).unwrap_or_else(|| base.pilot_classes.clone());
    let sources = strings("buildSources", false).unwrap_or_else(|| base.build_sources.clone());
    let draft_sources = strings("draftSources", false).unwrap_or_else(|| base.draft_sources.clone());

    let mut bad: Vec<String> = Vec::new();
    bad.extend(platforms.iter().filter(|p| !is_platform(p)).cloned());
    bad.extend(tiers.iter().filter(|t| !is_tier(t)).cloned());
    bad.extend(divisions.iter().filter(|d| !DIVISIONS.contains(&d.as_str())).cloned());
    bad.extend(classes.iter().filter(|c| *c != "otp" && *c != "main").map(|c| format!("pilot class {c}")));
    bad.extend(
        sources.iter().chain(draft_sources.iter()).filter(|s| !["otp", "main", "harvested"].contains(&s.as_str())).map(|s| format!("build source {s}")),
    );
    if !bad.is_empty() {
        bail!("Invalid scrape scope: {}", bad.join(", "));
    }
    if platforms.is_empty() || tiers.is_empty() {
        bail!("Scrape scope needs at least one platform and one tier");
    }
    if sources.is_empty() || draft_sources.is_empty() {
        bail!("Scrape scope needs at least one build source and one draft source");
    }

    let num = |k: &str, d: f64| -> f64 {
        input.get(k).map(crate::js::to_number).filter(|n| n.is_finite() && *n > 0.0).unwrap_or(d)
    };
    let boolean = |k: &str, d: bool| input.get(k).and_then(|v| v.as_bool()).unwrap_or(d);
    let hotfixes = match input.get("hotfixes").and_then(|v| v.as_array()) {
        Some(a) => a
            .iter()
            .filter_map(|h| {
                let patch = h.get("patch")?.as_str()?;
                let date = h.get("date")?.as_str()?;
                crate::js::date_parse(date)?;
                let note = h.get("note").filter(|n| crate::js::truthy(n)).map(js_string);
                Some(ManualHotfix { patch: patch.into(), date: date.into(), note })
            })
            .collect(),
        None => base.hotfixes.clone(),
    };
    let weights = |k: &str, d: &Vec<(String, f64)>, upper: bool| -> Vec<(String, f64)> {
        match input.get(k) {
            Some(Value::Object(m)) => crate::js::entries(m)
                .into_iter()
                .filter_map(|(k, w)| {
                    let n = crate::js::to_number(w);
                    (n.is_finite() && n >= 0.0).then(|| (if upper { k.to_uppercase() } else { k.clone() }, n))
                })
                .collect(),
            _ => d.clone(),
        }
    };
    let history_window = match input.get("historyWindow").and_then(|v| v.as_str()) {
        Some(w @ ("newest" | "patch")) => w.to_string(),
        _ => base.history_window.clone(),
    };
    Ok(ScrapeScope {
        platforms: dedupe(platforms),
        tiers: dedupe(tiers),
        divisions,
        max_league_pages_per_division: num("maxLeaguePagesPerDivision", base.max_league_pages_per_division),
        otp_min_mastery_points: num("otpMinMasteryPoints", base.otp_min_mastery_points),
        main_min_mastery_points: num("mainMinMasteryPoints", base.main_min_mastery_points),
        mastery_top_count: crate::js::round(num("masteryTopCount", base.mastery_top_count)).min(10.0),
        pilot_classes: dedupe(classes),
        harvest_irelias: boolean("harvestIrelias", base.harvest_irelias),
        history_window,
        match_ids_per_player: num("matchIdsPerPlayer", base.match_ids_per_player).min(100.0),
        max_matches_per_pilot: num("maxMatchesPerPilot", base.max_matches_per_pilot).min(1000.0),
        build_sources: dedupe(sources),
        lookup_harvested_ranks: boolean("lookupHarvestedRanks", base.lookup_harvested_ranks),
        own_games: boolean("ownGames", base.own_games),
        hotfixes,
        draft_sources: dedupe(draft_sources),
        region_weights: dedupe_weights(weights("regionWeights", &base.region_weights, true)),
        own_region_boost: num("ownRegionBoost", base.own_region_boost),
        patch_weights: dedupe_weights(weights("patchWeights", &base.patch_weights, false)),
    })
}

/// Object.fromEntries keeps the last value of a repeated key, at the first key's place.
fn dedupe_weights(v: Vec<(String, f64)>) -> Vec<(String, f64)> {
    let mut out: Vec<(String, f64)> = Vec::new();
    for (k, w) in v {
        if let Some(x) = out.iter_mut().find(|(k2, _)| *k2 == k) {
            x.1 = w;
        } else {
            out.push((k, w));
        }
    }
    out
}

fn list(raw: Option<String>) -> Option<Vec<Value>> {
    let v: Vec<Value> =
        raw?.split(',').map(|s| s.trim().to_uppercase()).filter(|s| !s.is_empty()).map(Value::String).collect();
    // `list()` returns [] for ",", which is truthy in JS.
    Some(v)
}

/// scrape.config.json (user copy first), then SCRAPE_PLATFORMS / SCRAPE_TIERS env overrides. Re-read on every call.
pub fn load_scope_from(file: &Path) -> Result<ScrapeScope> {
    let mut input: Map<String, Value> = std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    if let Some(p) = list(util::env("SCRAPE_PLATFORMS")) {
        input.insert("platforms".into(), Value::Array(p));
    }
    if let Some(t) = list(util::env("SCRAPE_TIERS")) {
        input.insert("tiers".into(), Value::Array(t));
    }
    parse_scope(&input, &default_scope())
}

pub fn load_scope() -> Result<ScrapeScope> {
    load_scope_from(&app_config::scope_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }
    #[test]
    fn scope_defaults_and_accepts_new_tiers() {
        let base = parse_scope(&Map::new(), &default_scope()).unwrap();
        assert_eq!(base.platforms, vec!["KR", "EUW1", "EUN1", "NA1", "JP1"]);
        assert_eq!(base.tiers, vec!["CHALLENGER", "GRANDMASTER", "MASTER"]);
        let wider =
            parse_scope(&obj(json!({"platforms": ["kr", "euw1"], "tiers": ["CHALLENGER", "DIAMOND", "emerald"]})), &default_scope())
                .unwrap();
        assert_eq!(wider.platforms, vec!["KR", "EUW1"]);
        assert_eq!(wider.tiers, vec!["CHALLENGER", "DIAMOND", "EMERALD"]);
        let e = parse_scope(&obj(json!({"tiers": ["WOOD"]})), &default_scope()).unwrap_err();
        assert!(e.to_string().contains("WOOD"));
        let e = parse_scope(&obj(json!({"platforms": []})), &default_scope()).unwrap_err();
        assert!(e.to_string().contains("at least one"));
    }
}
