//! Port of `server/model/evaluate.ts`: B6 "Evaluate builds". Grouped cross-validation by pilot, the bandwidth/prior
//! grid, the pilot bootstrap, every gate. The comparisons go over held-out games on a rayon pool (the "team"); each
//! game's results are applied in game order, so the report is the same as on one thread, and the same as 0.22.0's.

use super::context::{standardise, Contexts, N_ITEM};
use super::dataset::{game_timeline, moments_at, prepare_games, state_rows_by, PrepareOptions, PreparedGame, ScoredGame};
use super::draft::{advise_swap, build_draft_model, item_query_of, observed, recommend_draft, similarities, weigh_games, DraftCache, DraftExtra, DraftModel, DraftQuery, DraftSettings, Weighted};
use super::estimate::{estimate_decision, EstimateOptions, Observation, Query, Sims, Z, ESTIMATE_DEFAULTS};
use super::expected::{cross_fitted, expected_defaults, expected_of, fit_expected, fnv, Penalty};
use super::groups::{assign_draft_groups, auto_groups, GroupingStats};
use super::live::{NextStability, STABILITY_DEFAULTS};
use super::logistic::{calibration, js_max, js_min, score_probabilities, CalibrationRow};
use super::profiles::ChampionProfiles;
use super::state::{train_state_model, win_probability_of, StateModel, N_FEATURES};
use super::swaps::{KEEP as SWAP_KEEP, SELL as SWAP_SELL};
use super::window::{ece, StateSample};
use super::weights::{weight_defaults, Memory, PatchAger, RegionPatchFn, WeightOptions, MEMORY_PATCHES};
use crate::db::{IreliaGameRow, ParticipantLite, RecLogRow};
use crate::ddragon::StaticData;
use crate::engine::exclusivity::{is_purchasable_item, ExclusivityIndex};
use crate::engine::recommend::{recommend, EngineSettings, ValidatedRows};
use crate::engine::validator::is_completed_item;
use crate::js;
use crate::types::{BuildRecommendation, SwapPick};
use anyhow::Result;
use parking_lot::Mutex;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

// ------------------------------------------------------------------------------------------------- options

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryOptions {
    pub patches: f64,
    pub decays: Vec<f64>,
    pub min_older_games: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TuningOptions {
    pub pilot_strengths: Vec<f64>,
    pub recency_days: Vec<f64>,
}

#[derive(Clone, Debug)]
pub struct EvalOptions {
    pub folds: usize,
    pub lambdas: Vec<f64>,
    pub bandwidths: Vec<f64>,
    pub ks: Vec<f64>,
    pub bootstrap: usize,
    pub seed: f64,
    pub build_sources: Vec<String>,
    pub engine: EngineSettings,
    pub weights: WeightOptions,
    pub live: bool,
    pub min_games: usize,
    pub max_grid_games: usize,
    pub max_live_games: usize,
    pub memory: Option<MemoryOptions>,
    pub scoring: bool,
    pub tuning: Option<TuningOptions>,
    pub swaps: bool,
    pub update: Option<EvalUpdate>,
    pub grouping: Option<usize>,
    /// 2.2.0 candidate: also fit the game-state model on one frame per 5-minute interval of each game, and keep the
    /// sample with the lower held-out log loss.
    pub state_sample: bool,
    /// 2.2.0 candidate: also measure the builds with an item's ΔW ending when it is sold or consumed into an upgrade,
    /// and keep that only when the lift and its 95 % interval both improve.
    pub effective_window: bool,
}

/// What a quick update takes from its base.
#[derive(Clone, Debug)]
pub struct EvalUpdate {
    pub chosen: Chosen,
    pub lambda: f64,
    pub decay: Option<f64>,
    pub tuning: Option<TuningChoice>,
    pub salt: String,
    pub known: Vec<f64>,
    pub prints: Vec<f64>,
    pub penalties: Option<Vec<Option<Penalty>>>,
    pub grid_stride: usize,
    /// The base's state-model sample and ΔW definition (2.2.0): an update measures with them, as with λ.
    pub sample: StateSample,
    pub window: bool,
}

pub fn eval_defaults(engine: EngineSettings) -> EvalOptions {
    EvalOptions {
        folds: 5,
        lambdas: vec![0.3, 1.0, 3.0, 10.0],
        bandwidths: vec![0.5, 1.0, f64::INFINITY],
        ks: vec![30.0, 100.0, 300.0],
        bootstrap: 1000,
        seed: 1.0,
        build_sources: vec!["otp".into()],
        engine,
        weights: weight_defaults(),
        live: true,
        min_games: 200,
        max_grid_games: 10_000,
        max_live_games: 500,
        memory: Some(MemoryOptions { patches: MEMORY_PATCHES, decays: vec![0.5, 0.75, 0.9], min_older_games: 200.0 }),
        scoring: true,
        tuning: Some(TuningOptions { pilot_strengths: vec![0.0, 0.5, 2.0], recency_days: vec![7.0, 21.0] }),
        swaps: true,
        update: None,
        grouping: None,
        state_sample: false,
        effective_window: false,
    }
}

/// How many progress steps an evaluation with these options reports.
pub fn eval_steps(o: &EvalOptions) -> usize {
    let base = if o.update.is_some() {
        3 + if o.memory.is_some() { 2 } else { 0 } + if o.scoring { 2 } else { 0 } + if o.tuning.is_some() { 1 } else { 0 }
    } else {
        o.lambdas.len()
            + o.folds
            + 2
            + if o.memory.is_some() { o.folds + 2 } else { 0 }
            + if o.scoring { 2 } else { 0 }
            + if o.tuning.is_some() { o.folds + 1 } else { 0 }
            + if o.live { 1 } else { 0 }
    };
    base + if o.swaps { 1 } else { 0 }
}

pub const EVAL_DECISIONS: [&str; 9] = ["keystone", "secondaryTree", "summoners", "start", "maxFirst", "core1", "core2", "core3", "boots"];
const SCORED_ITEMS: [&str; 4] = ["core1", "core2", "core3", "boots"];
const PREGAME: [&str; 5] = ["keystone", "secondaryTree", "summoners", "start", "maxFirst"];
const BRIER_KEYS: [&str; 5] = ["keystone", "core1", "core2", "core3", "boots"];
const SWAP_KEYS: [&str; 1] = ["swap"];
const SLOTS: usize = 7;

fn set_of(keys: &[&'static str]) -> HashSet<&'static str> {
    keys.iter().copied().collect()
}

// ------------------------------------------------------------------------------------------------- report

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LambdaScore {
    pub lambda: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub brier: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub log_loss: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateReport {
    pub lambda: f64,
    pub brier: f64,
    pub log_loss: f64,
    pub base_brier: f64,
    pub base_log_loss: f64,
    pub frames: f64,
    pub by_lambda: Vec<LambdaScore>,
    pub calibration: Vec<CalibrationJson>,
    /// The two samples measured (2.2.0), when the candidate ran.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sample: Option<SampleReport>,
    /// Expected calibration error and reliability tables (20 bins) of the chosen state model on the held-out frames.
    #[serde(default)]
    pub reliability: Option<StateReliability>,
}

/// One state-model sample's best penalty and its held-out scores.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SampleScore {
    pub lambda: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub brier: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub log_loss: f64,
    /// Expected calibration error, 20 bins.
    #[serde(deserialize_with = "js::de_num")]
    pub ece: f64,
    /// Training rows per fold (mean).
    pub rows: f64,
}

/// Today's even stride against one frame per 5-minute interval: the kept one ("even" or "5min") has the lower held-out
/// log loss.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SampleReport {
    pub kept: String,
    pub even: SampleScore,
    pub interval: SampleScore,
}

/// The builds measured with one ΔW definition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowLift {
    pub draft: f64,
    pub baseline: f64,
    pub delta: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub low: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub high: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub brier: f64,
}

/// Today's ΔW (the result minus the chance before) against the effective window (2.2.0): `kept` "window" when its lift
/// and the lower end of its 95 % interval are both higher.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowReport {
    pub today: WindowLift,
    pub window: WindowLift,
    /// Scored item decisions (held out) whose item was sold or upgraded before the end.
    pub items: f64,
    pub kept: String,
    pub gate: String,
    pub gate_reason: String,
}

