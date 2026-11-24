//! The parts of `shared/types.ts` the backend builds as typed values (the rest is assembled as JSON where it is
//! sent). Field names serialize as the TS ones (camelCase), in the same order, so the dashboard reads them unchanged.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type Lane = &'static str;

/// "TOP", "middle", "MID" → the lane; jungle, bot, support, "" and UNKNOWN → None.
pub fn to_lane(position: Option<&str>) -> Option<&'static str> {
    let p = position.unwrap_or("").trim().to_uppercase();
    if p == "TOP" {
        Some("TOP")
    } else if p == "MIDDLE" || p == "MID" {
        Some("MIDDLE")
    } else {
        None
    }
}

pub const LANES: [&str; 2] = ["TOP", "MIDDLE"];

pub fn lane_label(lane: &str) -> &'static str {
    if lane == "TOP" {
        "Top"
    } else {
        "Mid"
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Opt<T> {
    pub value: T,
    pub games: f64,
    pub raw_games: f64,
    pub win_rate: f64,
    pub score: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct RunePage {
    pub primary_style: i64,
    pub sub_style: i64,
    pub perks: Vec<i64>,
    pub stat_perks: Vec<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SkillPlan {
    pub priority: Vec<String>,
    pub path: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SampleInfo {
    pub matchup_games: f64,
    pub total_games: f64,
    pub patches: Vec<String>,
    pub used_fallback: bool,
    pub lane_games: f64,
    pub other_lane_games: f64,
    pub used_lane_fallback: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ValidationReport {
    pub current_patch: String,
    pub fresh_games: f64,
    pub previous_patch_games: f64,
    pub stale_discarded: f64,
    pub invalid_discarded: f64,
    /// Insertion order (a JS object of non-index keys).
    pub invalid_reasons: indexmap::IndexMap<String, f64>,
    pub conflicts_rejected: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Situational {
    pub slot4: Vec<Opt<i64>>,
    pub slot5: Vec<Opt<i64>>,
    pub slot6: Vec<Opt<i64>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BuildRecommendation {
    pub patch: String,
    pub ddragon_version: String,
    pub enemy_champion_id: Option<i64>,
    pub lane: Option<String>,
    pub sample: SampleInfo,
    pub win_rate: Option<f64>,
    pub runes: Option<Opt<RunePage>>,
    pub summoners: Option<Opt<Vec<i64>>>,
    pub starting_items: Vec<Opt<Vec<i64>>>,
    pub core_build: Vec<Opt<Vec<i64>>>,
    pub boots: Vec<Opt<i64>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub boots_after: Option<Option<f64>>,
    pub situational: Situational,
    pub skills: Option<Opt<SkillPlan>>,
    pub validation: ValidationReport,
}

/// A JS `Record<number, …>` serialized as JSON: integer keys ascending.
pub type NumMap<V> = BTreeMap<i64, V>;

// ---- Draft-aware builds (server/model/draft.ts)

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PickStats {
    pub lift: f64,
    pub low: f64,
    pub high: f64,
    pub n_eff: f64,
    pub games: f64,
    pub pilots: f64,
    pub win_rate: f64,
    pub matchup_games: f64,
    /// Mean win chance just before the purchase (W̄), over the same weighted games as `win_rate`; null when unknown.
    #[serde(default)]
    pub start_win: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PickBaseline {
    pub value: String,
    pub lift: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PickAlternative {
    pub value: String,
    pub stats: PickStats,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BuildPick {
    pub key: String,
    pub label: String,
    pub value: String,
    pub stats: Option<PickStats>,
    pub reason: Option<String>,
    pub fallback: bool,
    pub changed: bool,
    pub baseline: Option<PickBaseline>,
    pub alternatives: Vec<PickAlternative>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ContextChip {
    pub key: String,
    pub label: String,
    pub value: String,
    pub z: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SwapPick {
    Keep,
    Swap { item: i64 },
    Sell,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SwapAlternative {
    pub item: Option<i64>,
    pub lift: f64,
    pub games: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SwapAdvice {
    pub held: i64,
    pub pick: SwapPick,
    pub lift: Option<f64>,
    pub low: Option<f64>,
    pub high: Option<f64>,
    pub n_eff: f64,
    pub games: f64,
    pub thin: bool,
    pub alternatives: Vec<SwapAlternative>,
    pub minute: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DraftSample {
    pub games: f64,
    pub n_eff: f64,
    pub pilots: f64,
    pub regions: indexmap::IndexMap<String, f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DraftRecommendation {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub estimates: Option<indexmap::IndexMap<String, indexmap::IndexMap<String, f64>>>,
    pub build: BuildRecommendation,
    pub picks: Vec<BuildPick>,
    pub context: Vec<ContextChip>,
    pub sample: DraftSample,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub swaps: Option<Vec<SwapAdvice>>,
}
