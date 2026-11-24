//! Port of `server/model/service.ts`: the draft-aware model inside the app: built from the database when its data
//! changes (never during champ select or a game) and asked for drafts.

use super::dataset::{prepare_games, score_games_with, train_state_by, PrepareOptions, PreparedGame, STATE_MAX_ROWS};
use super::window::StateSample;
use super::draft::{build_draft_model, recommend_draft, weigh_games, DraftCache, DraftExtra, DraftModel, DraftQuery, DraftSettings, Weighted};
use super::estimate::{EstimateOptions, ESTIMATE_DEFAULTS};
use super::evaluate::TuningChoice;
use super::expected::{cross_fitted, expected_defaults, Penalty};
use super::profiles::{ChampionProfiles, PROFILE_DEFAULTS};
use super::run_eval::model_patches;
use super::state::{win_probability_of, N_FEATURES};
use super::weights::{weight_defaults, Memory, PatchAger, RegionPatchFn, WeightOptions, MEMORY_PATCHES};
use crate::app_config::{self, app_config_dir};
use crate::db::{ChampionTotals, Db};
use crate::ddragon::StaticData;
use crate::engine::exclusivity::ExclusivityIndex;
use crate::js;
use crate::scope::ScrapeScope;
use crate::types::{BuildRecommendation, DraftRecommendation};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Mark {
    pub row: Option<f64>,
    pub games: f64,
    pub patch: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GateDecay {
    pub gate: String,
    pub decay: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GatePenalty {
    pub gate: String,
    pub penalty: Option<Penalty>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GateChoice {
    pub gate: String,
    pub choice: TuningChoice,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Gate {
    pub gate: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FullRef {
    pub evaluated_at: f64,
    pub mark: Mark,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ImportedFrom {
    pub machine: String,
    pub exported_at: f64,
}

/// The newest evaluation's result (latest.json).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EvalSummary {
    pub evaluated_at: f64,
    pub gate: String,
    #[serde(deserialize_with = "de_bandwidth")]
    pub bandwidth: f64,
    pub k: f64,
    pub lambda: f64,
    pub data_version: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub memory: Option<GateDecay>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub scoring: Option<GatePenalty>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tuning: Option<GateChoice>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub swaps: Option<Gate>,
    /// The game-state model's sample the evaluation kept (2.2.0): "even" (today's) or "5min".
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sample: Option<String>,
    /// The effective window's gate (2.2.0): "passed" when an item's ΔW ends at its sale or upgrade.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub window: Option<Gate>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mark: Option<Mark>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub full: Option<FullRef>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub imported_from: Option<ImportedFrom>,
}

fn de_bandwidth<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<f64, D::Error> {
    let v: Option<f64> = Deserialize::deserialize(d)?;
    Ok(v.unwrap_or(f64::INFINITY))
}

pub fn eval_dir() -> PathBuf {
    app_config_dir().join("evals")
}

pub fn read_evaluation() -> Option<EvalSummary> {
    let t = std::fs::read_to_string(eval_dir().join("latest.json")).ok()?;
    serde_json::from_str(&t).ok()
}

/// Whether a patch is older than patch memory's window on every region: what Archive and Delete may drop.
pub fn before_window(db: &Db, data: &StaticData, window: f64) -> Result<Box<dyn Fn(&str) -> Option<bool>>> {
    let keep: std::collections::HashSet<String> = model_patches(db, data, window)?.into_iter().collect();
    let order = super::weights::patch_order(data);
    let fb = crate::patches::Fallback { patch: data.patch.clone(), previous_patch: data.previous_patch.clone() };
    let mut currents = vec![data.patch.clone()];
    currents.extend(crate::patches::region_patches(&db.game_versions()?, &fb, &[], crate::util::now_ms()).into_iter().map(|(_, r)| r.patch));
    Ok(Box::new(move |patch: &str| {
        if keep.contains(patch) {
            return Some(false);
        }
        let mut known = false;
        for c in &currents {
            let Some(age) = super::weights::patch_age(&order, patch, c) else { continue };
            if age < window {
                return Some(false);
            }
            known = true;
        }
        if known {
            Some(true)
        } else {
            None
        }
    }))
}

pub const MEMORY_DEFAULT_DECAY: f64 = 0.75;

/// Patch memory as it stands: the Data tab's On / Off, else on once an evaluation found it better.
pub fn current_memory() -> Option<Memory> {
    let chosen = app_config::get_str("patchMemory");
    let ev = read_evaluation().and_then(|e| e.memory);
    if chosen.as_deref() == Some("off") {
        return None;
    }
    if chosen.as_deref() == Some("on") || ev.as_ref().map(|m| m.gate == "passed").unwrap_or(false) {
        return Some(Memory { decay: ev.map(|m| m.decay).unwrap_or(MEMORY_DEFAULT_DECAY), patches: MEMORY_PATCHES });
    }
    None
}

/// Fine-tuning's choice, when the last evaluation passed it.
pub fn current_tuning() -> Option<TuningChoice> {
    read_evaluation().and_then(|e| e.tuning).filter(|t| t.gate == "passed").map(|t| t.choice)
}

/// Runes and summoners against the draft as it stands: the penalties to fit with; None: off.
pub fn current_scoring() -> Option<Vec<Penalty>> {
    let cfg = app_config::read_app_config();
    let ev = read_evaluation().and_then(|e| e.scoring);
    let on = if cfg.get("patchMemory").map(|v| v.is_null()).unwrap_or(true) {
        ev.as_ref().map(|s| s.gate == "passed").unwrap_or(false)
    } else {
        cfg.get("pregameScoring").and_then(|v| v.as_str()) == Some("on")
    };
    if !on {
        return None;
    }
    Some(match ev.and_then(|e| e.penalty) {
        Some(p) => vec![p],
        None => expected_defaults().penalties,
    })
}

fn swaps_on(cfg: &serde_json::Map<String, Value>, ev: Option<&EvalSummary>) -> bool {
    if cfg.get("patchMemory").map(|v| v.is_null()).unwrap_or(true) {
        ev.and_then(|e| e.swaps.as_ref()).map(|s| s.gate == "passed").unwrap_or(false)
    } else {
        cfg.get("lateSwaps").and_then(|v| v.as_str()) == Some("on")
    }
}

/// The game-state model's sample and the ΔW definition the last evaluation kept (2.2.0); today's without one.
pub fn current_wpa() -> (StateSample, bool) {
    wpa_of(read_evaluation().as_ref())
}

pub fn wpa_of(ev: Option<&EvalSummary>) -> (StateSample, bool) {
    let sample = StateSample::from_name(ev.and_then(|e| e.sample.as_deref()));
    let window = ev.and_then(|e| e.window.as_ref()).map(|w| w.gate == "passed").unwrap_or(false);
    (sample, window)
}

pub fn current_swaps() -> bool {
    swaps_on(&app_config::read_app_config(), read_evaluation().as_ref())
}

/// The Data tab's model switches as saved (memory, scoring, swaps) as JSON parts of DraftModelInfo.
fn switches(memory: &Option<Memory>, scoring: &Option<Vec<Penalty>>) -> (Value, Value, Value) {
    let ev = read_evaluation();
    let cfg = app_config::read_app_config();
    let choice = cfg.get("patchMemory").and_then(|v| v.as_str()).unwrap_or("auto").to_string();
    let gate = |g: Option<&str>| g.unwrap_or("not evaluated").to_string();
    let mem = json!({
        "active": memory.is_some(), "choice": choice,
        "gate": gate(ev.as_ref().and_then(|e| e.memory.as_ref()).map(|m| m.gate.as_str())),
        "decay": memory.as_ref().map(|m| js::num(m.decay)).or_else(|| ev.as_ref().and_then(|e| e.memory.as_ref()).map(|m| js::num(m.decay))),
    });
    let sc = json!({
        "active": scoring.is_some(),
        "gate": gate(ev.as_ref().and_then(|e| e.scoring.as_ref()).map(|m| m.gate.as_str())),
        "choice": if choice == "auto" { "auto".to_string() } else { cfg.get("pregameScoring").and_then(|v| v.as_str()).unwrap_or("off").to_string() },
    });
    let sw = json!({
        "active": swaps_on(&cfg, ev.as_ref()),
        "gate": gate(ev.as_ref().and_then(|e| e.swaps.as_ref()).map(|m| m.gate.as_str())),
        "choice": if choice == "auto" { "auto".to_string() } else { cfg.get("lateSwaps").and_then(|v| v.as_str()).unwrap_or("off").to_string() },
    });
    (mem, sc, sw)
}

/// The model's info with its switches as just saved.
pub fn with_switches(mut info: Value) -> Value {
    let (m, s, w) = switches(&current_memory(), &current_scoring());
    if let (Some(im), Some(mm)) = (info.get_mut("memory").and_then(|v| v.as_object_mut()), m.as_object()) {
        for (k, v) in mm {
            im.insert(k.clone(), v.clone());
        }
    }
    if let (Some(is), Some(ss)) = (info.get_mut("scoring").and_then(|v| v.as_object_mut()), s.as_object()) {
        for (k, v) in ss {
            is.insert(k.clone(), v.clone());
        }
    }
    info["swaps"] = w;
    info
}

pub struct Built {
    pub version: String,
    pub model: DraftModel,
    pub prepared: Vec<Arc<PreparedGame>>,
    pub profiles: Arc<ChampionProfiles>,
    pub profile_rows: (Vec<ChampionTotals>, Vec<ChampionTotals>),
    pub build_ms: f64,
    pub built_at: i64,
    pub own_games: i64,
}

#[derive(Clone)]
pub struct BuildScoring {
    pub penalties: Vec<Penalty>,
}

/// Builds the model from the database; with patch memory, from every patch in its window.
#[allow(clippy::too_many_arguments)]
pub fn build_from_db(
    db: &Db,
    data: &Arc<StaticData>,
    scope: &ScrapeScope,
    starting_item_window_ms: f64,
    lambda: Option<f64>,
    memory: &Option<Memory>,
    scoring: Option<&BuildScoring>,
    region_patch: &RegionPatchFn,
    threads: usize,
) -> Result<Built> {
    build_from_db_with(db, data, scope, starting_item_window_ms, lambda, memory, scoring, region_patch, threads, (StateSample::Even, false))
}

/// The same with the evaluation's choices of 2.2.0: the game-state model's sample, and whether an item's ΔW ends at its
/// sale or upgrade (the draft model, the live re-ranker and the best buy all read the same).
#[allow(clippy::too_many_arguments)]
pub fn build_from_db_with(
    db: &Db,
    data: &Arc<StaticData>,
    scope: &ScrapeScope,
    starting_item_window_ms: f64,
    lambda: Option<f64>,
    memory: &Option<Memory>,
    scoring: Option<&BuildScoring>,
    region_patch: &RegionPatchFn,
    threads: usize,
    wpa: (StateSample, bool),
) -> Result<Built> {
    let (sample, window) = wpa;
    let t0 = Instant::now();
    let version = db.data_version();
    let own_games = db.own_game_count();
    let recent = model_patches(db, data, 2.0)?;
    let patches = match memory {
        Some(m) => model_patches(db, data, m.patches)?,
        None => recent.clone(),
    };
    let profile_rows = (db.champion_totals(&recent)?, db.older_champion_totals(&recent)?);
    let profiles = Arc::new(ChampionProfiles::new(&profile_rows.0, &profile_rows.1, data.clone(), PROFILE_DEFAULTS));
    let mut sources: Vec<String> = scope.draft_sources.clone();
    sources.push("me".into());
    let rows: Vec<Arc<crate::db::IreliaGameRow>> = db.model_games(&patches, &sources)?.into_iter().map(Arc::new).collect();
    let participants = db.model_participants(&patches, &sources)?;
    let o = PrepareOptions { starting_item_window_ms };
    let prepared: Vec<PreparedGame> = if threads > 1 && rows.len() >= 4000 {
        use rayon::prelude::*;
        let by = super::dataset::by_match(&participants);
        let empty: Vec<&crate::db::ParticipantLite> = vec![];
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build()?;
        pool.install(|| rows.par_iter().map(|r| super::dataset::prepare_game(r, by.get(r.match_id.as_str()).unwrap_or(&empty), data, &profiles, &o)).collect::<Vec<_>>())
            .into_iter()
            .flatten()
            .collect()
    } else {
        prepare_games(&rows, &participants, data, &profiles, &o)
    };
    drop(rows);
    let in_recent: std::collections::HashSet<&str> = recent.iter().map(|s| s.as_str()).collect();
    let state = if memory.is_some() {
        train_state_by(prepared.iter().filter(|g| in_recent.contains(g.patch.as_str())), lambda.unwrap_or(1.0), STATE_MAX_ROWS, sample)?
    } else {
        train_state_by(prepared.iter(), lambda.unwrap_or(1.0), STATE_MAX_ROWS, sample)?
    };
    let expected = match scoring {
        Some(s) => {
            let ager = PatchAger::new(data.clone(), region_patch.clone());
            let age_of = |g: &PreparedGame| ager.age(&g.platform, &g.patch);
            let refs: Vec<&PreparedGame> = prepared.iter().collect();
            Some(cross_fitted(&refs, &age_of, &super::expected::ExpectedOptions { penalties: s.penalties.clone(), ..expected_defaults() })?.0)
        }
        None => None,
    };
    let mut prepared = prepared;
    // Only the build reads each purchase's state features and raw live context; the frames go too (pAt kept).
    for g in prepared.iter_mut() {
        if let (Some(f), Some(st)) = (&g.frames, &state) {
            let n = f.len() / N_FEATURES;
            let mut p = Vec::with_capacity(n);
            for i in 0..n {
                let x: Vec<f64> = f[i * N_FEATURES..(i + 1) * N_FEATURES].iter().map(|v| *v as f64).collect();
                p.push(win_probability_of(&st.beta, &x));
            }
            g.p_at = Some(p);
        }
        g.frames = None;
        g.frame_t = None;
    }
    let prepared: Vec<Arc<PreparedGame>> = prepared.into_iter().map(Arc::new).collect();
    let scored = score_games_with(&prepared, state.as_ref(), expected.as_deref(), window);
    let mut model = build_draft_model(scored, state, profiles.clone());
    model.window = window;
    Ok(Built {
        version,
        model,
        prepared,
        profiles,
        profile_rows,
        build_ms: js::round(t0.elapsed().as_secs_f64() * 1000.0),
        built_at: crate::util::now_ms(),
        own_games,
    })
}

/// When new data is worth a build.
pub struct DataRebuild {
    pub share: f64,
    pub min_games: f64,
    pub min_age_ms: f64,
    pub max_age_ms: f64,
}
pub const DATA_REBUILD: DataRebuild = DataRebuild { share: 0.01, min_games: 50.0, min_age_ms: 30.0 * 60_000.0, max_age_ms: 6.0 * 60.0 * 60_000.0 };

pub fn data_due(built: &str, now: &str, age_ms: f64, o: &DataRebuild) -> bool {
    if built == now {
        return false;
    }
    if age_ms >= o.max_age_ms {
        return true;
    }
    if age_ms < o.min_age_ms {
        return false;
    }
    let parse = |s: &str| -> Vec<f64> { s.split(':').map(|x| js::to_number(&Value::String(x.into()))).collect() };
    let a = parse(built);
    let b = parse(now);
    let g = |v: &Vec<f64>, i: usize| v.get(i).copied().unwrap_or(f64::NAN);
    let changed = super::logistic::js_max(super::logistic::js_max((g(&b, 0) - g(&a, 0)).abs(), (g(&b, 2) - g(&a, 2)).abs()), (g(&b, 3) - g(&a, 3)).abs());
    if !changed.is_finite() {
        return true;
    }
    let g0 = g(&a, 0);
    changed >= super::logistic::js_max(o.min_games, (o.share * if g0.is_finite() { g0 } else { 0.0 }).ceil())
}

pub const DRAFT_CACHE_IDLE_MS: i64 = 3 * 60_000;

/// What the dashboard shows first: the user's choice, else the draft model once it passed the evaluation.
pub fn current_mode() -> &'static str {
    match app_config::get_str("buildMode").as_deref() {
        Some("draft") => "draft",
        Some("baseline") => "baseline",
        _ => {
            if read_evaluation().map(|e| e.gate == "passed").unwrap_or(false) {
                "draft"
            } else {
                "baseline"
            }
        }
    }
}

/// Draft settings from the scope and the last evaluation.
pub fn read_settings(scope: &ScrapeScope, own_platform: Option<String>, regions: Option<Vec<String>>) -> DraftSettings {
    let ev = read_evaluation();
    let tuned = current_tuning();
    let mut weights = WeightOptions {
        region_weights: scope.region_weights.clone(),
        own_platform,
        own_region_boost: scope.own_region_boost,
        patch_weights: scope.patch_weights.clone(),
        hotfixes: scope.hotfixes.clone(),
        regions,
        memory: current_memory(),
        ..weight_defaults()
    };
    if let Some(t) = &tuned {
        weights.pilot_strength = Some(t.pilot_strength);
        weights.recency_days = t.recency_days;
    }
    DraftSettings {
        weights,
        estimate: match &ev {
            Some(e) => EstimateOptions { h: e.bandwidth, k: e.k, ..ESTIMATE_DEFAULTS },
            None => ESTIMATE_DEFAULTS,
        },
        item_k: tuned.and_then(|t| t.item_k),
    }
}

pub struct DraftCacheState {
    built_at: i64,
    key: String,
    weighted: Weighted,
    cache: DraftCache,
    pub used_at: i64,
}

/// The model service (the model thread's state).
pub struct ModelService {
    pub built: Option<Built>,
    pub failed: Option<String>,
    built_for: String,
    older_games: f64,
    built_with_memory: bool,
    built_with_scoring: bool,
    attempted_at: i64,
    draft_cache: Option<DraftCacheState>,
}

/// What the service reads from the app (ModelEnv).
#[derive(Clone)]
pub struct ModelEnv {
    pub data: Arc<StaticData>,
    pub excl: Arc<ExclusivityIndex>,
    pub scope: ScrapeScope,
    pub region_patch: RegionPatchFn,
    pub own_platform: Option<String>,
    pub busy: bool,
    pub starting_item_window_ms: f64,
}

const REBUILD_EVERY_MS: i64 = 5 * 60_000;

pub const BUILD_CANCELLED: &str = "Champ select or a game started";

/// One build's inputs, taken on the model thread; `run` works on any thread with its own connection.
pub struct BuildJob {
    key: String,
    lambda: Option<f64>,
    memory: Option<Memory>,
    scoring: Option<BuildScoring>,
    wpa: (StateSample, bool),
}

impl BuildJob {
    pub fn run(&self, db: &Db, env: &ModelEnv, threads: usize, stop: &dyn Fn() -> bool) -> Result<Built> {
        if stop() {
            anyhow::bail!(BUILD_CANCELLED);
        }
        let built = build_from_db_with(db, &env.data, &env.scope, env.starting_item_window_ms, self.lambda, &self.memory, self.scoring.as_ref(), &env.region_patch, threads, self.wpa)?;
        if stop() {
            anyhow::bail!(BUILD_CANCELLED);
        }
        Ok(built)
    }
}

impl Default for ModelService {
    fn default() -> Self {
        ModelService { built: None, failed: None, built_for: String::new(), older_games: 0.0, built_with_memory: false, built_with_scoring: false, attempted_at: 0, draft_cache: None }
    }
}

impl ModelService {
    pub fn current(&self) -> Option<&Built> {
        self.built.as_ref()
    }
    pub fn holds_older_patches(&self) -> bool {
        self.built_with_memory
    }

    fn for_key(&self, env: &ModelEnv) -> String {
        let scoring = current_scoring();
        let settings = read_settings(&env.scope, env.own_platform.clone(), None);
        // 2.2.0: a new sample or ΔW definition is a new model (today's keep the key as it was).
        let (sample, window) = current_wpa();
        let wpa = format!("{}{}", if sample == StateSample::Even { "" } else { "|5min" }, if window { "|window" } else { "" });
        format!(
            "{}|{}|{}{}",
            env.data.version,
            js::num_to_string(settings.weights.memory.as_ref().map(|m| m.patches).unwrap_or(2.0)),
            match scoring {
                Some(s) => js::stringify(&serde_json::to_value(s).unwrap_or(Value::Null)),
                None => "overall".into(),
            },
            wpa
        )
    }

    pub fn needs_build(&self, db: &Db, env: &ModelEnv) -> bool {
        if crate::util::now_ms() - self.attempted_at < REBUILD_EVERY_MS {
            return false;
        }
        let Some(b) = &self.built else { return true };
        if env.busy {
            return false;
        }
        if self.for_key(env) != self.built_for {
            return true;
        }
        if db.own_game_count() != b.own_games {
            return true;
        }
        data_due(&b.version, &db.data_version(), (crate::util::now_ms() - b.built_at) as f64, &DATA_REBUILD)
    }

    pub fn rebuild(&mut self, db: &Db, env: &ModelEnv, threads: usize) -> Option<&Built> {
        let job = self.begin_build(env);
        let result = job.run(db, env, threads, &|| false);
        self.finish_build(job, result, env);
        self.built.as_ref()
    }

    /// What a build reads (the evaluation's penalty, patch memory, the scoring); marks the attempt.
    pub fn begin_build(&mut self, env: &ModelEnv) -> BuildJob {
        self.attempted_at = crate::util::now_ms();
        let ev = read_evaluation();
        let scoring = current_scoring();
        BuildJob {
            key: self.for_key(env),
            wpa: wpa_of(ev.as_ref()),
            lambda: ev.map(|e| e.lambda),
            memory: read_settings(&env.scope, env.own_platform.clone(), None).weights.memory,
            scoring: scoring.map(|p| BuildScoring { penalties: p }),
        }
    }

    /// Adopts a build's result: a failed one keeps the previous model (with the error for the Data tab), a cancelled
    /// one (`Err` with BUILD_CANCELLED) changes nothing. Returns whether a new model was adopted.
    pub fn finish_build(&mut self, job: BuildJob, result: Result<Built>, env: &ModelEnv) -> bool {
        match result {
            Ok(built) => {
                let ager = PatchAger::new(env.data.clone(), env.region_patch.clone());
                self.older_games = built.model.scored.iter().filter(|x| x.g.source != "me" && ager.age(&x.g.platform, &x.g.patch) >= 2.0).count() as f64;
                self.built_for = job.key;
                self.built_with_memory = job.memory.is_some();
                self.built_with_scoring = job.scoring.is_some();
                self.failed = None;
                self.draft_cache = None;
                eprintln!("[model] built from {} games in {} ms", built.prepared.len(), built.build_ms);
                self.built = Some(built);
                true
            }
            Err(e) if e.to_string() == BUILD_CANCELLED => {
                eprintln!("[model] {e}: the model stays as it was");
                false
            }
            Err(e) => {
                self.failed = Some(e.to_string());
                eprintln!("[model] build failed {e}");
                false
            }
        }
    }

    pub fn error(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    /// DraftModelInfo as JSON.
    pub fn info(&self, env: &ModelEnv) -> Option<Value> {
        let b = self.built.as_ref()?;
        let ev = read_evaluation();
        let s = read_settings(&env.scope, env.own_platform.clone(), None);
        let (mem, sc, sw) = switches(&s.weights.memory, &current_scoring());
        let mut memory = mem;
        memory["patches"] = js::num(MEMORY_PATCHES);
        memory["olderGames"] = js::num(self.older_games);
        memory["inModel"] = json!(self.built_with_memory);
        let mut scoring = sc;
        scoring["inModel"] = json!(self.built_with_scoring);
        let tuned = current_tuning();
        Some(json!({
            "gate": ev.as_ref().map(|e| e.gate.clone()).unwrap_or_else(|| "not evaluated".into()),
            "evaluatedAt": ev.as_ref().map(|e| js::num(e.evaluated_at)),
            "bandwidth": js::num(s.estimate.h), "k": js::num(s.estimate.k),
            "stateModel": b.model.state.as_ref().map(|m| json!({ "games": js::num(m.games), "rows": js::num(m.rows) })),
            "dataVersion": b.version,
            "games": b.model.scored.iter().filter(|x| x.g.source != "me").count(),
            "memory": memory,
            "scoring": scoring,
            "tuning": {
                "gate": ev.as_ref().and_then(|e| e.tuning.as_ref()).map(|t| t.gate.clone()).unwrap_or_else(|| "not evaluated".into()),
                "pilotStrength": js::num(tuned.map(|t| t.pilot_strength).unwrap_or(1.0)),
                "itemK": tuned.and_then(|t| t.item_k).map(js::num),
                "recencyDays": tuned.and_then(|t| t.recency_days).map(js::num),
            },
            "swaps": sw,
        }))
    }

    /// A draft-adjusted build for `q` next to `baseline` (None until a model is built). `keep`: champ select's caches.
    pub fn draft(&mut self, env: &ModelEnv, q: &DraftQuery, regions: Option<Vec<String>>, baseline: &BuildRecommendation, keep: bool) -> Option<DraftRecommendation> {
        let b = self.built.as_ref()?;
        if b.model.scored.is_empty() {
            return None;
        }
        let settings = read_settings(&env.scope, env.own_platform.clone(), regions);
        if !keep {
            self.draft_cache = None;
            return Some(recommend_draft(&b.model, q, &env.data, &env.excl, &env.region_patch, baseline, &settings, &DraftExtra::default()));
        }
        let mut platforms: Vec<String> = b.model.scored.iter().map(|s| s.g.platform.clone()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        platforms.sort();
        let key = format!(
            "{:?}|{}",
            settings.weights,
            platforms.iter().map(|p| format!("{p}:{:?}", (env.region_patch)(p))).collect::<Vec<_>>().join(",")
        );
        let stale = match &self.draft_cache {
            Some(c) => c.built_at != b.built_at || c.key != key,
            None => true,
        };
        if stale {
            self.draft_cache = Some(DraftCacheState {
                built_at: b.built_at,
                key,
                weighted: weigh_games(&b.model, &env.data, &env.region_patch, &settings.weights),
                cache: DraftCache::default(),
                used_at: 0,
            });
        }
        let c = self.draft_cache.as_mut().unwrap();
        c.used_at = crate::util::now_ms();
        Some(recommend_draft(&b.model, q, &env.data, &env.excl, &env.region_patch, baseline, &settings, &DraftExtra {
            weighted: Some(&c.weighted),
            cache: Some(&c.cache),
            ..Default::default()
        }))
    }

    pub fn trim_caches(&mut self, idle_ms: i64) -> bool {
        match &self.draft_cache {
            Some(c) if crate::util::now_ms() - c.used_at >= idle_ms => {
                self.draft_cache = None;
                true
            }
            _ => false,
        }
    }

    pub fn keeps_drafts(&self) -> bool {
        self.draft_cache.is_some()
    }

    pub fn evaluated(&mut self) {
        self.attempted_at = 0;
    }
    pub fn options_changed(&mut self) {
        self.attempted_at = 0;
    }
}