/// Game-time windows of the reliability tables, in minutes: 0–10, 10–20, 20–30, 30+.
pub const RELIABILITY_WINDOWS: [&str; 4] = ["0-10", "10-20", "20-30", "30+"];
/// Equal-width bins of the reliability tables.
pub const RELIABILITY_BINS: usize = 20;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReliabilityBinJson {
    pub from: f64,
    pub to: f64,
    pub count: f64,
    /// None for an empty bin.
    pub mean_prediction: Option<f64>,
    pub win_rate: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReliabilityJson {
    /// "all" or a minute window of `RELIABILITY_WINDOWS`.
    pub window: String,
    pub frames: f64,
    pub ece: f64,
    pub bins: Vec<ReliabilityBinJson>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateReliability {
    pub overall: ReliabilityJson,
    pub by_window: Vec<ReliabilityJson>,
}

fn reliability_json(window: &str, p: &[f64], y: &[f64]) -> ReliabilityJson {
    let r = super::logistic::reliability(p, y, RELIABILITY_BINS);
    ReliabilityJson {
        window: window.to_string(),
        frames: r.n,
        ece: r.ece,
        bins: r
            .bins
            .iter()
            .map(|b| ReliabilityBinJson { from: b.from, to: b.to, count: b.n, mean_prediction: b.predicted, win_rate: b.observed })
            .collect(),
    }
}

/// The reliability window of a frame `seconds` into the game (0 = 0–10 min … 3 = 30+). The frame's time is only kept as
/// the model input min(t, 2400)/1800 stored as f32, so a small tolerance keeps the exact minute marks in their window.
fn reliability_window(seconds: f64) -> usize {
    ((seconds / 600.0 + 1e-4).floor() as usize).min(3)
}

/// The overall and per-window reliability of `p` against `y`; `window[i]` is frame i's `reliability_window`.
fn state_reliability(p: &[f64], y: &[f64], window: &[u8]) -> StateReliability {
    let by_window = RELIABILITY_WINDOWS
        .iter()
        .enumerate()
        .map(|(w, name)| {
            let (mut pw, mut yw) = (Vec::new(), Vec::new());
            for i in 0..p.len() {
                if window[i] as usize == w {
                    pw.push(p[i]);
                    yw.push(y[i]);
                }
            }
            reliability_json(name, &pw, &yw)
        })
        .collect();
    StateReliability { overall: reliability_json("all", p, y), by_window }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationJson {
    pub from: f64,
    pub to: f64,
    pub n: f64,
    pub predicted: f64,
    pub observed: f64,
}

impl From<&CalibrationRow> for CalibrationJson {
    fn from(r: &CalibrationRow) -> Self {
        CalibrationJson { from: r.from, to: r.to, n: r.n, predicted: r.predicted, observed: r.observed }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GridRow {
    #[serde(deserialize_with = "de_h")]
    pub h: f64,
    pub k: f64,
    pub draft: f64,
    pub baseline: f64,
    pub delta: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Chosen {
    #[serde(deserialize_with = "de_h")]
    pub h: f64,
    pub k: f64,
}

/// A bandwidth read back: JS wrote Infinity as null.
fn de_h<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    let v: Option<f64> = Deserialize::deserialize(d)?;
    Ok(v.unwrap_or(f64::INFINITY))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionLift {
    pub key: String,
    pub draft: f64,
    pub baseline: f64,
    pub followed_draft: f64,
    pub followed_baseline: f64,
    pub n: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Within {
    pub draft: f64,
    pub baseline: f64,
    pub delta: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiftReport {
    pub draft: f64,
    pub baseline: f64,
    pub delta: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub low: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub high: f64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub within_pilot: Option<Within>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BrierReport {
    #[serde(deserialize_with = "js::de_num")]
    pub draft: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub baseline: f64,
    pub n: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stability {
    pub games: f64,
    pub minutes: f64,
    pub flips_per_game: f64,
    pub slot_changes_per_game: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WinCount {
    pub games: f64,
    pub wins: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Yours {
    pub linked: f64,
    pub followed: WinCount,
    pub other: WinCount,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryGridRow {
    pub decay: f64,
    pub memory: f64,
    pub classic: f64,
    pub delta: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryFull {
    pub memory: f64,
    pub classic: f64,
    pub delta: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub low: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub high: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub brier: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub classic_brier: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryEarly {
    pub memory: f64,
    pub classic: f64,
    pub delta: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub low: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub high: f64,
    pub current_games: f64,
    pub kept_games: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryReport {
    pub patches: f64,
    pub older_games: f64,
    pub decay: f64,
    pub grid: Vec<MemoryGridRow>,
    pub full: MemoryFull,
    pub early: MemoryEarly,
    pub gate: String,
    pub gate_reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpectedScore {
    pub brier: f64,
    pub log_loss: f64,
    pub base_brier: f64,
    pub base_log_loss: f64,
    pub games: f64,
    pub penalty: Option<Penalty>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FairLift {
    pub fair: f64,
    pub classic: f64,
    pub delta: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub low: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub high: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FairRaw {
    pub fair: f64,
    pub classic: f64,
    pub delta: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FairBrier {
    #[serde(deserialize_with = "js::de_num")]
    pub fair: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub classic: f64,
    pub n: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScoringDecision {
    pub key: String,
    pub fair: f64,
    pub classic: f64,
    pub changed: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScoringReport {
    pub expected: Option<ExpectedScore>,
    pub adjusted: FairLift,
    pub raw: FairRaw,
    pub brier: FairBrier,
    pub decisions: Vec<ScoringDecision>,
    pub gate: String,
    pub gate_reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SwapLift {
    pub advice: f64,
    pub keep: f64,
    pub delta: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub low: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub high: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapReport {
    pub lift: SwapLift,
    pub situations: f64,
    pub games: f64,
    pub swapped: f64,
    pub advised: f64,
    pub gate: String,
    pub gate_reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TuningChoice {
    pub pilot_strength: f64,
    pub item_k: Option<f64>,
    pub recency_days: Option<f64>,
}
pub const UNTUNED: TuningChoice = TuningChoice { pilot_strength: 1.0, item_k: None, recency_days: None };

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TuningGridRow {
    pub setting: String,
    pub value: f64,
    pub tuned: f64,
    pub current: f64,
    pub delta: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TuningFull {
    pub tuned: f64,
    pub current: f64,
    pub delta: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub low: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub high: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub brier: f64,
    #[serde(deserialize_with = "js::de_num")]
    pub current_brier: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TuningReport {
    pub grid: Vec<TuningGridRow>,
    pub choice: TuningChoice,
    pub full: Option<TuningFull>,
    pub gate: String,
    pub gate_reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub full_at: f64,
    pub new_games: f64,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub missing: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub changed: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub close: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupingJson {
    pub games: f64,
    pub groups: f64,
    pub spread: f64,
    pub ms: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalReport {
    pub evaluated_at: f64,
    pub games: f64,
    pub pilots: f64,
    pub folds: f64,
    pub state: Option<StateReport>,
    pub grid: Vec<GridRow>,
    pub chosen: Chosen,
    pub decisions: Vec<DecisionLift>,
    pub lift: LiftReport,
    pub brier: BrierReport,
    pub stability: Option<Stability>,
    pub yours: Option<Yours>,
    pub memory: Option<MemoryReport>,
    pub scoring: Option<ScoringReport>,
    #[serde(default)]
    pub tuning: Option<TuningReport>,
    #[serde(default)]
    pub swaps: Option<SwapReport>,
    pub gate: String,
    pub gate_reason: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub update: Option<UpdateInfo>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub grouping: Option<GroupingJson>,
    /// The effective-window candidate (2.2.0), when it ran.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub window: Option<WindowReport>,
}

// ------------------------------------------------------------------------------------------------- outcomes

#[derive(Clone, Debug)]
pub struct Outcome {
    pub pilot: Arc<str>,
    pub key: &'static str,
    pub wpa: f64,
    pub followed_draft: bool,
    pub followed_baseline: bool,
    pub w: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeyOutcomes {
    pub pilot: Vec<i32>,
    pub wpa: Vec<f64>,
    pub draft: Vec<u8>,
    pub baseline: Vec<u8>,
    pub w: Option<Vec<f64>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GroupedOutcomes {
    pub pilots: Vec<String>,
    pub by_key: Vec<KeyOutcomes>,
    /// The decisions, when not the gate's (EVAL_DECISIONS): late-game swaps'.
    pub keys: Option<Vec<String>>,
}

impl GroupedOutcomes {
    fn key_list(&self) -> Vec<String> {
        self.keys.clone().unwrap_or_else(|| EVAL_DECISIONS.iter().map(|s| s.to_string()).collect())
    }
}

pub fn group_outcomes(outcomes: &[Outcome], keys: Option<&[&'static str]>) -> GroupedOutcomes {
    let mut at: HashMap<&str, i32> = HashMap::new();
    let mut pilots: Vec<String> = Vec::new();
    for o in outcomes {
        if !at.contains_key(&*o.pilot) {
            at.insert(&o.pilot, pilots.len() as i32);
            pilots.push(o.pilot.to_string());
        }
    }
    let list: &[&'static str] = keys.unwrap_or(&EVAL_DECISIONS);
    let by_key = list
        .iter()
        .map(|key| {
            let of: Vec<&Outcome> = outcomes.iter().filter(|o| o.key == *key).collect();
            KeyOutcomes {
                pilot: of.iter().map(|o| at[&*o.pilot]).collect(),
                wpa: of.iter().map(|o| o.wpa).collect(),
                draft: of.iter().map(|o| o.followed_draft as u8).collect(),
                baseline: of.iter().map(|o| o.followed_baseline as u8).collect(),
                w: if of.iter().any(|o| o.w.is_some()) { Some(of.iter().map(|o| o.w.unwrap_or(1.0)).collect()) } else { None },
            }
        })
        .collect();
    GroupedOutcomes { pilots, by_key, keys: keys.map(|k| k.iter().map(|s| s.to_string()).collect()) }
}

/// A 48-bit hash of `s` with `salt` (sha256, first six bytes big-endian).
pub fn base_hash(salt: &str, s: &str) -> f64 {
    let mut h = Sha256::new();
    h.update(salt.as_bytes());
    h.update(s.as_bytes());
    let d = h.finalize();
    let mut v: u64 = 0;
    for b in &d[..6] {
        v = (v << 8) | *b as u64;
    }
    v as f64
}

/// What a game's measurement depends on, hashed.
pub fn game_print(salt: &str, g: &PreparedGame) -> f64 {
    use serde_json::{json, Value};
    let ns = |x: Option<f64>| x.map(js::num).unwrap_or(Value::Null);
    let v = json!([
        g.platform, g.patch, js::num(g.created), g.pilot, g.source, g.tier, ns(g.lp), ns(g.mastery), g.lane, js::num(g.win),
        g.opponent, g.enemies, g.allies, g.primary, g.sub, g.perks, g.shards, g.summoners, g.start, g.max_first, g.early,
        g.items.iter().map(|c| json!([js::num(c.item), js::num(c.t)])).collect::<Vec<_>>(),
        g.boots.as_ref().map(|b| json!([js::num(b.item), js::num(b.t)])),
        js::num(g.duration), ns(g.lane10), ns(g.lane15)
    ]);
    base_hash(salt, &js::stringify(&v))
}

/// `base`'s outcomes, then `more`'s (more's pilots named by `pilot_key`).
pub fn merge_outcomes(base: &GroupedOutcomes, more: &GroupedOutcomes, pilot_key: &dyn Fn(&str) -> String) -> GroupedOutcomes {
    let mut at: HashMap<String, i32> = base.pilots.iter().enumerate().map(|(i, p)| (p.clone(), i as i32)).collect();
    let mut pilots = base.pilots.clone();
    let index: Vec<i32> = more
        .pilots
        .iter()
        .map(|p| {
            let key = pilot_key(p);
            *at.entry(key.clone()).or_insert_with(|| {
                pilots.push(key);
                pilots.len() as i32 - 1
            })
        })
        .collect();
    let by_key = base
        .by_key
        .iter()
        .enumerate()
        .map(|(d, b)| {
            let m = &more.by_key[d];
            let ones = |k: usize| vec![1.0; k];
            KeyOutcomes {
                pilot: b.pilot.iter().copied().chain(m.pilot.iter().map(|p| index[*p as usize])).collect(),
                wpa: b.wpa.iter().chain(m.wpa.iter()).copied().collect(),
                draft: b.draft.iter().chain(m.draft.iter()).copied().collect(),
                baseline: b.baseline.iter().chain(m.baseline.iter()).copied().collect(),
                w: if b.w.is_some() || m.w.is_some() {
                    Some(b.w.clone().unwrap_or_else(|| ones(b.pilot.len())).into_iter().chain(m.w.clone().unwrap_or_else(|| ones(m.pilot.len()))).collect())
                } else {
                    None
                },
            }
        })
        .collect();
    GroupedOutcomes { pilots, by_key, keys: base.keys.clone() }
}

/// withinPilot() from grouped outcomes.
pub fn within_pilot_of(g: &GroupedOutcomes) -> Within {
    let add = |a: &[f64; 4], into: &mut [f64; 2]| {
        if a[1] == 0.0 || a[3] == 0.0 {
            return;
        }
        let w = (a[1] * a[3]) / (a[1] + a[3]);
        into[0] += w * (a[0] / a[1] - a[2] / a[3]);
        into[1] += w;
    };
    let mut sums: HashMap<&str, ([f64; 2], [f64; 2])> = HashMap::new();
    for (d, key) in EVAL_DECISIONS.iter().enumerate() {
        let Some(k) = g.by_key.get(d) else { continue };
        if k.pilot.is_empty() {
            continue;
        }
        let mut cells: indexmap::IndexMap<i32, ([f64; 4], [f64; 4])> = indexmap::IndexMap::new();
        for j in 0..k.pilot.len() {
            let c = cells.entry(k.pilot[j]).or_insert(([0.0; 4], [0.0; 4]));
            if k.draft[j] != 0 {
                c.0[0] += k.wpa[j];
                c.0[1] += 1.0;
            } else {
                c.0[2] += k.wpa[j];
                c.0[3] += 1.0;
            }
            if k.baseline[j] != 0 {
                c.1[0] += k.wpa[j];
                c.1[1] += 1.0;
            } else {
                c.1[2] += k.wpa[j];
                c.1[3] += 1.0;
            }
        }
        let mut s = ([0.0; 2], [0.0; 2]);
        for c in cells.values() {
            add(&c.0, &mut s.0);
            add(&c.1, &mut s.1);
        }
        sums.insert(key, s);
    }
    let keys: Vec<&str> = EVAL_DECISIONS.iter().copied().filter(|k| sums.contains_key(k)).collect();
    let mean = |f: &dyn Fn(&([f64; 2], [f64; 2])) -> [f64; 2]| -> f64 {
        if keys.is_empty() {
            return 0.0;
        }
        let mut t = 0.0;
        for k in &keys {
            let [x, w] = f(&sums[k]);
            t += if w != 0.0 { x / w } else { 0.0 };
        }
        t / keys.len() as f64
    };
    let draft = mean(&|s| s.0);
    let baseline = mean(&|s| s.1);
    Within { draft, baseline, delta: draft - baseline }
}

pub struct Lifts {
    pub per: Vec<DecisionLift>,
    pub draft: f64,
    pub baseline: f64,
}

/// Lift of following a model per decision, and their mean. `weights`: per pilot (the bootstrap).
pub fn lifts(g: &GroupedOutcomes, weights: Option<&[f64]>, min_group: f64, only: Option<&[&str]>) -> Lifts {
    let mut per: Vec<DecisionLift> = Vec::new();
    for (d, key) in g.key_list().iter().enumerate() {
        if let Some(o) = only {
            if !o.contains(&key.as_str()) {
                continue;
            }
        }
        let k = &g.by_key[d];
        let (mut ad, mut ab, mut n) = ([0.0f64; 4], [0.0f64; 4], 0.0f64);
        for j in 0..k.pilot.len() {
            let w = weights.map(|ws| ws[k.pilot[j] as usize]).unwrap_or(1.0) * k.w.as_ref().map(|ws| ws[j]).unwrap_or(1.0);
            if w == 0.0 || w.is_nan() {
                continue;
            }
            n += w;
            if k.draft[j] != 0 {
                ad[0] += w * k.wpa[j];
                ad[1] += w;
            } else {
                ad[2] += w * k.wpa[j];
                ad[3] += w;
            }
            if k.baseline[j] != 0 {
                ab[0] += w * k.wpa[j];
                ab[1] += w;
            } else {
                ab[2] += w * k.wpa[j];
                ab[3] += w;
            }
        }
        let lift = |a: &[f64; 4]| if a[1] >= min_group && a[3] >= min_group { a[0] / a[1] - a[2] / a[3] } else { 0.0 };
        per.push(DecisionLift {
            key: key.clone(),
            draft: lift(&ad),
            baseline: lift(&ab),
            followed_draft: if n != 0.0 { ad[1] / n } else { 0.0 },
            followed_baseline: if n != 0.0 { ab[1] / n } else { 0.0 },
            n,
        });
    }
    let mean = |f: &dyn Fn(&DecisionLift) -> f64| {
        let mut s = 0.0;
        for d in &per {
            s += f(d);
        }
        s / per.len() as f64
    };
    let draft = mean(&|d| d.draft);
    let baseline = mean(&|d| d.baseline);
    Lifts { per, draft, baseline }
}

fn clip(p: f64) -> f64 {
    js_min(0.99, js_max(0.01, p))
}

fn sq(x: f64) -> f64 {
    x * x
}

/// What a build recommends for each decision.
pub type Picks = HashMap<&'static str, String>;

pub fn picks_of(b: &BuildRecommendation) -> Picks {
    let mut out = HashMap::new();
    if let Some(r) = &b.runes {
        out.insert("keystone", r.value.perks.first().map(|x| x.to_string()).unwrap_or_else(|| "undefined".into()));
        out.insert("secondaryTree", r.value.sub_style.to_string());
    }
    if let Some(s) = &b.summoners {
        let mut v = s.value.clone();
        v.sort();
        out.insert("summoners", v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","));
    }
    if let Some(s) = b.starting_items.first() {
        out.insert("start", s.value.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","));
    }
    if let Some(s) = &b.skills {
        if let Some(p) = s.value.priority.first() {
            out.insert("maxFirst", p.clone());
        }
    }
    let core: Vec<i64> = b.core_build.first().map(|o| o.value.clone()).unwrap_or_default();
    for (i, id) in core.iter().take(3).enumerate() {
        out.insert(["core1", "core2", "core3"][i], id.to_string());
    }
    if let Some(bt) = b.boots.first() {
        out.insert("boots", bt.value.to_string());
    }
    out
}

/// What the pilot did, and the WPA of it, in TS insertion order.
pub fn actual(s: &ScoredGame) -> Vec<(&'static str, String, f64)> {
    let g = &s.g;
    let mut out: Vec<(&'static str, String, f64)> =
        vec![("keystone", g.perks[0].to_string(), s.wpa0), ("secondaryTree", g.sub.to_string(), s.wpa0), ("summoners", g.summoners.clone(), s.wpa0)];
    if !g.start.is_empty() {
        out.push(("start", g.start.clone(), s.wpa0));
    }
    if let Some(m) = g.max_first.as_ref().filter(|m| !m.is_empty()) {
        out.push(("maxFirst", m.clone(), s.wpa0));
    }
    for (i, c) in g.items.iter().take(3).enumerate() {
        out.push((["core1", "core2", "core3"][i], js::num_to_string(c.item), s.item_wpa[i]));
    }
    if let (Some(b), Some(w)) = (&g.boots, s.boots_wpa) {
        out.push(("boots", js::num_to_string(b.item), w));
    }
    out
}

/// Each game's purchases' win probability under each fold's state model.
pub fn purchase_chances(games: &[&PreparedGame], models: &[Option<StateModel>]) -> Vec<f64> {
    let nf = models.len();
    let mut out = vec![0.0; games.len() * nf * SLOTS];
    for (f, m) in models.iter().enumerate() {
        let base = m.as_ref().map(|m| m.base).unwrap_or(0.5);
        let p = |x: &Option<Box<[f64; N_FEATURES]>>| match (m, x) {
            (Some(m), Some(x)) => win_probability_of(&m.beta, &x[..]),
            _ => base,
        };
        for (k, g) in games.iter().enumerate() {
            let at = (k * nf + f) * SLOTS;
            for (slot, c) in g.items.iter().enumerate() {
                out[at + slot] = p(&c.x);
            }
            if let Some(b) = &g.boots {
                out[at + SLOTS - 1] = p(&b.x);
            }
        }
    }
    out
}

/// Each purchase's win probability when it left the inventory (2.2.0's effective window), in purchase_chances's
/// layout: NaN when kept to the end; the chance before it when it left before the first frame.
pub fn end_chances(games: &[&PreparedGame], models: &[Option<StateModel>], before: &[f64]) -> Vec<f64> {
    let nf = models.len();
    let mut out = vec![f64::NAN; games.len() * nf * SLOTS];
    for (f, m) in models.iter().enumerate() {
        let base = m.as_ref().map(|m| m.base).unwrap_or(0.5);
        for (k, g) in games.iter().enumerate() {
            let at = (k * nf + f) * SLOTS;
            let mut put = |slot: usize, e: &Option<super::window::WindowEnd>| {
                if let Some(e) = e {
                    out[at + slot] = match (m, &e.x) {
                        (Some(m), Some(x)) => win_probability_of(&m.beta, &x[..]),
                        (None, Some(_)) => base,
                        (_, None) => before[at + slot],
                    };
                }
            };
            for slot in 0..g.items.len() {
                if let Some(e) = g.item_ends.get(slot) {
                    put(slot, e);
                }
            }
            if g.boots.is_some() {
                put(SLOTS - 1, &g.boots_end);
            }
        }
    }
    out
}

fn strided(n: usize, stride: usize) -> Vec<usize> {
    (0..n).step_by(stride.max(1)).collect()
}

// ------------------------------------------------------------------------------------------------- the team

/// The evaluation's threads: a rayon pool. `for_each` works out each index anywhere and applies the results in
/// index order, as evalTeam.ts's team does.
pub struct Team {
    pool: Option<rayon::ThreadPool>,
    pub size: usize,
    control: Option<Arc<EvalControl>>,
}

/// The app's hold on a running evaluation (server/jobs.ts): `stop` ends it (Cancel, a data-folder move), `busy`
/// (champ select or a game) has it finish on one thread. Neither changes a result: work is computed in index order
/// batches and applied in index order, whatever the threads.
#[derive(Default, Debug)]
pub struct EvalControl {
    pub busy: std::sync::atomic::AtomicBool,
    pub stop: std::sync::atomic::AtomicBool,
}

/// The panic payload a stopped evaluation unwinds with (caught by the job that runs it).
#[derive(Debug, Clone, Copy)]
pub struct EvalStopped;

impl EvalControl {
    pub fn check(&self) {
        if self.stop.load(Ordering::Relaxed) {
            std::panic::panic_any(EvalStopped);
        }
    }
}

impl Team {
    pub fn new(threads: usize) -> Team {
        Team::with_control(threads, None)
    }

    pub fn with_control(threads: usize, control: Option<Arc<EvalControl>>) -> Team {
        let size = threads.max(1);
        let pool = if size > 1 { rayon::ThreadPoolBuilder::new().num_threads(size).thread_name(|i| format!("eval-{i}")).build().ok() } else { None };
        Team { pool, size, control }
    }

    pub fn for_each<R: Send>(&self, n: usize, compute: impl Fn(usize) -> R + Sync + Send, mut apply: impl FnMut(usize, R), progress: &(dyn Fn(usize, usize) + Sync)) {
        let done = AtomicUsize::new(0);
        let run = |j: usize| {
            if let Some(c) = &self.control {
                c.check();
            }
            let r = compute(j);
            let d = done.fetch_add(1, Ordering::Relaxed) + 1;
            progress(d, n);
            r
        };
        let results: Vec<R> = match (&self.pool, &self.control) {
            (Some(p), None) => p.install(|| (0..n).into_par_iter().map(run).collect()),
            (Some(p), Some(c)) => {
                // In batches, so champ select or a game moves the rest onto this thread alone.
                let batch = (self.size * 16).max(64);
                let mut out: Vec<R> = Vec::with_capacity(n);
                let mut start = 0;
                while start < n {
                    c.check();
                    let end = (start + batch).min(n);
                    if c.busy.load(Ordering::Relaxed) {
                        out.extend((start..end).map(run));
                    } else {
                        out.extend(p.install(|| (start..end).into_par_iter().map(run).collect::<Vec<R>>()));
                    }
                    start = end;
                }
                out
            }
            (None, _) => (0..n).map(run).collect(),
        };
        for (j, r) in results.into_iter().enumerate() {
            apply(j, r);
        }
    }
}

// ------------------------------------------------------------------------------------------------- input

#[derive(Clone, Debug)]
pub struct EvalProgress {
    pub step: usize,
    pub steps: usize,
    pub label: String,
    pub part: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StepTime {
    pub label: String,
    pub ms: f64,
}

/// What a full evaluation measured, or a quick update with its base's added: saved as the next base.
#[derive(Clone, Debug, Default)]
pub struct EvalBaseData {
    pub salt: String,
    pub known: Vec<f64>,
    pub prints: Vec<f64>,
    pub report: Option<EvalReport>,
    pub final_: Option<(GroupedOutcomes, [f64; 3])>,
    pub memory: Option<(GroupedOutcomes, [f64; 2], GroupedOutcomes)>,
    pub scoring: Option<ScoringBase>,
    pub tuning: Option<(GroupedOutcomes, [f64; 2])>,
    pub swaps: Option<(GroupedOutcomes, [f64; 4])>,
}

#[derive(Clone, Debug, Default)]
pub struct ScoringBase {
    pub adjusted: GroupedOutcomes,
    pub raw: GroupedOutcomes,
    pub brier: [f64; 3],
    pub changed: Vec<(String, [f64; 2])>,
    pub expected_p: Vec<f64>,
    pub expected_y: Vec<f64>,
    pub expected_pb: Vec<f64>,
    pub penalties: Vec<Option<Penalty>>,
}

pub struct EvalInput<'a> {
    /// The stored games, in the database's order (with their timelines).
    pub rows: Vec<IreliaGameRow>,
    /// Reads the raw timelines of these games again (the live replay's sample).
    pub timelines_of: Option<Box<dyn Fn(&[String]) -> HashMap<String, String> + 'a>>,
    pub classic_patches: Option<Vec<String>>,
    pub participants: Vec<ParticipantLite>,
    pub data: Arc<StaticData>,
    pub excl: Arc<ExclusivityIndex>,
    pub profiles: Arc<ChampionProfiles>,
    pub region_patch: RegionPatchFn,
    pub rec_log: Vec<RecLogRow>,
    pub starting_item_window_ms: f64,
    pub log: Box<dyn Fn(&str) + Sync + 'a>,
    pub progress: Box<dyn Fn(EvalProgress) + Sync + 'a>,
    pub threads: usize,
    pub base: Option<EvalBaseData>,
    pub salt: Option<String>,
    /// The app's stop and busy flags (None: the command line and the parity harness).
    pub control: Option<Arc<EvalControl>>,
    /// Tests only: changes the prepared games before they are used (0.x's `prepare` option).
    pub prepared_hook: Option<Box<dyn Fn(&mut Vec<PreparedGame>) + 'a>>,
}

pub struct EvalOutput {
    pub report: EvalReport,
    pub next: Option<EvalBaseData>,
    pub timings: Vec<StepTime>,
}

struct Clock {
    timings: Vec<StepTime>,
    at: Instant,
    label: String,
}

// ------------------------------------------------------------------------------------------------- evaluate

struct FoldBaselines {
    baselines: Mutex<HashMap<String, Arc<BuildRecommendation>>>,
    train_rows: Mutex<Option<Vec<Arc<IreliaGameRow>>>>,
    validated: Mutex<HashMap<String, Arc<ValidatedRows>>>,
    win_rates: HashMap<String, (f64, f64)>,
    lane_rate: HashMap<&'static str, (f64, f64)>,
}

impl FoldBaselines {
    fn base_rate(&self, lane: &str, key: &str, option: &str) -> f64 {
        let lr = self.lane_rate.get(lane).copied().unwrap_or((1.0, 2.0));
        let e = self.win_rates.get(&format!("{lane}:{key}:{option}")).copied().unwrap_or((0.0, 0.0));
        (e.0 + 10.0 * (lr.0 / lr.1)) / (e.1 + 10.0)
    }
}

/// The evaluation. Runs on `input.threads` threads; the report is the same on any number.
pub fn evaluate(input: EvalInput, opts: &EvalOptions) -> Result<EvalOutput> {
    let mut clock = Clock { timings: vec![], at: Instant::now(), label: "Reading the stored games".into() };
    let r = evaluate_timed(input, opts, &mut clock);
    clock.timings.push(StepTime { label: clock.label.clone(), ms: clock.at.elapsed().as_secs_f64() * 1000.0 });
    r.map(|(report, next)| EvalOutput { report, next, timings: clock.timings })
}

fn pts(x: f64) -> String {
    format!("{}{} pts", if x >= 0.0 { "+" } else { "" }, js::to_fixed(x * 100.0, 2))
}

#[allow(clippy::type_complexity)]
fn evaluate_timed(input: EvalInput, opts: &EvalOptions, clock: &mut Clock) -> Result<(EvalReport, Option<EvalBaseData>)> {
    let data = input.data.clone();
    let excl = input.excl.clone();
    let profiles = input.profiles.clone();
    let region_patch = input.region_patch.clone();
    let log = &input.log;
    let steps = eval_steps(opts);
    let step_no = std::cell::Cell::new(0usize);
    let current = RefCell::new(clock.label.clone());
    let clock = RefCell::new(clock);
    let lap = |label: &str| {
        let mut c = clock.borrow_mut();
        let now = Instant::now();
        let ms = now.duration_since(c.at).as_secs_f64() * 1000.0;
        let l = c.label.clone();
        c.timings.push(StepTime { label: l, ms });
        c.at = now;
        c.label = label.to_string();
        *current.borrow_mut() = label.to_string();
    };
    let step = |label: &str| {
        lap(label);
        step_no.set(step_no.get() + 1);
        (input.progress)(EvalProgress { step: step_no.get(), steps, label: label.to_string(), part: None });
    };
    let team = Team::with_control(input.threads, input.control.clone());
    let progress_fn = &input.progress;
    let part_for = |label: String, at: usize| {
        move |done: usize, of: usize| {
            if of > 0 && (done % 64 == 0 || done == of) {
                progress_fn(EvalProgress { step: at, steps, label: label.clone(), part: Some((done as f64 / of as f64).min(1.0)) });
            }
        }
    };
    // The same, through fold `f` of a step that goes over the folds in turn (evaluate.ts foldPart): the bar moves on
    // from fold to fold instead of starting again at each.
    let fold_part_for = |label: String, at: usize, f: usize, folds: usize| {
        move |done: usize, of: usize| {
            if of > 0 && (done % 64 == 0 || done == of) {
                let frac = (f as f64 + done as f64 / of as f64) / folds.max(1) as f64;
                progress_fn(EvalProgress { step: at, steps, label: label.clone(), part: Some(frac.min(1.0)) });
            }
        }
    };

    // ---- reading and preparing
    let rows: Vec<Arc<IreliaGameRow>> = input.rows.into_iter().map(Arc::new).collect();
    let prepared_all = {
        let by = super::dataset::by_match(&input.participants);
        let empty: Vec<&ParticipantLite> = vec![];
        let o = PrepareOptions { starting_item_window_ms: input.starting_item_window_ms };
        let mut out: Vec<Option<PreparedGame>> = Vec::new();
        let compute = |r: &Arc<IreliaGameRow>| super::dataset::prepare_game(r, by.get(r.match_id.as_str()).unwrap_or(&empty), &data, &profiles, &o);
        match &team.pool {
            Some(p) => p.install(|| out = rows.par_iter().map(compute).collect()),
            None => out = rows.iter().map(compute).collect(),
        }
        out.into_iter().flatten().collect::<Vec<_>>()
    };
    let _ = prepare_games; // (the one-thread path of the same preparation)
    let mut prepared = prepared_all;
    if let Some(hook) = &input.prepared_hook {
        hook(&mut prepared);
    }
    let classic: Option<HashSet<String>> = input.classic_patches.as_ref().map(|p| p.iter().cloned().collect());
    let is_classic = |g: &PreparedGame| classic.as_ref().map(|c| c.contains(&g.patch)).unwrap_or(true);
    let ager = PatchAger::new(data.clone(), region_patch.clone());
    let age_of = |g: &PreparedGame| ager.age(&g.platform, &g.patch);
    let memory_window = opts.memory.as_ref().map(|m| m.patches).unwrap_or(0.0);
    // Split: evaluated games, and those only patch memory reads.
    let mut games_v: Vec<PreparedGame> = Vec::new();
    let mut older_v: Vec<PreparedGame> = Vec::new();
    for mut g in prepared.drain(..) {
        if g.source == "me" {
            continue;
        }
        if is_classic(&g) {
            games_v.push(g);
        } else {
            g.frames = None;
            let a = age_of(&g);
            if a >= 2.0 && a < memory_window {
                older_v.push(g);
            }
        }
    }
    let mut grouping: Option<GroupingStats> = None;
    if let (Some(groups), None) = (opts.grouping, &opts.update) {
        lap("Grouping the drafts");
        let n_g = games_v.len();
        let mut all: Vec<PreparedGame> = games_v.drain(..).chain(older_v.drain(..)).collect();
        let target = if groups > 0 { groups } else { auto_groups(all.len()) };
        let st = assign_draft_groups(&mut all, target, opts.seed);
        log(&format!(
            "grouped drafts: {} groups of {} games, spread {} sd, {} s",
            js::thousands(st.groups as f64),
            js::thousands(st.games as f64),
            js::to_fixed(st.spread, 3),
            js::to_fixed(st.ms / 1000.0, 1)
        ));
        older_v = all.split_off(n_g);
        games_v = all;
        grouping = Some(st);
        lap("Setting up the folds");
    }
    for (k, g) in games_v.iter_mut().chain(older_v.iter_mut()).enumerate() {
        g.ix = k;
    }
    let nf = opts.folds;
    let fold_of = |g: &PreparedGame| fnv(&format!("{}:{}", js::num_to_string(opts.seed), g.pilot)) as usize % nf;

    // ---- B2: the state model's penalty, by held-out log loss (the frames are needed here only)
    let width = N_FEATURES;
    let fold_tests: Vec<Vec<usize>> = (0..nf).map(|f| (0..games_v.len()).filter(|i| fold_of(&games_v[*i]) == f).collect()).collect();
    let held_out_games: usize = fold_tests.iter().map(|t| t.len()).sum();
    let pilots_n = games_v.iter().map(|g| g.pilot.as_str()).collect::<HashSet<_>>().len();
    log(&format!("{} games from {} pilots, {} folds by pilot", games_v.len(), pilots_n, nf));
    let up = opts.update.clone();
    let live_stride = 1usize.max((held_out_games as f64 / opts.max_live_games as f64).ceil() as usize);
    let replay_ids: HashSet<String> = if opts.live && up.is_none() {
        fold_tests.iter().flat_map(|t| t.iter().enumerate().filter(|(i, _)| i % live_stride == 0).map(|(_, gi)| games_v[*gi].match_id.clone())).collect()
    } else {
        HashSet::new()
    };
    // 2.2.0: with the sample candidate on, every penalty is fitted on both samples; otherwise on today's (or, in a quick
    // update, the base's), exactly as before.
    let sample_on = opts.state_sample && up.is_none();
    let samples: Vec<StateSample> = if sample_on { vec![StateSample::Even, StateSample::Interval] } else { vec![up.as_ref().map(|u| u.sample).unwrap_or_default()] };
    type Fit = (f64, Vec<Option<StateModel>>, (f64, f64), (Vec<f64>, Vec<f64>, Vec<f64>), Vec<u8>);
    let mut state_fits: Vec<Option<Fit>> = samples.iter().map(|_| None).collect();
    let mut first_fits: Vec<Option<Fit>> = samples.iter().map(|_| None).collect();
    let mut by_lambdas: Vec<Vec<LambdaScore>> = samples.iter().map(|_| Vec::new()).collect();
    let mut sample_rows: Vec<f64> = samples.iter().map(|_| 0.0).collect();
    {
        let fold_rows_by: Vec<Vec<Option<super::dataset::StateRows>>> = samples
            .iter()
            .map(|sample| (0..nf).map(|f| state_rows_by(games_v.iter().filter(|g| fold_of(g) != f), super::dataset::STATE_MAX_ROWS, *sample)).collect())
            .collect();
        for (si, fr) in fold_rows_by.iter().enumerate() {
            sample_rows[si] = fr.iter().map(|r| r.as_ref().map(|r| r.x.len()).unwrap_or(0) as f64).sum::<f64>() / js_max(1.0, nf as f64);
        }
        let fold_base: Vec<f64> = (0..nf)
            .map(|f| {
                let train: Vec<&PreparedGame> = games_v.iter().filter(|g| fold_of(g) != f).collect();
                let mut s = 0.0;
                for g in &train {
                    s += g.win;
                }
                s / js_max(1.0, train.len() as f64)
            })
            .collect();
        let lambdas: Vec<f64> = match &up {
            Some(u) => vec![u.lambda],
            None => opts.lambdas.clone(),
        };
        for (i, lambda) in lambdas.iter().enumerate() {
            step(&format!("Testing the game-state model ({} of {})", i + 1, lambdas.len()));
            for (si, sample) in samples.iter().enumerate() {
                let fold_rows = &fold_rows_by[si];
                let (mut p, mut y, mut pb) = (Vec::new(), Vec::new(), Vec::new());
                let mut win_of: Vec<u8> = Vec::new();
                let mut models: Vec<Option<StateModel>> = Vec::new();
                for f in 0..nf {
                    let m = match &fold_rows[f] {
                        Some(r) => {
                            let rows: Vec<&[f64]> = r.x.iter().map(|x| x.as_slice()).collect();
                            Some(train_state_model(&rows, &r.win, r.games as f64, *lambda)?)
                        }
                        None => None,
                    };
                    for gi in &fold_tests[f] {
                        let g = &games_v[*gi];
                        let Some(fr) = &g.frames else { continue };
                        for k in 0..fr.len() / width {
                            let x: Vec<f64> = fr[k * width..(k + 1) * width].iter().map(|v| *v as f64).collect();
                            p.push(match &m {
                                Some(m) => win_probability_of(&m.beta, &x),
                                None => fold_base[f],
                            });
                            pb.push(fold_base[f]);
                            y.push(g.win);
                            win_of.push(reliability_window(fr[k * width + 1] as f64 * 1800.0) as u8);
                        }
                    }
                    models.push(m);
                }
                let s = if p.is_empty() { (f64::NAN, f64::NAN) } else {
                    let sc = score_probabilities(&p, &y);
                    (sc.brier, sc.log_loss)
                };
                if sample_on || *sample != StateSample::Even {
                    log(&format!(
                        "state model λ={}, {}: Brier {}, log loss {}, ECE {} on {} held-out frames",
                        js::num_to_string(*lambda),
                        sample.label(),
                        js::to_fixed(s.0, 4),
                        js::to_fixed(s.1, 4),
                        js::to_fixed(ece(&p, &y, 20), 4),
                        p.len()
                    ));
                } else {
                    log(&format!("state model λ={}: Brier {}, log loss {} on {} held-out frames", js::num_to_string(*lambda), js::to_fixed(s.0, 4), js::to_fixed(s.1, 4), p.len()));
                }
                by_lambdas[si].push(LambdaScore { lambda: *lambda, brier: s.0, log_loss: s.1 });
                let fit = (*lambda, models, s, (p, y, pb), win_of);
                let better = s.1.is_finite() && state_fits[si].as_ref().map(|b| s.1 < b.2 .1).unwrap_or(true);
                if first_fits[si].is_none() {
                    first_fits[si] = Some(fit.clone());
                }
                if better {
                    state_fits[si] = Some(fit);
                }
            }
        }
    }
    let mut bests: Vec<Fit> = state_fits.into_iter().zip(first_fits).map(|(b, f)| b.or(f).expect("at least one penalty")).collect();
    // The kept sample: the 5-minute one only with a lower held-out log loss.
    let kept_at = if sample_on && bests[1].2 .1.is_finite() && !(bests[1].2 .1 >= bests[0].2 .1) { 1 } else { 0 };
    let sample_report = if sample_on {
        let score = |si: usize| {
            let b = &bests[si];
            SampleScore { lambda: b.0, brier: b.2 .0, log_loss: b.2 .1, ece: ece(&b.3 .0, &b.3 .1, 20), rows: sample_rows[si] }
        };
        let (even, interval) = (score(0), score(1));
        log(&format!(
            "state-model sample: {} λ={}: log loss {}, Brier {}, ECE {}, {} rows a fold; {} λ={}: log loss {}, Brier {}, ECE {}, {} rows a fold; kept: {}",
            samples[0].label(),
            js::num_to_string(even.lambda),
            js::to_fixed(even.log_loss, 4),
            js::to_fixed(even.brier, 4),
            js::to_fixed(even.ece, 4),
            js::num_to_string(js::round(even.rows)),
            samples[1].label(),
            js::num_to_string(interval.lambda),
            js::to_fixed(interval.log_loss, 4),
            js::to_fixed(interval.brier, 4),
            js::to_fixed(interval.ece, 4),
            js::num_to_string(js::round(interval.rows)),
            samples[kept_at].label()
        ));
        Some(SampleReport { kept: samples[kept_at].name().into(), even, interval })
    } else {
        input.base.as_ref().filter(|_| up.is_some()).and_then(|b| b.report.as_ref()).and_then(|r| r.state.as_ref()).and_then(|s| s.sample.clone())
    };
    let best_state = bests.swap_remove(kept_at);
    drop(bests);
    let by_lambda = by_lambdas.swap_remove(kept_at);
    let state_models: Vec<Option<StateModel>> = best_state.1.clone();
    let state_report = {
        let (p, y, pb) = &best_state.3;
        if !pb.is_empty() && best_state.2 .0.is_finite() {
            let base = score_probabilities(pb, y);
            Some(StateReport {
                lambda: best_state.0,
                brier: best_state.2 .0,
                log_loss: best_state.2 .1,
                base_brier: base.brier,
                base_log_loss: base.log_loss,
                frames: p.len() as f64,
                by_lambda: by_lambda.clone(),
                calibration: calibration(p, y, 10).iter().map(CalibrationJson::from).collect(),
                sample: sample_report,
                reliability: Some(state_reliability(p, y, &best_state.4)),
            })
        } else {
            None
        }
    };
    if let Some(r) = state_report.as_ref().and_then(|r| r.reliability.as_ref()) {
        for w in std::iter::once(&r.overall).chain(r.by_window.iter()) {
            let label = if w.window == "all" { "all game times".to_string() } else { format!("minutes {}", w.window) };
            log(&format!("state model calibration, {label}: ECE {} on {} held-out frames", js::to_fixed(w.ece, 4), js::thousands(w.frames)));
        }
    }
    drop(best_state);
    // Each purchase's win probability under each fold's state model, once; then the frames and features go.
    let item_p = {
        let all: Vec<&PreparedGame> = games_v.iter().chain(older_v.iter()).collect();
        purchase_chances(&all, &state_models)
    };
    // 2.2.0's effective window: measured against today's ΔW (the candidate), or used as a quick update's base did.
    let window_on = opts.effective_window && up.is_none();
    let use_window = AtomicBool::new(up.as_ref().map(|u| u.window).unwrap_or(false));
    let (end_p, window_items) = if window_on || use_window.load(Ordering::Relaxed) {
        let all: Vec<&PreparedGame> = games_v.iter().chain(older_v.iter()).collect();
        let n = games_v.iter().map(|g| g.item_ends.iter().take(3).filter(|e| e.is_some()).count() + if g.boots.is_some() && g.boots_end.is_some() { 1 } else { 0 }).sum::<usize>();
        (end_chances(&all, &state_models, &item_p), n as f64)
    } else {
        (Vec::new(), 0.0)
    };
    for g in games_v.iter_mut().chain(older_v.iter_mut()) {
        g.frames = None;
        g.frame_t = None;
        g.purchase_ends = vec![];
        for e in g.item_ends.iter_mut().flatten() {
            e.x = None;
        }
        if let Some(e) = g.boots_end.as_mut() {
            e.x = None;
        }
        if opts.swaps {
            for c in g.swaps.iter_mut() {
                c.p = Some(
                    state_models
                        .iter()
                        .map(|m| match (m, &c.x) {
                            (Some(m), Some(x)) => win_probability_of(&m.beta, &x[..]),
                            _ => m.as_ref().map(|m| m.base).unwrap_or(0.5),
                        })
                        .collect(),
                );
            }
        }
        for c in g.items.iter_mut() {
            c.x = None;
        }
        if let Some(b) = g.boots.as_mut() {
            b.x = None;
        }
        for c in g.swaps.iter_mut() {
            c.x = None;
        }
        if !opts.swaps {
            g.swaps.clear();
        }
        g.purchases = vec![];
    }
    let games: Vec<Arc<PreparedGame>> = games_v.into_iter().map(Arc::new).collect();
    let older_games: Vec<Arc<PreparedGame>> = older_v.into_iter().map(Arc::new).collect();
    // The evaluated games' rows (their lane baselines, the live replay).
    let rows_by_id: HashMap<String, Arc<IreliaGameRow>> = {
        let ids: HashSet<&str> = games.iter().map(|g| g.match_id.as_str()).collect();
        rows.iter()
            .filter(|r| ids.contains(r.match_id.as_str()))
            .map(|r| {
                let keep = replay_ids.contains(&r.match_id) && input.timelines_of.is_none();
                let mut c = (**r).clone();
                if !keep {
                    c.timeline = None;
                }
                (r.match_id.clone(), Arc::new(c))
            })
            .collect()
    };
    drop(rows);
    let mut parts_by_match: HashMap<String, Vec<ParticipantLite>> = HashMap::new();
    for p in input.participants {
        if replay_ids.contains(&p.match_id) {
            parts_by_match.entry(p.match_id.clone()).or_default().push(p);
        }
    }
    let replay_timelines: Option<HashMap<String, String>> =
        input.timelines_of.as_ref().map(|f| f(&replay_ids.iter().cloned().collect::<Vec<_>>()));

    // ---- the comparisons
    // `window`: an item's ΔW ends when it is sold or upgraded (2.2.0); otherwise today's, the result minus the chance before.
    let delta = |g: &PreparedGame, at: usize, window: bool| -> f64 {
        if window && !end_p[at].is_nan() {
            end_p[at] - item_p[at]
        } else {
            g.win - item_p[at]
        }
    };
    let scored_as = |list: &[Arc<PreparedGame>], f: usize, expected: Option<&[f64]>, window: bool| -> Vec<ScoredGame> {
        let base = state_models[f].as_ref().map(|m| m.base).unwrap_or(0.5);
        list.iter()
            .enumerate()
            .map(|(i, g)| {
                let at = (g.ix * nf + f) * SLOTS;
                ScoredGame {
                    g: g.clone(),
                    wpa0: g.win - expected.and_then(|e| e.get(i).copied()).filter(|e| e.is_finite()).unwrap_or(base),
                    item_wpa: (0..g.items.len()).map(|slot| delta(g, at + slot, window)).collect(),
                    boots_wpa: g.boots.as_ref().map(|_| delta(g, at + SLOTS - 1, window)),
                    swap_wpa: g.swaps.iter().map(|c| g.win - c.p.as_ref().map(|p| p[f]).unwrap_or(base)).collect(),
                    item_w0: (0..g.items.len()).map(|slot| item_p[at + slot]).collect(),
                    boots_w0: g.boots.as_ref().map(|_| item_p[at + SLOTS - 1]),
                }
            })
            .collect()
    };
    let scored = |list: &[Arc<PreparedGame>], f: usize, expected: Option<&[f64]>| -> Vec<ScoredGame> { scored_as(list, f, expected, use_window.load(Ordering::Relaxed)) };
    // The game-state model's chance before a decision (the Brier score's starting point): with the window, an item's
    // ΔW no longer says it, so it is read from the purchase's chance.
    let p_state_as = |s: &ScoredGame, f: usize, key: &str, wpa: f64, window: bool| -> f64 {
        let slot = match key {
            "core1" => Some(0),
            "core2" => Some(1),
            "core3" => Some(2),
            "boots" => Some(SLOTS - 1),
            _ => None,
        };
        match slot {
            Some(k) if window => item_p[(s.g.ix * nf + f) * SLOTS + k],
            _ => s.g.win - wpa,
        }
    };
    let p_state = |s: &ScoredGame, f: usize, key: &str, wpa: f64| -> f64 { p_state_as(s, f, key, wpa, use_window.load(Ordering::Relaxed)) };
    let folds: Vec<(Vec<Arc<PreparedGame>>, Vec<Arc<PreparedGame>>)> = (0..nf)
        .map(|f| {
            (
                games.iter().filter(|g| fold_of(g) != f).cloned().collect(),
                games.iter().filter(|g| fold_of(g) == f).cloned().collect(),
            )
        })
        .collect();

    // A quick update's bookkeeping.
    let base_in = input.base.clone();
    let salt = up.as_ref().map(|u| u.salt.clone()).or(input.salt.clone()).unwrap_or_default();
    let pilot_key = |p: &str| -> String { radix36(base_hash(&salt, p)) };
    let fresh: Option<HashSet<String>> = up.as_ref().map(|u| {
        let known: HashSet<u64> = u.known.iter().map(|x| *x as u64).collect();
        games.iter().filter(|g| !known.contains(&(base_hash(&salt, &g.match_id) as u64))).map(|g| g.match_id.clone()).collect()
    });
    let is_fresh = |g: &PreparedGame| fresh.as_ref().map(|f| f.contains(&g.match_id)).unwrap_or(true);
    let missing = match (&up, &fresh) {
        (Some(u), Some(fr)) => u.known.len() as f64 - (games.len() as f64 - fr.len() as f64),
        _ => 0.0,
    };
    let changed_games = match (&up, &fresh) {
        (Some(u), Some(fr)) => {
            let print_of: HashMap<u64, f64> = u.known.iter().zip(u.prints.iter()).map(|(h, p)| (*h as u64, *p)).collect();
            games.iter().filter(|g| !fr.contains(&g.match_id)).filter(|g| print_of.get(&(base_hash(&salt, &g.match_id) as u64)).copied() != Some(game_print(&salt, g))).count() as f64
        }
        _ => 0.0,
    };
    let idle = |f: usize| fresh.is_some() && !folds[f].1.iter().any(|g| is_fresh(g));
    let detail_at = |list: &[ScoredGame]| -> Vec<usize> {
        if fresh.is_some() {
            list.iter().enumerate().filter(|(_, s)| is_fresh(&s.g)).map(|(i, _)| i).collect()
        } else {
            strided(list.len(), 1)
        }
    };
    let grid_stride = 1usize.max((held_out_games as f64 / opts.max_grid_games as f64).ceil() as usize);
    let subsample_at = |list: &[ScoredGame]| -> Vec<usize> {
        match (&fresh, &up) {
            (Some(_), Some(u)) => list
                .iter()
                .enumerate()
                .filter(|(_, s)| is_fresh(&s.g) && (base_hash(&salt, &format!("sample:{}", s.g.match_id)) as u64) % u.grid_stride as u64 == 0)
                .map(|(i, _)| i)
                .collect(),
            _ => strided(list.len(), grid_stride),
        }
    };
    let merge_in = |b: Option<&GroupedOutcomes>, m: GroupedOutcomes| -> GroupedOutcomes {
        if up.is_some() {
            match b {
                Some(b) => merge_outcomes(b, &m, &pilot_key),
                None => GroupedOutcomes { pilots: m.pilots.iter().map(|p| pilot_key(p)).collect(), ..m },
            }
        } else {
            m
        }
    };
    let for_base = |g: &GroupedOutcomes| -> GroupedOutcomes {
        if up.is_some() {
            g.clone()
        } else {
            GroupedOutcomes { pilots: g.pilots.iter().map(|p| pilot_key(p)).collect(), ..g.clone() }
        }
    };
    let held_out_label = || match &fresh {
        Some(f) => format!("{} new held-out games", js::thousands(f.len() as f64)),
        None => format!("all {} held-out games", js::thousands(held_out_games as f64)),
    };
    let mut next = EvalBaseData { salt: salt.clone(), ..Default::default() };

    let draft_query = |g: &PreparedGame| DraftQuery { lane: g.lane, opponent: g.opponent, enemies: g.enemies.clone(), allies: g.allies.clone() };

    // Lane baselines per fold.
    let baselines: Vec<FoldBaselines> = (0..nf)
        .map(|f| {
            let train = &folds[f].0;
            let src: Vec<&Arc<PreparedGame>> = train.iter().filter(|g| opts.build_sources.contains(&g.source)).collect();
            let train_rows: Vec<Arc<IreliaGameRow>> = src.iter().filter_map(|g| rows_by_id.get(&g.match_id).cloned()).collect();
            let mut win_rates: HashMap<String, (f64, f64)> = HashMap::new();
            let mut lane_rate: HashMap<&'static str, (f64, f64)> = HashMap::new();
            let list: Vec<Arc<PreparedGame>> = src.iter().map(|g| (*g).clone()).collect();
            for s in scored(&list, f, None) {
                let lr = lane_rate.entry(s.g.lane).or_insert((0.0, 0.0));
                lr.0 += s.g.win;
                lr.1 += 1.0;
                for (key, option, _) in actual(&s) {
                    let e = win_rates.entry(format!("{}:{}:{}", s.g.lane, key, option)).or_insert((0.0, 0.0));
                    e.0 += s.g.win;
                    e.1 += 1.0;
                }
            }
            FoldBaselines {
                baselines: Mutex::new(HashMap::new()),
                train_rows: Mutex::new(Some(train_rows)),
                validated: Mutex::new(HashMap::new()),
                win_rates,
                lane_rate,
            }
        })
        .collect();
    let baseline_for = |f: usize, lane: &'static str, opponent: Option<i64>| -> Arc<BuildRecommendation> {
        let key = format!("{}:{}", lane, opponent.map(|o| o.to_string()).unwrap_or_else(|| "null".into()));
        if let Some(b) = baselines[f].baselines.lock().get(&key) {
            return b.clone();
        }
        let rows = baselines[f].train_rows.lock().clone().expect("stored rows");
        let rp = region_patch.clone();
        let b = Arc::new(recommend(&rows, &data, &excl, opponent, &opts.engine, Some(lane), &|p| rp(p), Some(&baselines[f].validated)));
        baselines[f].baselines.lock().insert(key, b.clone());
        b
    };
    let baselines_made: Mutex<HashSet<usize>> = Mutex::new(HashSet::new());
    let baselines_of = |f: usize| {
        if baselines_made.lock().contains(&f) {
            return;
        }
        let mut pairs: Vec<(&'static str, Option<i64>)> = Vec::new();
        for g in &folds[f].1 {
            if is_fresh(g) && !pairs.contains(&(g.lane, g.opponent)) {
                pairs.push((g.lane, g.opponent));
            }
        }
        team.for_each(pairs.len(), |j| baseline_for(f, pairs[j].0, pairs[j].1), |_, _| {}, &|_, _| {});
        baselines[f].validated.lock().clear();
        baselines_made.lock().insert(f);
    };
    let fold_model_as = |f: usize, window: bool| -> (DraftModel, Weighted, Vec<ScoredGame>) {
        let model = build_draft_model(scored_as(&folds[f].0, f, None, window), state_models[f].clone(), profiles.clone());
        let weighted = weigh_games(&model, &data, &region_patch, &opts.weights);
        let test = scored_as(&folds[f].1, f, None, window);
        (model, weighted, test)
    };
    let fold_model = |f: usize| fold_model_as(f, use_window.load(Ordering::Relaxed));
    let scored_set = set_of(&EVAL_DECISIONS);

    // ---- the bandwidth / prior grid
    let settings_grid: Vec<(f64, f64)> = opts.bandwidths.iter().flat_map(|h| opts.ks.iter().map(move |k| (*h, *k))).collect();
    let mut grid: Vec<GridRow> = match (&up, &base_in) {
        (Some(_), Some(b)) => b.report.as_ref().map(|r| r.grid.clone()).unwrap_or_default(),
        _ => vec![],
    };
    let chosen: Chosen = if let Some(u) = &up {
        u.chosen
    } else {
        let mut grid_outcomes: Vec<Vec<Outcome>> = settings_grid.iter().map(|_| vec![]).collect();
        for f in 0..nf {
            step(&format!("Comparing builds on fold {} of {} ({} settings)", f + 1, nf, settings_grid.len()));
            if f == 0 {
                for ff in 0..nf {
                    baselines_of(ff);
                }
                for b in &baselines {
                    *b.train_rows.lock() = None;
                }
            }
            let (model, weighted, test) = fold_model(f);
            let cache = DraftCache::default();
            let at = strided(test.len(), grid_stride);
            let progress = part_for(current.borrow().clone(), step_no.get());
            team.for_each(
                at.len(),
                |j| {
                    let s = &test[at[j]];
                    let baseline = baseline_for(f, s.g.lane, s.g.opponent);
                    let q = draft_query(&s.g);
                    let mut pds: Vec<Picks> = vec![HashMap::new(); settings_grid.len()];
                    for h in &opts.bandwidths {
                        let memo = RefCell::new(super::estimate::Memo::default());
                        for (gi, (sh, sk)) in settings_grid.iter().enumerate() {
                            if sh.to_bits() != h.to_bits() {
                                continue;
                            }
                            let settings = DraftSettings { weights: opts.weights.clone(), estimate: EstimateOptions { h: *h, k: *sk, ..ESTIMATE_DEFAULTS }, item_k: None };
                            let rec = recommend_draft(&model, &q, &data, &excl, &region_patch, &baseline, &settings, &DraftExtra {
                                reasons: Some(false),
                                detail: false,
                                weighted: Some(&weighted),
                                cache: Some(&cache),
                                memo: Some(&memo),
                                decisions: Some(&scored_set),
                            });
                            pds[gi] = picks_of(&rec.build);
                        }
                    }
                    pds
                },
                |j, pds| {
                    let s = &test[at[j]];
                    let pb = picks_of(&baseline_for(f, s.g.lane, s.g.opponent));
                    let act = actual(s);
                    let pilot: Arc<str> = Arc::from(s.g.pilot.as_str());
                    for (gi, pd) in pds.iter().enumerate() {
                        for (key, option, wpa) in &act {
                            grid_outcomes[gi].push(Outcome {
                                pilot: pilot.clone(),
                                key,
                                wpa: *wpa,
                                followed_draft: pd.get(key) == Some(option),
                                followed_baseline: pb.get(key) == Some(option),
                                w: None,
                            });
                        }
                    }
                },
                &progress,
            );
        }
        let mut best: Option<(f64, f64, f64)> = None;
        for (i, (h, k)) in settings_grid.iter().enumerate() {
            let l = lifts(&group_outcomes(&grid_outcomes[i], None), None, 10.0, None);
            grid_outcomes[i].clear();
            grid.push(GridRow { h: *h, k: *k, draft: l.draft, baseline: l.baseline, delta: l.draft - l.baseline });
            log(&format!(
                "h={} k={}: lift draft {} pts, baseline {} pts",
                js_num_or_infinity(*h),
                js::num_to_string(*k),
                js::to_fixed(l.draft * 100.0, 2),
                js::to_fixed(l.baseline * 100.0, 2)
            ));
            if best.map(|b| l.draft - l.baseline > b.2).unwrap_or(true) {
                best = Some((*h, *k, l.draft - l.baseline));
            }
        }
        Chosen { h: best.map(|b| b.0).unwrap_or(ESTIMATE_DEFAULTS.h), k: best.map(|b| b.1).unwrap_or(ESTIMATE_DEFAULTS.k) }
    };
    if up.is_some() {
        for f in 0..nf {
            if !idle(f) {
                baselines_of(f);
            }
        }
    }

    // ---- the final comparison
    step(&format!("Comparing builds on {}", held_out_label()));
    let want_classic = opts.memory.is_some() || opts.scoring || opts.tuning.is_some();
    // One pass over the held-out games with one ΔW definition: the outcomes, the Brier sums, each game's picks and
    // estimates. With the effective-window candidate on there are two passes (today's, then the window's).
    type Pass = (Vec<Outcome>, [f64; 3], HashMap<String, Picks>, HashMap<String, HashMap<&'static str, f64>>);
    let passes = if window_on { 2 } else { 1 };
    let final_pass = |window: bool, pass: usize| -> Pass {
        let mut final_outcomes: Vec<Outcome> = Vec::new();
        let mut final_brier = [0.0f64; 3];
        let mut classic_picks: HashMap<String, Picks> = HashMap::new();
        let mut classic_thetas: HashMap<String, HashMap<&'static str, f64>> = HashMap::new();
        for f in 0..nf {
            if idle(f) {
                continue;
            }
            let (model, weighted, test) = fold_model_as(f, window);
            let cache = DraftCache::default();
            let settings = DraftSettings { weights: opts.weights.clone(), estimate: EstimateOptions { h: chosen.h, k: chosen.k, ..ESTIMATE_DEFAULTS }, item_k: None };
            let at = detail_at(&test);
            let progress = fold_part_for(current.borrow().clone(), step_no.get(), pass * nf + f, passes * nf);
            team.for_each(
                at.len(),
                |j| {
                    let s = &test[at[j]];
                    let baseline = baseline_for(f, s.g.lane, s.g.opponent);
                    let rec = recommend_draft(&model, &draft_query(&s.g), &data, &excl, &region_patch, &baseline, &settings, &DraftExtra {
                        reasons: Some(false),
                        detail: true,
                        weighted: Some(&weighted),
                        cache: Some(&cache),
                        memo: None,
                        decisions: Some(&scored_set),
                    });
                    let mut theta: HashMap<&'static str, f64> = HashMap::new();
                    for (key, option, _) in actual(s) {
                        theta.insert(key, rec.estimates.as_ref().and_then(|e| e.get(key)).and_then(|m| m.get(&option)).copied().unwrap_or(0.0));
                    }
                    (picks_of(&rec.build), theta)
                },
                |j, (pd, estimate)| {
                    let s = &test[at[j]];
                    let pb = picks_of(&baseline_for(f, s.g.lane, s.g.opponent));
                    let mut theta: HashMap<&'static str, f64> = HashMap::new();
                    let pilot: Arc<str> = Arc::from(s.g.pilot.as_str());
                    for (key, option, wpa) in actual(s) {
                        final_outcomes.push(Outcome {
                            pilot: pilot.clone(),
                            key,
                            wpa,
                            followed_draft: pd.get(key) == Some(&option),
                            followed_baseline: pb.get(key) == Some(&option),
                            w: None,
                        });
                        if opts.scoring && PREGAME.contains(&key) {
                            theta.insert(key, estimate.get(key).copied().unwrap_or(0.0));
                        }
                        if BRIER_KEYS.contains(&key) {
                            let p_state = p_state_as(s, f, key, wpa, window);
                            final_brier[0] += sq(clip(p_state + estimate.get(key).copied().unwrap_or(0.0)) - s.g.win);
                            final_brier[1] += sq(clip(baselines[f].base_rate(s.g.lane, key, &option)) - s.g.win);
                            final_brier[2] += 1.0;
                        }
                    }
                    if want_classic {
                        classic_picks.insert(s.g.match_id.clone(), pd);
                    }
                    if opts.scoring {
                        classic_thetas.insert(s.g.match_id.clone(), theta);
                    }
                },
                &progress,
            );
        }
        (final_outcomes, final_brier, classic_picks, classic_thetas)
    };
    let (final_outcomes, mut final_brier, mut classic_picks, mut classic_thetas) = final_pass(use_window.load(Ordering::Relaxed), 0);
    let alt_pass = if window_on { Some(final_pass(true, 1)) } else { None };
    let mut grouped = merge_in(base_in.as_ref().and_then(|b| b.final_.as_ref().map(|x| &x.0)), group_outcomes(&final_outcomes, None));
    drop(final_outcomes);
    if let Some(b) = base_in.as_ref().and_then(|b| b.final_.as_ref()) {
        final_brier = [b.1[0] + final_brier[0], b.1[1] + final_brier[1], b.1[2] + final_brier[2]];
    }
    let mut l = lifts(&grouped, None, 10.0, None);

    // The pilot bootstrap of the lift difference.
    let interval = |g: &GroupedOutcomes, only: Option<&[&str]>| -> (f64, f64) {
        let pilot_count = g.pilots.len();
        let n = if pilot_count > 1 { opts.bootstrap } else { 0 };
        let seed0 = to_uint32(opts.seed * 2654435761.0);
        let mut deltas: Vec<f64> = vec![0.0; n];
        team.for_each(
            n,
            |b| {
                let mut seed = (seed0 as i64 + crate::jsmath::imul(to_int32((b * pilot_count) as f64), 0x6d2b79f5) as i64).rem_euclid(1 << 32) as u32;
                let mut rand = || {
                    seed = seed.wrapping_add(0x6d2b79f5);
                    let mut t = seed as i32;
                    t = crate::jsmath::imul(t ^ ((t as u32) >> 15) as i32, t | 1);
                    t ^= t.wrapping_add(crate::jsmath::imul(t ^ ((t as u32) >> 7) as i32, t | 61));
                    ((t ^ ((t as u32) >> 14) as i32) as u32) as f64 / 4294967296.0
                };
                let mut w = vec![0.0; pilot_count];
                for _ in 0..pilot_count {
                    w[(rand() * pilot_count as f64).floor() as usize] += 1.0;
                }
                let r = lifts(g, Some(&w), 10.0, only);
                r.draft - r.baseline
            },
            |b, d| deltas[b] = d,
            &|_, _| {},
        );
        deltas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let q = |p: f64| if deltas.is_empty() { f64::NAN } else { deltas[(deltas.len() - 1).min((p * deltas.len() as f64).floor() as usize)] };
        (q(0.025), q(0.975))
    };
    step("Confidence interval");
    let (mut low, mut high) = interval(&grouped, None);
    // The effective window against today's ΔW: kept when its lift and the lower end of its interval are both higher.
    let mut window_report: Option<WindowReport> = if window_on {
        None
    } else {
        base_in.as_ref().filter(|_| up.is_some()).and_then(|b| b.report.as_ref()).and_then(|r| r.window.clone())
    };
    if let Some((alt_outcomes, alt_brier, alt_picks, alt_thetas)) = alt_pass {
        let g_alt = group_outcomes(&alt_outcomes, None);
        drop(alt_outcomes);
        let l_alt = lifts(&g_alt, None, 10.0, None);
        let (lo, hi) = interval(&g_alt, None);
        let brier_of = |b: &[f64; 3]| if b[2] != 0.0 { b[0] / b[2] } else { f64::NAN };
        let today = WindowLift { draft: l.draft, baseline: l.baseline, delta: l.draft - l.baseline, low, high, brier: brier_of(&final_brier) };
        let window = WindowLift { draft: l_alt.draft, baseline: l_alt.baseline, delta: l_alt.draft - l_alt.baseline, low: lo, high: hi, brier: brier_of(&alt_brier) };
        let keep = window.delta > today.delta && window.low > today.low;
        let gate_reason = if keep {
            format!("the lift rises {} and its 95 % interval's lower end {}", pts(window.delta - today.delta), pts(window.low - today.low))
        } else if !(window.delta > today.delta) {
            format!("the lift moves {} (needs to rise)", pts(window.delta - today.delta))
        } else {
            format!("the 95 % interval's lower end moves {} (needs to rise)", pts(window.low - today.low))
        };
        log(&format!(
            "effective window: today's ΔW (the result minus the chance before) lift {} (95 % CI {} to {}), Brier {}; ΔW ending at a sale or upgrade ({} held-out item decisions end early) lift {} (95 % CI {} to {}), Brier {}; kept: {} ({})",
            pts(today.delta),
            pts(today.low),
            pts(today.high),
            js::to_fixed(today.brier, 4),
            js::num_to_string(window_items),
            pts(window.delta),
            pts(window.low),
            pts(window.high),
            js::to_fixed(window.brier, 4),
            if keep { "the effective window" } else { "today's ΔW" },
            gate_reason
        ));
        if keep {
            use_window.store(true, Ordering::Relaxed);
            grouped = g_alt;
            l = l_alt;
            final_brier = alt_brier;
            classic_picks = alt_picks;
            classic_thetas = alt_thetas;
            (low, high) = (lo, hi);
        }
        window_report = Some(WindowReport {
            today,
            window,
            items: window_items,
            kept: if keep { "window" } else { "today" }.into(),
            gate: if keep { "passed" } else { "failed" }.into(),
            gate_reason,
        });
    }
    next.final_ = Some((for_base(&grouped), final_brier));
    let lift = LiftReport { draft: l.draft, baseline: l.baseline, delta: l.draft - l.baseline, low, high, within_pilot: Some(within_pilot_of(&grouped)) };
    let brier = BrierReport {
        draft: if final_brier[2] != 0.0 { final_brier[0] / final_brier[2] } else { f64::NAN },
        baseline: if final_brier[2] != 0.0 { final_brier[1] / final_brier[2] } else { f64::NAN },
        n: final_brier[2],
    };
    let test_games: f64 = match (&up, &fresh) {
        (Some(u), Some(fr)) => (u.known.len() + fr.len()) as f64,
        _ => held_out_games as f64,
    };

    let ctx = Ctx {
        sh: Shared { data: &data, excl: &excl, region_patch: &region_patch, age_of: &age_of, baseline_for: &baseline_for, up: up.as_ref() },
        opts,
        data: &data,
        excl: &excl,
        region_patch: &region_patch,
        team: &team,
        folds: &folds,
        older_games: &older_games,
        games: &games,
        chosen,
        test_games,
        fold_of: &fold_of,
        age_of: &age_of,
        scored: &scored,
        p_state: &p_state,
        baseline_for: &baseline_for,
        idle: &idle,
        detail_at: &detail_at,
        subsample_at: &subsample_at,
        is_fresh: &is_fresh,
        held_out_label: &held_out_label,
        step: &step,
        log,
        interval: &interval,
        merge_in: &merge_in,
        for_base: &for_base,
        up: up.as_ref(),
        base: base_in.as_ref(),
        state_models: &state_models,
        profiles: &profiles,
        brier_draft: brier.draft,
        salt: &salt,
    };
    let memory = match &opts.memory {
        Some(mo) => Some(evaluate_memory(&ctx, mo, &classic_picks, &mut next)?),
        None => None,
    };
    let scoring = if opts.scoring { Some(evaluate_scoring(&ctx, &classic_picks, &classic_thetas, &mut next)?) } else { None };
    let tuning = match &opts.tuning {
        Some(t) => Some(evaluate_tuning(&ctx, t, if want_classic { Some(&classic_picks) } else { None }, &mut next)?),
        None => None,
    };
    let swaps = if opts.swaps { Some(evaluate_swaps(&ctx, &mut next)?) } else { None };
    drop(classic_picks);

    // ---- Live stability
    let mut stability: Option<Stability> = if up.is_some() { base_in.as_ref().and_then(|b| b.report.as_ref()).and_then(|r| r.stability.clone()) } else { None };
    if opts.live && up.is_none() {
        let settings = EstimateOptions { h: chosen.h, k: chosen.k, ..ESTIMATE_DEFAULTS };
        step(&format!("Replaying {} held-out games minute by minute", js::thousands(replay_ids.len() as f64)));
        let (mut flips, mut slot_changes, mut replayed, mut minutes) = (0.0, 0.0, 0.0, 0.0);
        for f in 0..nf {
            let (model, weighted, test) = fold_model(f);
            let obs_by_slot: Mutex<HashMap<usize, (super::estimate::ObsRef, Arc<Contexts>)>> = Mutex::new(HashMap::new());
            let obs_for = |slot: usize| -> (super::estimate::ObsRef, Arc<Contexts>) {
                if let Some(o) = obs_by_slot.lock().get(&slot) {
                    return o.clone();
                }
                let mut at: Vec<usize> = Vec::new();
                let mut list: Vec<Observation> = Vec::new();
                for wg in &weighted.list {
                    let s = &model.scored[wg.i];
                    let Some(c) = s.g.items.get(slot) else { continue };
                    let w = wg.base * weighted.patch(&s.g, &[c.item as i64]);
                    if w <= 0.0 {
                        continue;
                    }
                    at.push(model.item_row[wg.i] as usize + slot);
                    list.push(Observation {
                        option: super::draft::opt_str(c.item),
                        wpa: s.item_wpa[slot],
                        win: s.g.win,
                        start_win: s.item_w0[slot],
                        w,
                        lane: s.g.lane,
                        opponent: s.g.opponent,
                        pilot: Arc::from(s.g.pilot.as_str()),
                        z: Z::Row(model.z_items.clone(), model.item_row[wg.i] as usize + slot),
                        row: Some(at.len() - 1),
                        age: Some(wg.age),
                    });
                }
                let obs = observed(list, &opts.weights);
                let zi = &model.z_items;
                let zs = Contexts::of(at.len(), zi.d, |out, r, to| out[to..to + zi.d].copy_from_slice(&zi.flat[at[r] * zi.d..at[r] * zi.d + zi.d]));
                let v = (obs, Arc::new(zs));
                obs_by_slot.lock().insert(slot, v.clone());
                v
            };
            let at = strided(test.len(), live_stride);
            let progress = fold_part_for(current.borrow().clone(), step_no.get(), f, nf);
            team.for_each(
                at.len(),
                |j| -> Option<(f64, f64, f64)> {
                    let s = &test[at[j]];
                    let row = rows_by_id.get(&s.g.match_id)?;
                    let raw = match &replay_timelines {
                        Some(t) => t.get(&s.g.match_id).cloned(),
                        None => row.timeline.clone(),
                    }?;
                    let mut r2 = (**row).clone();
                    r2.timeline = Some(raw);
                    let ps: Vec<&ParticipantLite> = parts_by_match.get(&s.g.match_id).map(|v| v.iter().collect()).unwrap_or_default();
                    let tl = game_timeline(&r2, &ps)?;
                    let times: Vec<f64> = tl.cap.t.iter().copied().filter(|t| *t >= 3.0 * 60_000.0).collect();
                    let moments = moments_at(&tl, &times, &profiles, &data);
                    let z_draft = standardise(&s.g.draft, &model.draft_scale);
                    let mut stable = NextStability::new(STABILITY_DEFAULTS);
                    let mut last_slot: i64 = -1;
                    let (mut mins, mut fl, mut sc) = (0.0, 0.0, 0.0);
                    for mo in &moments {
                        let slot = mo.owned.iter().filter(|id| is_completed_item(data.items.get(&(**id as i64)))).count();
                        if slot >= 6 {
                            break;
                        }
                        mins += 1.0;
                        let (obs, zs) = obs_for(slot);
                        let mut z = z_draft.clone();
                        z.extend(standardise(&mo.live, &model.live_scale));
                        let sims = if chosen.h.is_finite() { Some(Sims { h: chosen.h, values: Arc::new(similarities(&zs, &z, chosen.h)), groups: None }) } else { None };
                        let est = estimate_decision(&obs, &Query { lane: s.g.lane, opponent: s.g.opponent, z, sims, memo: None }, &settings, None, None);
                        let owned: Vec<i64> = mo.owned.iter().map(|x| *x as i64).collect();
                        let options: Vec<super::estimate::OptionEstimate> = est
                            .options
                            .into_iter()
                            .filter(|o| {
                                let id = js::to_number(&serde_json::Value::String(o.option.to_string()));
                                id.fract() == 0.0 && is_purchasable_item(data.items.get(&(id as i64))) && excl.blocker(&owned, id as i64).is_none()
                            })
                            .collect();
                        let changed = stable.step(slot as i64, &options, None);
                        if changed && slot as i64 == last_slot {
                            fl += 1.0;
                        }
                        if slot as i64 != last_slot {
                            if last_slot >= 0 {
                                sc += 1.0;
                            }
                            last_slot = slot as i64;
                        }
                    }
                    Some((mins, fl, sc))
                },
                |_, r| {
                    if let Some((m, fl, sc)) = r {
                        replayed += 1.0;
                        minutes += m;
                        flips += fl;
                        slot_changes += sc;
                    }
                },
                &progress,
            );
            let _ = &weighted;
        }
        stability = Some(Stability {
            games: replayed,
            minutes,
            flips_per_game: if replayed != 0.0 { flips / replayed } else { 0.0 },
            slot_changes_per_game: if replayed != 0.0 { slot_changes / replayed } else { 0.0 },
        });
    }

    // ---- Yours
    let linked: Vec<&RecLogRow> = input.rec_log.iter().filter(|r| r.match_id.as_deref().map(|m| !m.is_empty()).unwrap_or(false) && r.bought.as_deref().map(|b| !b.is_empty()).unwrap_or(false) && r.phase == "draft").collect();
    let yours = if linked.is_empty() {
        None
    } else {
        let (mut fo, mut ot) = (WinCount { games: 0.0, wins: 0.0 }, WinCount { games: 0.0, wins: 0.0 });
        for r in &linked {
            let shown: serde_json::Value = serde_json::from_str(&r.shown).unwrap_or_default();
            let bought: serde_json::Value = serde_json::from_str(r.bought.as_deref().unwrap_or("null")).unwrap_or_default();
            let core0 = shown["core"].as_array().and_then(|a| a.first()).cloned();
            let did = shown["keystone"] == bought["keystone"]
                && core0.is_some()
                && bought["purchases"].as_array().map(|p| p.contains(core0.as_ref().unwrap())).unwrap_or(false);
            let g = if did { &mut fo } else { &mut ot };
            g.games += 1.0;
            g.wins += r.win.unwrap_or(0) as f64;
        }
        Some(Yours { linked: linked.len() as f64, followed: fo, other: ot })
    };

    let gate_reason = if test_games < opts.min_games as f64 {
        format!("only {} held-out games (needs {})", js::num_to_string(test_games), opts.min_games)
    } else if !(lift.low > 0.0) {
        format!("the lift over the baseline's 95 % interval reaches {} pts (needs > 0)", js::to_fixed(lift.low * 100.0, 2))
    } else if !(brier.draft <= brier.baseline) {
        format!("Brier {} is worse than the baseline's {}", js::to_fixed(brier.draft, 4), js::to_fixed(brier.baseline, 4))
    } else {
        "lift above zero with 95 % confidence, Brier no worse".into()
    };
    let passed = test_games >= opts.min_games as f64 && lift.low > 0.0 && brier.draft <= brier.baseline;
    let report = EvalReport {
        evaluated_at: crate::util::now_ms() as f64,
        games: games.len() as f64,
        pilots: pilots_n as f64,
        folds: nf as f64,
        state: state_report,
        grid,
        chosen,
        decisions: l.per,
        lift,
        brier,
        stability,
        yours,
        memory,
        scoring,
        tuning,
        swaps,
        gate: if passed { "passed".into() } else { "failed".into() },
        gate_reason,
        update: fresh.as_ref().map(|f| UpdateInfo { full_at: 0.0, new_games: f.len() as f64, reason: String::new(), missing: Some(missing), changed: Some(changed_games), close: None }),
        grouping: grouping.map(|g| GroupingJson { games: g.games as f64, groups: g.groups as f64, spread: g.spread, ms: g.ms }),
        window: window_report,
    };
    let list: Vec<&Arc<PreparedGame>> = games.iter().filter(|g| is_fresh(g)).collect();
    let measured: Vec<f64> = list.iter().map(|g| base_hash(&salt, &g.match_id)).collect();
    let prints: Vec<f64> = list.iter().map(|g| game_print(&salt, g)).collect();
    next.known = up.as_ref().map(|u| u.known.clone()).unwrap_or_default().into_iter().chain(measured).collect();
    next.prints = up.as_ref().map(|u| u.prints.clone()).unwrap_or_default().into_iter().chain(prints).collect();
    next.report = Some(report.clone());
    Ok((report, Some(next)))
}

fn js_num_or_infinity(x: f64) -> String {
    js::num_to_string(x)
}

/// `x >>> 0`.
pub fn to_uint32(x: f64) -> u32 {
    if !x.is_finite() {
        return 0;
    }
    (x.trunc().rem_euclid(4294967296.0)) as u32
}
/// ToInt32.
pub fn to_int32(x: f64) -> i32 {
    to_uint32(x) as i32
}

/// `n.toString(36)` for a non-negative integer.
pub fn radix36(x: f64) -> String {
    let mut n = x as u64;
    if n == 0 {
        return "0".into();
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while n > 0 {
        out.push(digits[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

// ------------------------------------------------------------------------------------------------- the later gates

type ScoredFn<'a> = dyn Fn(&[Arc<PreparedGame>], usize, Option<&[f64]>) -> Vec<ScoredGame> + Sync + 'a;

/// What the team's work closures read (every field shareable between threads).
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct Shared<'a> {
    data: &'a Arc<StaticData>,
    excl: &'a Arc<ExclusivityIndex>,
    region_patch: &'a RegionPatchFn,
    age_of: &'a (dyn Fn(&PreparedGame) -> f64 + Sync),
    baseline_for: &'a (dyn Fn(usize, &'static str, Option<i64>) -> Arc<BuildRecommendation> + Sync),
    up: Option<&'a EvalUpdate>,
}

fn query_of(g: &PreparedGame) -> DraftQuery {
    DraftQuery { lane: g.lane, opponent: g.opponent, enemies: g.enemies.clone(), allies: g.allies.clone() }
}

#[allow(dead_code)]
struct Ctx<'a> {
    sh: Shared<'a>,
    opts: &'a EvalOptions,
    data: &'a Arc<StaticData>,
    excl: &'a Arc<ExclusivityIndex>,
    region_patch: &'a RegionPatchFn,
    team: &'a Team,
    folds: &'a [(Vec<Arc<PreparedGame>>, Vec<Arc<PreparedGame>>)],
    older_games: &'a [Arc<PreparedGame>],
    games: &'a [Arc<PreparedGame>],
    chosen: Chosen,
    test_games: f64,
    fold_of: &'a (dyn Fn(&PreparedGame) -> usize + Sync),
    age_of: &'a (dyn Fn(&PreparedGame) -> f64 + Sync),
    scored: &'a ScoredFn<'a>,
    /// The state model's chance before a decision (the Brier score's starting point), for the ΔW definition in use.
    p_state: &'a (dyn Fn(&ScoredGame, usize, &str, f64) -> f64 + Sync),
    baseline_for: &'a (dyn Fn(usize, &'static str, Option<i64>) -> Arc<BuildRecommendation> + Sync),
    idle: &'a dyn Fn(usize) -> bool,
    detail_at: &'a dyn Fn(&[ScoredGame]) -> Vec<usize>,
    subsample_at: &'a dyn Fn(&[ScoredGame]) -> Vec<usize>,
    is_fresh: &'a (dyn Fn(&PreparedGame) -> bool + Sync),
    held_out_label: &'a dyn Fn() -> String,
    step: &'a dyn Fn(&str),
    log: &'a (dyn Fn(&str) + Sync),
    interval: &'a dyn Fn(&GroupedOutcomes, Option<&[&str]>) -> (f64, f64),
    merge_in: &'a dyn Fn(Option<&GroupedOutcomes>, GroupedOutcomes) -> GroupedOutcomes,
    for_base: &'a dyn Fn(&GroupedOutcomes) -> GroupedOutcomes,
    up: Option<&'a EvalUpdate>,
    base: Option<&'a EvalBaseData>,
    state_models: &'a [Option<StateModel>],
    profiles: &'a Arc<ChampionProfiles>,
    brier_draft: f64,
    salt: &'a str,
}

impl Ctx<'_> {
    fn settings(&self, weights: WeightOptions) -> DraftSettings {
        DraftSettings { weights, estimate: EstimateOptions { h: self.chosen.h, k: self.chosen.k, ..ESTIMATE_DEFAULTS }, item_k: None }
    }
    fn model(&self, f: usize, list: &[Arc<PreparedGame>], expected: Option<&[f64]>) -> DraftModel {
        build_draft_model((self.scored)(list, f, expected), self.state_models[f].clone(), self.profiles.clone())
    }
    fn report_base(&self) -> Option<&EvalReport> {
        self.base.and_then(|b| b.report.as_ref())
    }
}

/// One fold's held-out games: the side built by `side` against `b`'s picks.
#[allow(clippy::too_many_arguments)]
fn compare_sides(
    ctx: &Ctx,
    f: usize,
    model: &DraftModel,
    settings: &DraftSettings,
    b: &HashMap<String, Picks>,
    test: &[ScoredGame],
    detail: bool,
    outcomes: &mut Vec<Outcome>,
    brier: &mut [f64; 2],
    only: &HashSet<&'static str>,
) {
    let weighted = weigh_games(model, ctx.data, ctx.region_patch, &settings.weights);
    let cache = DraftCache::default();
    let at = if detail { (ctx.detail_at)(test) } else { (ctx.subsample_at)(test) };
    let all = set_of(&EVAL_DECISIONS);
    let sh = ctx.sh;
    ctx.team.for_each(
        at.len(),
        |j| {
            let s = &test[at[j]];
            let baseline = (sh.baseline_for)(f, s.g.lane, s.g.opponent);
            let rec = recommend_draft(model, &query_of(&s.g), sh.data, sh.excl, sh.region_patch, &baseline, settings, &DraftExtra {
                reasons: Some(false),
                detail,
                weighted: Some(&weighted),
                cache: Some(&cache),
                memo: None,
                decisions: Some(only),
            });
            let mut est: HashMap<&'static str, f64> = HashMap::new();
            if detail {
                for (key, option, _) in actual(s) {
                    if BRIER_KEYS.contains(&key) {
                        est.insert(key, rec.estimates.as_ref().and_then(|e| e.get(key)).and_then(|m| m.get(&option)).copied().unwrap_or(0.0));
                    }
                }
            }
            (picks_of(&rec.build), est)
        },
        |j, (pa, est)| {
            let s = &test[at[j]];
            let empty = HashMap::new();
            let pb = b.get(&s.g.match_id).unwrap_or(&empty);
            let pilot: Arc<str> = Arc::from(s.g.pilot.as_str());
            for (key, option, wpa) in actual(s) {
                let tuned = if only.len() == all.len() || only.contains(key) { pa.get(key) } else { pb.get(key) };
                outcomes.push(Outcome {
                    pilot: pilot.clone(),
                    key,
                    wpa,
                    followed_draft: tuned == Some(&option),
                    followed_baseline: pb.get(key) == Some(&option),
                    w: None,
                });
                if detail && BRIER_KEYS.contains(&key) {
                    brier[0] += sq(clip((ctx.p_state)(s, f, key, wpa) + est.get(key).copied().unwrap_or(0.0)) - s.g.win);
                    brier[1] += 1.0;
                }
            }
        },
        &|_, _| {},
    );
}

fn evaluate_memory(ctx: &Ctx, mo: &MemoryOptions, classic_picks: &HashMap<String, Picks>, next: &mut EvalBaseData) -> Result<MemoryReport> {
    let opts = ctx.opts;
    let nf = opts.folds;
    let older = |f: usize| -> Vec<Arc<PreparedGame>> { ctx.older_games.iter().filter(|g| (ctx.fold_of)(g) != f).cloned().collect() };
    let older_count = ctx.older_games.len() as f64
        + ctx.games.iter().filter(|g| {
            let a = (ctx.age_of)(g);
            a >= 2.0 && a < mo.patches
        }).count() as f64;
    let with_memory = |decay: f64| WeightOptions { memory: Some(Memory { decay, patches: mo.patches }), ..opts.weights.clone() };
    let none = |b: f64| MemoryFull { memory: 0.0, classic: 0.0, delta: 0.0, low: f64::NAN, high: f64::NAN, brier: b, classic_brier: b };
    let report = |decay: f64, grid: Vec<MemoryGridRow>, full: MemoryFull, early: MemoryEarly, gate_reason: String, passed: bool| MemoryReport {
        patches: mo.patches,
        older_games: older_count,
        decay,
        grid,
        full,
        early,
        gate: if passed { "passed".into() } else { "failed".into() },
        gate_reason,
    };
    if older_count < mo.min_older_games || ctx.test_games < opts.min_games as f64 {
        let n = if ctx.up.is_some() { 2 } else { nf + 2 };
        for _ in 0..n {
            (ctx.step)("Patch memory: not enough games to compare");
        }
        let reason = if older_count < mo.min_older_games {
            format!("only {} games from older patches (needs {})", js::num_to_string(older_count), js::num_to_string(mo.min_older_games))
        } else {
            format!("only {} held-out games (needs {})", js::num_to_string(ctx.test_games), opts.min_games)
        };
        return Ok(report(
            mo.decays.first().copied().unwrap_or(0.0),
            vec![],
            none(f64::NAN),
            MemoryEarly { memory: 0.0, classic: 0.0, delta: 0.0, low: f64::NAN, high: f64::NAN, current_games: 0.0, kept_games: 0.0 },
            reason,
            false,
        ));
    }
    (ctx.log)(&format!("patch memory: {} games from older patches (within {} patches)", js::num_to_string(older_count), js::num_to_string(mo.patches)));
    let scored_set = set_of(&EVAL_DECISIONS);
    let (grid, decay) = if let Some(u) = ctx.up {
        (ctx.report_base().and_then(|r| r.memory.as_ref()).map(|m| m.grid.clone()).unwrap_or_default(), u.decay.unwrap_or(mo.decays[0]))
    } else {
        let mut grid_outcomes: Vec<Vec<Outcome>> = mo.decays.iter().map(|_| vec![]).collect();
        for f in 0..nf {
            (ctx.step)(&format!("Patch memory: fold {} of {} ({} fades per patch)", f + 1, nf, mo.decays.len()));
            let train: Vec<Arc<PreparedGame>> = ctx.folds[f].0.iter().cloned().chain(older(f)).collect();
            let model = ctx.model(f, &train, None);
            let test = (ctx.scored)(&ctx.folds[f].1, f, None);
            for (i, decay) in mo.decays.iter().enumerate() {
                let mut b = [0.0; 2];
                compare_sides(ctx, f, &model, &ctx.settings(with_memory(*decay)), classic_picks, &test, false, &mut grid_outcomes[i], &mut b, &scored_set);
            }
        }
        let grid: Vec<MemoryGridRow> = mo
            .decays
            .iter()
            .enumerate()
            .map(|(i, decay)| {
                let r = lifts(&group_outcomes(&grid_outcomes[i], None), None, 10.0, None);
                (ctx.log)(&format!(
                    "patch memory, fade {} per patch: lift {} pts, two patches {} pts",
                    js::num_to_string(*decay),
                    js::to_fixed(r.draft * 100.0, 2),
                    js::to_fixed(r.baseline * 100.0, 2)
                ));
                MemoryGridRow { decay: *decay, memory: r.draft, classic: r.baseline, delta: r.draft - r.baseline }
            })
            .collect();
        let mut best = &grid[0];
        for g in &grid {
            if g.delta > best.delta {
                best = g;
            }
        }
        let d = best.decay;
        (grid, d)
    };

    (ctx.step)(&format!("Patch memory on {}", (ctx.held_out_label)()));
    let mut full_outcomes: Vec<Outcome> = Vec::new();
    let mut full_brier = [0.0f64; 2];
    for f in 0..nf {
        if (ctx.idle)(f) {
            continue;
        }
        let train: Vec<Arc<PreparedGame>> = ctx.folds[f].0.iter().cloned().chain(older(f)).collect();
        let model = ctx.model(f, &train, None);
        let test = (ctx.scored)(&ctx.folds[f].1, f, None);
        compare_sides(ctx, f, &model, &ctx.settings(with_memory(decay)), classic_picks, &test, true, &mut full_outcomes, &mut full_brier, &scored_set);
    }
    let base_mem = ctx.base.and_then(|b| b.memory.as_ref());
    let full_g = (ctx.merge_in)(base_mem.map(|m| &m.0), group_outcomes(&full_outcomes, None));
    if let Some(m) = base_mem {
        full_brier[0] += m.1[0];
        full_brier[1] += m.1[1];
    }
    let r = lifts(&full_g, None, 10.0, None);
    let (lo, hi) = (ctx.interval)(&full_g, None);
    let full = MemoryFull {
        memory: r.draft,
        classic: r.baseline,
        delta: r.draft - r.baseline,
        low: lo,
        high: hi,
        brier: if full_brier[1] != 0.0 { full_brier[0] / full_brier[1] } else { f64::NAN },
        classic_brier: ctx.brier_draft,
    };

    (ctx.step)("Patch memory early in a patch");
    let mut early_outcomes: Vec<Outcome> = Vec::new();
    let (mut current_games, mut kept_games) = (0.0, 0.0);
    for f in 0..nf {
        let train: Vec<Arc<PreparedGame>> = ctx.folds[f]
            .0
            .iter()
            .filter(|g| {
                if (ctx.age_of)(g) != 0.0 {
                    return true;
                }
                current_games += 1.0;
                let kept = fnv(&format!("{}:{}", js::num_to_string(opts.seed), g.match_id)) % 4 == 0;
                if kept {
                    kept_games += 1.0;
                }
                kept
            })
            .cloned()
            .collect();
        if let (Some(_), Some(u)) = (ctx.up, ctx.up) {
            let any = ctx.folds[f].1.iter().any(|g| (ctx.is_fresh)(g) && (base_hash(ctx.salt, &format!("sample:{}", g.match_id)) as u64) % u.grid_stride as u64 == 0);
            if !any {
                continue;
            }
        }
        let test = (ctx.scored)(&ctx.folds[f].1, f, None);
        // The two-patch model's picks first, then patch memory's against them.
        let two_patch_picks = {
            let model = ctx.model(f, &train, None);
            let settings = ctx.settings(opts.weights.clone());
            let weighted = weigh_games(&model, ctx.data, ctx.region_patch, &settings.weights);
            let cache = DraftCache::default();
            let at = (ctx.subsample_at)(&test);
            let mut picks: HashMap<String, Picks> = HashMap::new();
            let sh = ctx.sh;
            ctx.team.for_each(
                at.len(),
                |j| {
                    let s = &test[at[j]];
                    let baseline = (sh.baseline_for)(f, s.g.lane, s.g.opponent);
                    picks_of(
                        &recommend_draft(&model, &query_of(&s.g), sh.data, sh.excl, sh.region_patch, &baseline, &settings, &DraftExtra {
                            reasons: Some(false),
                            detail: false,
                            weighted: Some(&weighted),
                            cache: Some(&cache),
                            memo: None,
                            decisions: Some(&scored_set),
                        })
                        .build,
                    )
                },
                |j, p| {
                    picks.insert(test[at[j]].g.match_id.clone(), p);
                },
                &|_, _| {},
            );
            picks
        };
        let all_train: Vec<Arc<PreparedGame>> = train.iter().cloned().chain(older(f)).collect();
        let model = ctx.model(f, &all_train, None);
        let mut b = [0.0; 2];
        compare_sides(ctx, f, &model, &ctx.settings(with_memory(decay)), &two_patch_picks, &test, false, &mut early_outcomes, &mut b, &scored_set);
    }
    let early_g = (ctx.merge_in)(base_mem.map(|m| &m.2), group_outcomes(&early_outcomes, None));
    next.memory = Some(((ctx.for_base)(&full_g), full_brier, (ctx.for_base)(&early_g)));
    let er = lifts(&early_g, None, 10.0, None);
    let (elo, ehi) = (ctx.interval)(&early_g, None);
    let folds_less = js_max(1.0, nf as f64 - 1.0);
    let early = MemoryEarly {
        memory: er.draft,
        classic: er.baseline,
        delta: er.draft - er.baseline,
        low: elo,
        high: ehi,
        current_games: js::round(current_games / folds_less),
        kept_games: js::round(kept_games / folds_less),
    };
    (ctx.log)(&format!(
        "patch memory: all games {} pts ({} to {}), early in a patch {} pts ({} to {})",
        js::to_fixed(full.delta * 100.0, 2),
        js::to_fixed(full.low * 100.0, 2),
        js::to_fixed(full.high * 100.0, 2),
        js::to_fixed(early.delta * 100.0, 2),
        js::to_fixed(early.low * 100.0, 2),
        js::to_fixed(early.high * 100.0, 2)
    ));
    let ci = |lo: f64, hi: f64| format!("{} to {} pts", js::to_fixed(lo * 100.0, 2), js::to_fixed(hi * 100.0, 2));
    let gate_reason = if !(full.delta >= 0.0) {
        format!("worse than two patches alone with all games ({} pts)", js::to_fixed(full.delta * 100.0, 2))
    } else if !(full.brier <= full.classic_brier) {
        format!("Brier {} is worse than two patches alone ({})", js::to_fixed(full.brier, 4), js::to_fixed(full.classic_brier, 4))
    } else if !(full.low > 0.0 || early.low > 0.0) {
        format!("not better with 95 % confidence (all games {}, early in a patch {})", ci(full.low, full.high), ci(early.low, early.high))
    } else {
        format!("better with 95 % confidence {}, no worse otherwise, Brier no worse", if full.low > 0.0 { "with all games" } else { "early in a patch" })
    };
    let passed = full.delta >= 0.0 && full.brier <= full.classic_brier && (full.low > 0.0 || early.low > 0.0);
    Ok(report(decay, grid, full, early, gate_reason, passed))
}

fn evaluate_scoring(ctx: &Ctx, classic_picks: &HashMap<String, Picks>, classic_thetas: &HashMap<String, HashMap<&'static str, f64>>, next: &mut EvalBaseData) -> Result<ScoringReport> {
    let opts = ctx.opts;
    let nf = opts.folds;
    let none = FairLift { fair: 0.0, classic: 0.0, delta: 0.0, low: f64::NAN, high: f64::NAN };
    let failed = |gate_reason: String, expected: Option<ExpectedScore>| ScoringReport {
        expected,
        adjusted: none.clone(),
        raw: FairRaw { fair: 0.0, classic: 0.0, delta: 0.0 },
        brier: FairBrier { fair: f64::NAN, classic: f64::NAN, n: 0.0 },
        decisions: vec![],
        gate: "failed".into(),
        gate_reason,
    };
    if ctx.test_games < opts.min_games as f64 {
        (ctx.step)("Expected win rate: not enough games");
        (ctx.step)("Runes and summoners against the draft: not enough games");
        return Ok(failed(format!("only {} held-out games (needs {})", js::num_to_string(ctx.test_games), opts.min_games), None));
    }
    (ctx.step)("Expected win rate before the game");
    let eo = expected_defaults();
    // Each fold's expected win rate (side by side).
    let per_fold: Vec<(Vec<f64>, Option<Penalty>, Vec<f64>)> = {
        let mut out: Vec<Option<(Vec<f64>, Option<Penalty>, Vec<f64>)>> = (0..nf).map(|_| None).collect();
        let sh = ctx.sh;
        ctx.team.for_each(
            nf,
            |f| -> Result<(Vec<f64>, Option<Penalty>, Vec<f64>), String> {
                let train: Vec<&PreparedGame> = ctx.folds[f].0.iter().map(|g| &**g).collect();
                let test = &ctx.folds[f].1;
                let kept = sh.up.and_then(|u| u.penalties.as_ref()).and_then(|p| p.get(f).copied().flatten());
                let o = match kept {
                    Some(k) => super::expected::ExpectedOptions { penalties: vec![k], ..eo.clone() },
                    None => eo.clone(),
                };
                let (values, penalty) = cross_fitted(&train, ctx.age_of, &o).map_err(|e| e.to_string())?;
                let full = match penalty {
                    Some(p) => fit_expected(&train, ctx.age_of, p, &eo).map_err(|e| e.to_string())?,
                    None => None,
                };
                let tv: Vec<f64> = test.iter().map(|g| full.as_ref().map(|m| expected_of(m, g, ctx.age_of)).unwrap_or(f64::NAN)).collect();
                Ok((values, penalty, tv))
            },
            |f, r| out[f] = r.ok(),
            &|_, _| {},
        );
        out.into_iter().map(|x| x.unwrap_or((vec![], None, vec![]))).collect()
    };
    // The penalty most folds chose (the more rigid on a tie).
    let mut votes: Vec<(Penalty, usize)> = Vec::new();
    for x in &per_fold {
        if let Some(p) = x.1 {
            match votes.iter_mut().find(|v| v.0 == p) {
                Some(v) => v.1 += 1,
                None => votes.push((p, 1)),
            }
        }
    }
    votes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| crate::engine::recommend::js_order(b.0.lambda - a.0.lambda)));
    let penalty = votes.first().map(|v| v.0);
    let (mut p, mut pb, mut y) = (Vec::new(), Vec::new(), Vec::new());
    for f in 0..nf {
        let train = &ctx.folds[f].0;
        let mut s = 0.0;
        for g in train {
            s += g.win;
        }
        let rate = s / js_max(1.0, train.len() as f64);
        for (i, g) in ctx.folds[f].1.iter().enumerate() {
            if !(ctx.is_fresh)(g) {
                continue;
            }
            let e = per_fold[f].2.get(i).copied().unwrap_or(f64::NAN);
            if e.is_finite() {
                p.push(e);
                pb.push(rate);
                y.push(g.win);
            }
        }
    }
    if let Some(sb) = ctx.base.and_then(|b| b.scoring.as_ref()) {
        p = sb.expected_p.iter().copied().chain(p).collect();
        pb = sb.expected_pb.iter().copied().chain(pb).collect();
        y = sb.expected_y.iter().copied().chain(y).collect();
    }
    if p.is_empty() {
        (ctx.step)("Runes and summoners against the draft: not enough games");
        return Ok(failed("too few games to fit the expected win rate".into(), None));
    }
    let se = score_probabilities(&p, &y);
    let sb = score_probabilities(&pb, &y);
    let expected = ExpectedScore { brier: se.brier, log_loss: se.log_loss, base_brier: sb.brier, base_log_loss: sb.log_loss, games: p.len() as f64, penalty };
    (ctx.log)(&format!(
        "expected win rate before the game: Brier {} (overall win rate {}), log loss {} ({}) on {} held-out games",
        js::to_fixed(se.brier, 4),
        js::to_fixed(sb.brier, 4),
        js::to_fixed(se.log_loss, 4),
        js::to_fixed(sb.log_loss, 4),
        p.len()
    ));
    (ctx.step)(&format!("Runes and summoners against the draft on {}", (ctx.held_out_label)()));
    let mut adjusted: Vec<Outcome> = Vec::new();
    let mut raw: Vec<Outcome> = Vec::new();
    let mut brier_sum = [0.0f64; 3];
    let mut changed: indexmap::IndexMap<String, [f64; 2]> = indexmap::IndexMap::new();
    let settings = ctx.settings(opts.weights.clone());
    let pregame = set_of(&PREGAME);
    for f in 0..nf {
        if (ctx.idle)(f) {
            continue;
        }
        let model = ctx.model(f, &ctx.folds[f].0, Some(&per_fold[f].0));
        let weighted = weigh_games(&model, ctx.data, ctx.region_patch, &opts.weights);
        let cache = DraftCache::default();
        let test = (ctx.scored)(&ctx.folds[f].1, f, None);
        let at = (ctx.detail_at)(&test);
        let sh = ctx.sh;
        ctx.team.for_each(
            at.len(),
            |j| {
                let s = &test[at[j]];
                let baseline = (sh.baseline_for)(f, s.g.lane, s.g.opponent);
                let rec = recommend_draft(&model, &query_of(&s.g), sh.data, sh.excl, sh.region_patch, &baseline, &settings, &DraftExtra {
                    reasons: Some(false),
                    detail: true,
                    weighted: Some(&weighted),
                    cache: Some(&cache),
                    memo: None,
                    decisions: Some(&pregame),
                });
                let mut est: HashMap<&'static str, f64> = HashMap::new();
                for (key, option, _) in actual(s) {
                    if PREGAME.contains(&key) {
                        est.insert(key, rec.estimates.as_ref().and_then(|e| e.get(key)).and_then(|m| m.get(&option)).copied().unwrap_or(0.0));
                    }
                }
                (picks_of(&rec.build), est)
            },
            |j, (pa, est)| {
                let i = at[j];
                let s = &test[i];
                let e = per_fold[f].2.get(i).copied().unwrap_or(f64::NAN);
                let empty_p = HashMap::new();
                let empty_t = HashMap::new();
                let pc = classic_picks.get(&s.g.match_id).unwrap_or(&empty_p);
                let tc = classic_thetas.get(&s.g.match_id).unwrap_or(&empty_t);
                let pilot: Arc<str> = Arc::from(s.g.pilot.as_str());
                for (key, option, wpa) in actual(s) {
                    if !PREGAME.contains(&key) {
                        continue;
                    }
                    let followed_draft = pa.get(key) == Some(&option);
                    let followed_baseline = pc.get(key) == Some(&option);
                    let overall = s.g.win - wpa;
                    let exp = if e.is_finite() { e } else { overall };
                    adjusted.push(Outcome { pilot: pilot.clone(), key, wpa: s.g.win - exp, followed_draft, followed_baseline, w: None });
                    raw.push(Outcome { pilot: pilot.clone(), key, wpa, followed_draft, followed_baseline, w: None });
                    brier_sum[0] += sq(clip(exp + est.get(key).copied().unwrap_or(0.0)) - s.g.win);
                    brier_sum[1] += sq(clip(overall + tc.get(key).copied().unwrap_or(0.0)) - s.g.win);
                    brier_sum[2] += 1.0;
                    let c = changed.entry(key.to_string()).or_insert([0.0; 2]);
                    c[0] += 1.0;
                    if pa.get(key) != pc.get(key) {
                        c[1] += 1.0;
                    }
                }
            },
            &|_, _| {},
        );
    }
    let prior = ctx.base.and_then(|b| b.scoring.as_ref());
    let ga = (ctx.merge_in)(prior.map(|p| &p.adjusted), group_outcomes(&adjusted, None));
    let gr = (ctx.merge_in)(prior.map(|p| &p.raw), group_outcomes(&raw, None));
    if let Some(pr) = prior {
        brier_sum[0] += pr.brier[0];
        brier_sum[1] += pr.brier[1];
        brier_sum[2] += pr.brier[2];
        for (key, c) in &pr.changed {
            let now = changed.entry(key.clone()).or_insert([0.0; 2]);
            now[0] += c[0];
            now[1] += c[1];
        }
    }
    next.scoring = Some(ScoringBase {
        adjusted: (ctx.for_base)(&ga),
        raw: (ctx.for_base)(&gr),
        brier: brier_sum,
        changed: changed.iter().map(|(k, v)| (k.clone(), *v)).collect(),
        expected_p: p.clone(),
        expected_y: y.clone(),
        expected_pb: pb.clone(),
        penalties: per_fold.iter().map(|x| x.1).collect(),
    });
    let la = lifts(&ga, None, 10.0, Some(&PREGAME));
    let lr = lifts(&gr, None, 10.0, Some(&PREGAME));
    let (lo, hi) = (ctx.interval)(&ga, Some(&PREGAME));
    let adj = FairLift { fair: la.draft, classic: la.baseline, delta: la.draft - la.baseline, low: lo, high: hi };
    let brier_of = FairBrier {
        fair: if brier_sum[2] != 0.0 { brier_sum[0] / brier_sum[2] } else { f64::NAN },
        classic: if brier_sum[2] != 0.0 { brier_sum[1] / brier_sum[2] } else { f64::NAN },
        n: brier_sum[2],
    };
    (ctx.log)(&format!(
        "runes and summoners against the draft: {} pts ({} to {}) over today's scoring",
        js::to_fixed(adj.delta * 100.0, 2),
        js::to_fixed(adj.low * 100.0, 2),
        js::to_fixed(adj.high * 100.0, 2)
    ));
    let informative = expected.log_loss < expected.base_log_loss;
    let gate_reason = if !informative {
        format!(
            "the expected win rate predicts held-out results no better than the overall win rate (log loss {} against {})",
            js::to_fixed(expected.log_loss, 4),
            js::to_fixed(expected.base_log_loss, 4)
        )
    } else if !(adj.delta >= 0.0) {
        format!("worse than today's scoring ({})", pts(adj.delta))
    } else if !(brier_of.fair <= brier_of.classic) {
        format!("Brier {} is worse than today's scoring ({})", js::to_fixed(brier_of.fair, 4), js::to_fixed(brier_of.classic, 4))
    } else if !(adj.low > 0.0) {
        format!("not better with 95 % confidence ({} to {})", pts(adj.low), pts(adj.high))
    } else {
        "better with 95 % confidence, Brier no worse".into()
    };
    let passed = informative && adj.low > 0.0 && brier_of.fair <= brier_of.classic;
    Ok(ScoringReport {
        expected: Some(expected),
        adjusted: adj,
        raw: FairRaw { fair: lr.draft, classic: lr.baseline, delta: lr.draft - lr.baseline },
        brier: brier_of,
        decisions: la
            .per
            .iter()
            .map(|d| {
                let c = changed.get(&d.key).copied().unwrap_or([0.0; 2]);
                ScoringDecision { key: d.key.clone(), fair: d.draft, classic: d.baseline, changed: c[1] / js_max(1.0, c[0]) }
            })
            .collect(),
        gate: if passed { "passed".into() } else { "failed".into() },
        gate_reason,
    })
}

fn evaluate_swaps(ctx: &Ctx, next: &mut EvalBaseData) -> Result<SwapReport> {
    let opts = ctx.opts;
    let nf = opts.folds;
    (ctx.step)(&format!("Late-game swaps on {}", (ctx.held_out_label)()));
    let mut outcomes: Vec<Outcome> = Vec::new();
    let mut counts = [0.0f64; 4]; // situations, games, swapped, advised
    let o = EstimateOptions { h: ctx.chosen.h, k: ctx.chosen.k, ..ESTIMATE_DEFAULTS };
    let keep_s = js::num_to_string(SWAP_KEEP);
    let sell_s = js::num_to_string(SWAP_SELL);
    for f in 0..nf {
        if (ctx.idle)(f) {
            continue;
        }
        let test = (ctx.scored)(&ctx.folds[f].1, f, None);
        let at: Vec<usize> = (ctx.detail_at)(&test).into_iter().filter(|i| !test[*i].g.swaps.is_empty()).collect();
        if at.is_empty() {
            continue;
        }
        let model = ctx.model(f, &ctx.folds[f].0, None);
        let weighted = weigh_games(&model, ctx.data, ctx.region_patch, &opts.weights);
        let cache = DraftCache::default();
        let _sh = ctx.sh;
        ctx.team.for_each(
            at.len(),
            |j| {
                let g = &test[at[j]].g;
                let q = item_query_of(&model, &query_of(g), o.h);
                let mut advice: HashMap<u64, String> = HashMap::new();
                g.swaps
                    .iter()
                    .map(|d| {
                        if let Some(a) = advice.get(&d.held.to_bits()) {
                            return a.clone();
                        }
                        let r = advise_swap(&model, &weighted, &q, d.held, &o, Some(&cache));
                        let a = match r.map(|r| r.pick) {
                            None | Some(SwapPick::Keep) => keep_s.clone(),
                            Some(SwapPick::Sell) => sell_s.clone(),
                            Some(SwapPick::Swap { item }) => item.to_string(),
                        };
                        advice.insert(d.held.to_bits(), a.clone());
                        a
                    })
                    .collect::<Vec<String>>()
            },
            |j, picks| {
                let s = &test[at[j]];
                counts[1] += 1.0;
                let pilot: Arc<str> = Arc::from(s.g.pilot.as_str());
                for (k, d) in s.g.swaps.iter().enumerate() {
                    counts[0] += 1.0;
                    if d.item != SWAP_KEEP {
                        counts[2] += 1.0;
                    }
                    if picks[k] != keep_s {
                        counts[3] += 1.0;
                    }
                    outcomes.push(Outcome {
                        pilot: pilot.clone(),
                        key: "swap",
                        wpa: s.swap_wpa[k],
                        followed_draft: picks[k] == js::num_to_string(d.item),
                        followed_baseline: d.item == SWAP_KEEP,
                        w: Some(d.w),
                    });
                }
            },
            &|_, _| {},
        );
    }
    let prior = ctx.base.and_then(|b| b.swaps.as_ref());
    if let Some(p) = prior {
        counts[0] += p.1[0];
        counts[1] += p.1[1];
        counts[2] += p.1[2];
        counts[3] += p.1[3];
    }
    let g = (ctx.merge_in)(prior.map(|p| &p.0), group_outcomes(&outcomes, Some(&SWAP_KEYS)));
    next.swaps = Some(((ctx.for_base)(&g), counts));
    let failed = |gate_reason: String, lift: SwapLift| SwapReport {
        lift,
        situations: counts[0],
        games: counts[1],
        swapped: if counts[0] != 0.0 { counts[2] / counts[0] } else { 0.0 },
        advised: if counts[0] != 0.0 { counts[3] / counts[0] } else { 0.0 },
        gate: "failed".into(),
        gate_reason,
    };
    let none = SwapLift { advice: 0.0, keep: 0.0, delta: 0.0, low: f64::NAN, high: f64::NAN };
    if counts[1] < opts.min_games as f64 {
        return Ok(failed(
            format!("only {} held-out games with a late full-inventory shop visit (needs {})", js::num_to_string(counts[1]), opts.min_games),
            none,
        ));
    }
    let r = lifts(&g, None, 10.0, None);
    let (lo, hi) = (ctx.interval)(&g, None);
    let lift = SwapLift { advice: r.draft, keep: r.baseline, delta: r.draft - r.baseline, low: lo, high: hi };
    (ctx.log)(&format!(
        "late-game swaps: {} ({} to {}) over keeping every item, {} situations in {} games",
        pts(lift.delta),
        pts(lift.low),
        pts(lift.high),
        js::num_to_string(counts[0]),
        js::num_to_string(counts[1])
    ));
    if !(lift.delta >= 0.0) {
        return Ok(failed(format!("worse than keeping every item ({})", pts(lift.delta)), lift));
    }
    if !(lift.low > 0.0) {
        return Ok(failed(format!("not better than keeping every item with 95 % confidence ({} to {})", pts(lift.low), pts(lift.high)), lift));
    }
    let mut r = failed(String::new(), lift);
    r.gate = "passed".into();
    r.gate_reason = "better than keeping every item with 95 % confidence".into();
    Ok(r)
}

fn evaluate_tuning(ctx: &Ctx, t: &TuningOptions, classic_picks: Option<&HashMap<String, Picks>>, next: &mut EvalBaseData) -> Result<TuningReport> {
    let opts = ctx.opts;
    let nf = opts.folds;
    let failed = |gate_reason: String, grid: Vec<TuningGridRow>, choice: TuningChoice, full: Option<TuningFull>| TuningReport {
        grid,
        choice,
        full,
        gate: "failed".into(),
        gate_reason,
    };
    let Some(classic_picks) = classic_picks.filter(|_| ctx.test_games >= opts.min_games as f64) else {
        for _ in 0..nf + 1 {
            (ctx.step)("Fine-tuning: not enough games");
        }
        return Ok(failed(format!("only {} held-out games (needs {})", js::num_to_string(ctx.test_games), opts.min_games), vec![], UNTUNED, None));
    };
    let settings_of = |c: &TuningChoice| DraftSettings {
        weights: WeightOptions { pilot_strength: Some(c.pilot_strength), recency_days: c.recency_days, ..opts.weights.clone() },
        estimate: EstimateOptions { h: ctx.chosen.h, k: ctx.chosen.k, ..ESTIMATE_DEFAULTS },
        item_k: c.item_k,
    };
    let mut variants: Vec<(&'static str, f64)> = Vec::new();
    variants.extend(t.pilot_strengths.iter().filter(|v| **v != 1.0).map(|v| ("pilotStrength", *v)));
    variants.extend(opts.ks.iter().filter(|k| **k != ctx.chosen.k).map(|v| ("itemK", *v)));
    variants.extend(t.recency_days.iter().map(|v| ("recencyDays", *v)));
    let scored_set = set_of(&EVAL_DECISIONS);
    let items_set = set_of(&SCORED_ITEMS);
    if let Some(u) = ctx.up {
        let base_full = ctx.report_base().and_then(|r| r.tuning.as_ref()).and_then(|t| t.full.as_ref()).is_some();
        if !(u.tuning.is_some() && base_full) {
            (ctx.step)("Fine-tuning: as the full evaluation left it");
            return Ok(ctx
                .report_base()
                .and_then(|r| r.tuning.clone())
                .unwrap_or_else(|| failed("not evaluated by the full evaluation".into(), vec![], UNTUNED, None)));
        }
    }
    let (grid, choice) = if let Some(u) = ctx.up {
        (ctx.report_base().and_then(|r| r.tuning.as_ref()).map(|t| t.grid.clone()).unwrap_or_default(), u.tuning.unwrap())
    } else {
        let mut grid_outcomes: Vec<Vec<Outcome>> = variants.iter().map(|_| vec![]).collect();
        for f in 0..nf {
            (ctx.step)(&format!("Fine-tuning: fold {} of {} ({} settings)", f + 1, nf, variants.len()));
            let model = ctx.model(f, &ctx.folds[f].0, None);
            let test = (ctx.scored)(&ctx.folds[f].1, f, None);
            for (i, (setting, value)) in variants.iter().enumerate() {
                let mut c = UNTUNED;
                match *setting {
                    "pilotStrength" => c.pilot_strength = *value,
                    "itemK" => c.item_k = Some(*value),
                    _ => c.recency_days = Some(*value),
                }
                let mut b = [0.0; 2];
                compare_sides(ctx, f, &model, &settings_of(&c), classic_picks, &test, false, &mut grid_outcomes[i], &mut b, if *setting == "itemK" { &items_set } else { &scored_set });
            }
        }
        let grid: Vec<TuningGridRow> = variants
            .iter()
            .enumerate()
            .map(|(i, (setting, value))| {
                let r = lifts(&group_outcomes(&grid_outcomes[i], None), None, 10.0, None);
                (ctx.log)(&format!(
                    "fine-tuning, {} {}: lift {} pts, chosen model {} pts",
                    setting,
                    js::num_to_string(*value),
                    js::to_fixed(r.draft * 100.0, 2),
                    js::to_fixed(r.baseline * 100.0, 2)
                ));
                TuningGridRow { setting: setting.to_string(), value: *value, tuned: r.draft, current: r.baseline, delta: r.draft - r.baseline }
            })
            .collect();
        let mut c = UNTUNED;
        for setting in ["pilotStrength", "itemK", "recencyDays"] {
            let mut best: Option<&TuningGridRow> = None;
            for g in grid.iter().filter(|g| g.setting == setting) {
                if g.delta > 0.0 && best.map(|b| g.delta > b.delta).unwrap_or(true) {
                    best = Some(g);
                }
            }
            if let Some(b) = best {
                match setting {
                    "pilotStrength" => c.pilot_strength = b.value,
                    "itemK" => c.item_k = Some(b.value),
                    _ => c.recency_days = Some(b.value),
                }
            }
        }
        if c.pilot_strength == 1.0 && c.item_k.is_none() && c.recency_days.is_none() {
            (ctx.step)("Fine-tuning: no setting did better");
            return Ok(failed("no setting did better than the chosen model on the subsample".into(), grid, UNTUNED, None));
        }
        (grid, c)
    };
    (ctx.step)(&format!("Fine-tuning on {}", (ctx.held_out_label)()));
    let mut full_outcomes: Vec<Outcome> = Vec::new();
    let mut full_brier = [0.0f64; 2];
    for f in 0..nf {
        if (ctx.idle)(f) {
            continue;
        }
        let model = ctx.model(f, &ctx.folds[f].0, None);
        let test = (ctx.scored)(&ctx.folds[f].1, f, None);
        compare_sides(ctx, f, &model, &settings_of(&choice), classic_picks, &test, true, &mut full_outcomes, &mut full_brier, &scored_set);
    }
    let base_t = ctx.base.and_then(|b| b.tuning.as_ref());
    let g = (ctx.merge_in)(base_t.map(|t| &t.0), group_outcomes(&full_outcomes, None));
    if let Some(b) = base_t {
        full_brier[0] += b.1[0];
        full_brier[1] += b.1[1];
    }
    next.tuning = Some(((ctx.for_base)(&g), full_brier));
    let r = lifts(&g, None, 10.0, None);
    let (lo, hi) = (ctx.interval)(&g, None);
    let full = TuningFull {
        tuned: r.draft,
        current: r.baseline,
        delta: r.draft - r.baseline,
        low: lo,
        high: hi,
        brier: if full_brier[1] != 0.0 { full_brier[0] / full_brier[1] } else { f64::NAN },
        current_brier: ctx.brier_draft,
    };
    (ctx.log)(&format!(
        "fine-tuning: {} pts ({} to {})",
        js::to_fixed(full.delta * 100.0, 2),
        js::to_fixed(full.low * 100.0, 2),
        js::to_fixed(full.high * 100.0, 2)
    ));
    let passed = full.low > 0.0 && full.brier <= full.current_brier;
    let gate_reason = if !(full.low > 0.0) {
        format!("not better than the chosen model with 95 % confidence ({} to {} pts)", js::to_fixed(full.low * 100.0, 2), js::to_fixed(full.high * 100.0, 2))
    } else if !(full.brier <= full.current_brier) {
        format!("Brier {} is worse than the chosen model's {}", js::to_fixed(full.brier, 4), js::to_fixed(full.current_brier, 4))
    } else {
        "better than the chosen model with 95 % confidence, Brier no worse".into()
    };
    Ok(if passed {
        TuningReport { grid, choice, full: Some(full), gate: "passed".into(), gate_reason }
    } else {
        failed(gate_reason, grid, choice, Some(full))
    })
}

#[allow(dead_code)]
fn _unused(_: &[ScoredGame], _: N) {}
type N = [f64; N_ITEM];
