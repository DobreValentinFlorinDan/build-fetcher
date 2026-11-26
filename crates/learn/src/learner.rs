//! The learner: incremental, guarded updates of both model steps from the store's new records.
//!
//! The server calls `train_step` in the background while the user is idle. Each call reads the records after the
//! last one trained on, mixes them with a replay sample of earlier examples, continues training the current models
//! (warm start), evaluates the candidates on the store's held-out validation records, and keeps a candidate only if
//! no metric got worse (`Guardrail`); otherwise the previous checkpoint stays in force. Every call writes one line
//! per model to the learning log.

use crate::backend::{select_backend, Backend};
use crate::checkpoint::{self, Entry, Manifest, Store, LOGISTIC, SHARED_NN, TEAM_NN};
use crate::teamnet::{TeamNet, TeamOptions, TeamSample};
use crate::features::{sample_weight, samples_of, sigmoid, team_sample, ItemClasses, PatchClock, Rng, Sample, Situation, WeightOptions};
use crate::logistic::{LogisticModel, TrainOptions};
use crate::metrics::{evaluation, fmt_metrics, Evaluation, Guardrail};
use crate::nn::{NetOptions, SharedNet};
use anyhow::{anyhow, Result};
use rayon::prelude::*;
use bf_core::build::{Activity, DataSource, MatchRecord, Metrics, ModelStatus, Role};
use bf_core::ddragon::StaticData;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LearnConfig {
    pub weights: WeightOptions,
    pub logistic: TrainOptions,
    pub net: NetOptions,
    pub guardrail: Guardrail,
    /// Model step 2 on or off.
    pub train_net: bool,
    /// Earlier examples kept in memory and mixed into each update (so an update on one patch's games doesn't forget
    /// the rest), and how many per new example.
    pub replay: usize,
    pub replay_share: f64,
    /// Situations kept per champion-role for the recommender.
    pub contexts_per_pair: usize,
    pub seed: u64,
    pub use_gpu: bool,
    /// The GPU's results are checked against the processor's once every this many calls.
    pub verify_every: u64,
    /// Continuous learning (`absorb`).
    #[serde(default)]
    pub continuous: ContinuousOptions,
    /// Model step 3, the team network: dormant until its threshold (1.3.0).
    #[serde(default)]
    pub team: TeamOptions,
}

/// How `absorb` learns: in passes over at least `min_games` new games, with a smaller step than a batch update.
///
/// Measured on synthetic data (docs/MEASUREMENTS.md, 1.2.0): one game at a time at the batch update's full step, a
/// game's examples (one label per team) pulled the draft around so much that 40 checks in a row kept the model in
/// force. With a tenth of the step and one pass, 12 of 40 checks were accepted and the model ended better than with
/// batches of 500 (log loss 0.4911 against 0.4937, on the same 6,000 games).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContinuousOptions {
    pub min_games: usize,
    /// The step size relative to a batch update's, and passes over each group of games.
    pub lr_scale: f64,
    pub epochs: usize,
}

impl Default for ContinuousOptions {
    fn default() -> Self {
        ContinuousOptions { min_games: 1, lr_scale: 0.1, epochs: 1 }
    }
}

impl Default for LearnConfig {
    fn default() -> Self {
        LearnConfig {
            weights: WeightOptions::default(),
            logistic: TrainOptions::default(),
            net: NetOptions::default(),
            guardrail: Guardrail::default(),
            train_net: true,
            replay: 60_000,
            replay_share: 1.0,
            contexts_per_pair: 160,
            seed: 0xbf_1ea2,
            use_gpu: false,
            verify_every: 50,
            continuous: ContinuousOptions::default(),
            team: TeamOptions::default(),
        }
    }
}

/// How much one `train_step` may do.
#[derive(Clone)]
pub struct Budget {
    /// Records read from the store in this step.
    pub max_records: usize,
    /// Wall-clock limit, checked between mini-batches.
    pub deadline: Option<Duration>,
    /// What the user is doing, asked before the step and between mini-batches: anything but Idle stops it.
    pub activity: Arc<dyn Fn() -> Activity + Send + Sync>,
    /// Fewer new records than this: the step waits (`Outcome::Waiting`) and reads nothing, so the records stay for a
    /// later, bigger update. A tiny update can't beat the model in force on noise-sized evidence, and a rejected one
    /// still marks its records as read.
    pub min_records: usize,
    /// The same for the very first update (no model yet): it has to beat the baseline on every metric at once.
    pub first_min_records: usize,
    /// Once a model exists, an update with fewer than `min_records` goes ahead anyway when this long has passed since
    /// the last update (slow fetching: a shared key, few regions).
    pub max_wait: Option<Duration>,
    /// Fewer held-out validation matches than this: the step waits (the guardrail would judge on noise).
    pub min_validation: usize,
}

impl Budget {
    /// Up to `max_records`, no time limit, always idle, no minimums (tests and measurements).
    pub fn records(max_records: usize) -> Budget {
        Budget { max_records, deadline: None, activity: Arc::new(|| Activity::Idle), min_records: 0, first_min_records: 0, max_wait: None, min_validation: 0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    /// At least one model step was accepted.
    Accepted,
    /// Every candidate was worse than what is in force.
    Rejected,
    /// No new records.
    NoData,
    /// Fewer new records (or validation matches) than the budget's minimum: nothing was read, nothing changed.
    Waiting,
    /// Stopped between mini-batches (activity, deadline or `interrupt`): nothing changed except as reported.
    Interrupted,
    /// Not started: the user isn't idle (for `absorb`: in champ select or a game).
    NotIdle,
    /// Continuous learning: games learnt into the draft (`absorb`).
    Learned,
    /// Learning is off.
    Disabled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepReport {
    pub kind: String,
    pub accepted: bool,
    /// The new checkpoint, when accepted.
    pub version: Option<String>,
    /// Metrics of what was in force, and of the candidate, on the same validation examples.
    pub before: Metrics,
    pub after: Metrics,
    /// What got worse, when rejected.
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateReport {
    pub outcome: Outcome,
    pub steps: Vec<StepReport>,
    pub records: usize,
    pub samples: usize,
    pub replayed: usize,
    pub batches: usize,
    pub validation_samples: usize,
    pub train_ms: f64,
    pub eval_ms: f64,
    pub elapsed_ms: f64,
    /// Resident memory after the step, and the process's peak so far (Linux only), in bytes.
    pub rss_bytes: Option<u64>,
    pub peak_rss_bytes: Option<u64>,
    /// Model weights and optimiser state in memory.
    pub model_bytes: usize,
    pub backend: String,
    pub log: Vec<String>,
}

/// The models in force, as the recommender uses them.
pub struct Served {
    pub logistic: Arc<LogisticModel>,
    pub logistic_version: String,
    pub net: Option<Arc<SharedNet>>,
    pub net_version: Option<String>,
    pub contexts: Arc<BTreeMap<String, Vec<Situation>>>,
    /// Step 3, once it has woken up and passed a check (1.3.0).
    pub team: Option<Arc<TeamNet>>,
    pub team_version: Option<String>,
}

impl Served {
    pub fn logit(&self, s: &Situation) -> f64 {
        self.logistic.logit(s) + self.net.as_ref().map(|n| n.residual(s)).unwrap_or(0.0)
    }
    pub fn predict(&self, s: &Situation) -> f64 {
        sigmoid(self.logit(s))
    }
    /// With the team network's residual when it is in force and the whole team is known (in game).
    pub fn predict_team(&self, s: &Situation, team: Option<&TeamSample>) -> f64 {
        let r = match (&self.team, team) {
            (Some(n), Some(t)) => n.residual(t),
            _ => 0.0,
        };
        sigmoid(self.logit(s) + r)
    }
    /// "v12" (step 1), "v12+v13" (step 2 on top of step 1), "+v14" more with step 3.
    pub fn version(&self) -> String {
        let mut v = match &self.net_version {
            Some(n) => format!("{}+{}", self.logistic_version, n),
            None => self.logistic_version.clone(),
        };
        if let Some(t) = &self.team_version {
            v = format!("{v}+{t}");
        }
        v
    }
    /// Training examples the model has seen for a champion-role.
    pub fn pair_samples(&self, champion: i64, role: Role) -> u64 {
        self.logistic.pair_samples.get(&(champion, role.index() as u8)).copied().unwrap_or(0)
    }
    pub fn contexts(&self, champion: i64, role: Role) -> &[Situation] {
        self.contexts.get(&context_key(champion, role)).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

pub fn context_key(champion: i64, role: Role) -> String {
    format!("{champion}:{}", role.as_riot())
}

struct State {
    manifest: Manifest,
    logistic: Option<(String, Arc<LogisticModel>)>,
    net: Option<(String, Arc<SharedNet>)>,
    replay: Vec<Sample>,
    replay_seen: u64,
    contexts: Arc<BTreeMap<String, Vec<Situation>>>,
    /// Continuous learning: the models being trained game by game, not served until a check accepts them.
    draft: Option<Draft>,
    /// The calibration pool (1.2.2): held-out examples of the last `CAL_POOL_MATCHES` calibration records (one record
    /// in `CALIB_HOLDOUT`, never trained on), one entry per match. Kept across checks, rejections and resets, and
    /// refilled from the store once per run, so the temperature line always has enough matches to be fitted. Before,
    /// each draft gathered its own and lost them when reset: at most about 174 matches, under the 250 the fit needs,
    /// so a draft's probabilities were never corrected and every check failed on calibration.
    cal_pool: std::collections::VecDeque<Vec<Sample>>,
    cal_pool_filled: bool,
    /// Step 3 (1.3.0): in force, its draft (once awake), its replay examples, and the stored games with a timeline
    /// when this run started (counted once, for the threshold).
    team: Option<(String, Arc<TeamNet>)>,
    team_draft: Option<TeamNet>,
    team_replay: Vec<(Sample, TeamSample)>,
    team_seen: u64,
    stored_timelines: Option<u64>,
}

/// Calibration records the pool keeps (the newest), and how far back before the cursor it looks when refilled.
const CAL_POOL_MATCHES: usize = 2_000;
const CAL_POOL_SCAN: u64 = 20_000;

/// The draft of continuous learning (in memory only: after a restart, the games since the last accepted model are
/// learnt again from the store, so nothing is lost).
struct Draft {
    logistic: LogisticModel,
    net: Option<SharedNet>,
    /// The last record learnt (the store's cursor moves to it when a check accepts the draft).
    cursor: u64,
    clock: PatchClock,
    /// Records learnt since the last accepted model, and since the last check.
    records: usize,
    since_check: usize,
    /// Checks in a row that kept the model in force.
    rejections: u32,
    last_check: Instant,
    /// Records with a timeline learnt since the last accepted model (they count towards step 3's threshold once
    /// the cursor moves past them).
    timeline: u64,
}

/// How often continuous learning is checked, and when a draft that keeps failing is dropped.
#[derive(Clone, Debug)]
pub struct CheckPolicy {
    /// Learnt games before a check.
    pub every: usize,
    /// A check with fewer games after this long (slow fetching).
    pub max_wait: Option<Duration>,
    /// Fewer held-out validation matches: no check yet.
    pub min_validation: usize,
    /// Checks in a row that kept the model in force before the draft is reset to it.
    pub max_rejections: u32,
}

impl Default for CheckPolicy {
    fn default() -> Self {
        CheckPolicy { every: 150, max_wait: Some(Duration::from_secs(3600)), min_validation: 100, max_rejections: 8 }
    }
}

/// What one `absorb` call did.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AbsorbReport {
    /// `Learned`, or why nothing was learnt (`NoData`, `Waiting` before the first model, `NotIdle` in champ select or
    /// a game, `Disabled`, `Interrupted`).
    pub outcome: Outcome,
    pub records: usize,
    pub samples: usize,
    pub replayed: usize,
    pub train_ms: f64,
    /// Records learnt since the last check, and since the last accepted model.
    pub since_check: usize,
    pub since_accepted: usize,
}

pub struct Learner {
    store: Store,
    data: Arc<StaticData>,
    classes: ItemClasses,
    config: LearnConfig,
    backend: Arc<dyn Backend>,
    state: Mutex<State>,
    manifest: RwLock<Manifest>,
    served: RwLock<Option<Arc<Served>>>,
    stop: AtomicBool,
    learning: AtomicBool,
    /// When an update last ran (accepted or rejected), or when the learner opened: for `Budget::max_wait`.
    last_update: Mutex<Instant>,
    /// What the learner is waiting for, shown first in the status log.
    waiting: Mutex<Option<String>>,
    /// Step 3's state for the status (dormant with its progress, or awake).
    team_status: Mutex<Option<String>>,
}

/// The process's peak resident memory (VmHWM), where the system says it.
pub fn peak_rss() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = s.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// The champion-role win-rate baseline: what the guardrail compares the first model with, and the "no model" row of
/// the measurements. Shrunk toward 50% by 10 games.
pub fn baseline_predictions(src: &dyn DataSource, samples: &[Sample]) -> Result<Vec<f64>> {
    let mut rate: BTreeMap<(i64, Role), (f64, f64)> = BTreeMap::new();
    for v in src.volumes(&[])? {
        let e = rate.entry((v.champion, v.role)).or_insert((0.0, 0.0));
        e.0 += v.wins as f64;
        e.1 += v.games as f64;
    }
    Ok(samples
        .iter()
        .map(|s| {
            let (w, g) = s.situation.role.and_then(|r| rate.get(&(s.situation.champion, r)).copied()).unwrap_or((0.0, 0.0));
            (w + 5.0) / (g + 10.0)
        })
        .collect())
}

pub fn evaluate(p: &[f64], val: &[Sample]) -> Metrics {
    evaluate_full(p, val).metrics
}

/// Metrics with the guardrail's extras (examples and noise floor per window). `games` is the number of examples.
pub fn evaluate_full(p: &[f64], val: &[Sample]) -> Evaluation {
    let y: Vec<f64> = val.iter().map(|s| s.label()).collect();
    let w: Vec<usize> = val.iter().map(|s| s.window()).collect();
    let g: Vec<u64> = val.iter().map(|s| s.group).collect();
    let mut e = evaluation(p, &y, &w, &g);
    e.metrics.games = val.len() as u32;
    e.metrics.matches = val.iter().map(|s| s.game).collect::<std::collections::BTreeSet<u64>>().len() as u32;
    e
}

impl Learner {
    /// Opens (or starts) the checkpoints in `dir` and loads the active models.
    pub fn open(dir: impl AsRef<Path>, data: Arc<StaticData>, config: LearnConfig) -> Result<Learner> {
        let mut log = vec![];
        let backend = select_backend(config.use_gpu, config.verify_every, &mut log);
        let l = Learner::open_with_backend(dir, data, config, backend)?;
        for line in &log {
            l.store.log(line);
        }
        Ok(l)
    }

    /// `open` with a given backend (tests: a stand-in accelerator behind `CheckedBackend`).
    pub fn open_with_backend(dir: impl AsRef<Path>, data: Arc<StaticData>, config: LearnConfig, backend: Arc<dyn Backend>) -> Result<Learner> {
        let store = Store::open(dir.as_ref())?;
        let manifest = store.manifest()?;
        let logistic = match &manifest.active_logistic {
            Some(v) => {
                let e = manifest.entry(v).ok_or_else(|| anyhow!("active checkpoint {v} missing from the manifest"))?;
                Some((v.clone(), Arc::new(store.load_logistic(e)?)))
            }
            None => None,
        };
        let net = match &manifest.active_nn {
            Some(v) => {
                let e = manifest.entry(v).ok_or_else(|| anyhow!("active checkpoint {v} missing from the manifest"))?;
                Some((v.clone(), Arc::new(store.load_nn(e)?)))
            }
            None => None,
        };
        let team = match &manifest.active_team {
            Some(v) => {
                let e = manifest.entry(v).ok_or_else(|| anyhow!("active checkpoint {v} missing from the manifest"))?;
                Some((v.clone(), Arc::new(store.load_team(e)?)))
            }
            None => None,
        };
        let team_draft = if manifest.team_awake { Some(team.as_ref().map(|t| (*t.1).clone()).unwrap_or_else(|| TeamNet::new(config.team.net.seed ^ config.seed))) } else { None };
        let contexts = Arc::new(store.load_contexts());
        let classes = ItemClasses::new(&data);
        let l = Learner {
            store,
            data,
            classes,
            config,
            backend,
            state: Mutex::new(State { manifest: manifest.clone(), logistic, net, replay: vec![], replay_seen: 0, contexts,
                draft: None,
                cal_pool: Default::default(),
                cal_pool_filled: false,
                team,
                team_draft,
                team_replay: vec![],
                team_seen: 0,
                stored_timelines: None,
            }),
            manifest: RwLock::new(manifest),
            served: RwLock::new(None),
            stop: AtomicBool::new(false),
            learning: AtomicBool::new(true),
            last_update: Mutex::new(Instant::now()),
            waiting: Mutex::new(None),
            team_status: Mutex::new(None),
        };
        l.publish(&l.state.lock());
        Ok(l)
    }

    pub fn config(&self) -> &LearnConfig {
        &self.config
    }

    pub fn backend(&self) -> Arc<dyn Backend> {
        self.backend.clone()
    }

    pub fn static_data(&self) -> Arc<StaticData> {
        self.data.clone()
    }

    /// The models in force (None before the first accepted update).
    pub fn served(&self) -> Option<Arc<Served>> {
        self.served.read().clone()
    }

    /// Learning on or off (Settings). Off: `train_step` does nothing.
    pub fn set_learning(&self, on: bool) {
        self.learning.store(on, Ordering::SeqCst);
    }

    /// Stops a running `train_step` at its next mini-batch (champ select started, the app is closing).
    pub fn interrupt(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn status(&self) -> ModelStatus {
        ModelStatus {
            learning: self.learning.load(Ordering::SeqCst),
            backend: self.backend.name(),
            checkpoints: self.manifest.read().infos(),
            log: self.waiting.lock().iter().cloned().chain(self.team_line()).chain(self.store.log_tail(50)).collect(),
        }
    }

    fn publish(&self, st: &State) {
        let served = st.logistic.as_ref().map(|(v, m)| {
            Arc::new(Served {
                logistic: m.clone(),
                logistic_version: v.clone(),
                net: st.net.as_ref().map(|(_, n)| n.clone()),
                net_version: st.net.as_ref().map(|(v, _)| v.clone()),
                contexts: st.contexts.clone(),
                team: st.team.as_ref().map(|(_, n)| n.clone()),
                team_version: st.team.as_ref().map(|(v, _)| v.clone()),
            })
        });
        *self.served.write() = served;
        *self.manifest.write() = st.manifest.clone();
    }

    /// Makes `version` the checkpoint in force for its kind. Rolling step 1 back also rolls step 2 back to the newest
    /// network trained on top of that version (or none). Records already trained on are not read again.
    pub fn rollback(&self, version: &str) -> Result<()> {
        let mut st = self.state.lock();
        let e = st.manifest.entry(version).cloned().ok_or_else(|| anyhow!("no checkpoint {version}"))?;
        // A draft learnt on top of the model being replaced is dropped; its games are learnt again on top of this one.
        st.draft = None;
        *self.waiting.lock() = None;
        if e.kind == TEAM_NN {
            st.team = Some((e.version.clone(), Arc::new(self.store.load_team(&e)?)));
            st.manifest.active_team = Some(e.version.clone());
        } else if e.kind == LOGISTIC {
            // Step 3 was judged on top of the step 1 in force: it goes with it.
            st.team = None;
            st.manifest.active_team = None;
            st.logistic = Some((e.version.clone(), Arc::new(self.store.load_logistic(&e)?)));
            st.manifest.active_logistic = Some(e.version.clone());
            let nn = st.manifest.checkpoints.iter().rev().find(|c| c.kind == SHARED_NN && c.parent.as_deref() == Some(version)).cloned();
            match nn {
                Some(n) => {
                    st.net = Some((n.version.clone(), Arc::new(self.store.load_nn(&n)?)));
                    st.manifest.active_nn = Some(n.version);
                }
                None => {
                    st.net = None;
                    st.manifest.active_nn = None;
                }
            }
        } else {
            st.net = Some((e.version.clone(), Arc::new(self.store.load_nn(&e)?)));
            st.manifest.active_nn = Some(e.version.clone());
        }
        self.store.save_manifest(&st.manifest)?;
        let served = st.manifest.active_logistic.clone().unwrap_or_default() + st.manifest.active_nn.as_ref().map(|n| format!("+{n}")).as_deref().unwrap_or("");
        self.store.log(&format!("rolled back to {version} ({}); serving {served}", e.kind));
        self.publish(&st);
        Ok(())
    }

    fn logits_of(&self, m: &LogisticModel, xs: &[Sample]) -> Vec<f64> {
        let refs: Vec<&Situation> = xs.iter().map(|s| &s.situation).collect();
        m.logits(&refs, self.backend.as_ref())
    }

    fn residuals_of(n: &SharedNet, xs: &[Sample]) -> Vec<f64> {
        let refs: Vec<&Situation> = xs.iter().map(|s| &s.situation).collect();
        n.residuals(&refs)
    }

    /// Scores the models in force again on today's held-out sample, when it has more matches than when they were
    /// scored (the sample grows as games arrive, so early scores rest on very few matches). Only the shown metrics
    /// change; the models don't. Returns the log line, or None when there was nothing to do.
    pub fn rescore(&self, src: &dyn DataSource) -> Result<Option<String>> {
        let mut st = self.state.lock();
        let Some((lv, lm)) = st.logistic.clone() else { return Ok(None) };
        let val: Vec<Sample> = src.validation_records()?.iter().flat_map(|r| samples_of(r, &self.data, &self.classes)).collect();
        if val.is_empty() {
            return Ok(None);
        }
        let matches = val.iter().map(|s| s.game).collect::<std::collections::BTreeSet<u64>>().len() as u32;
        let scored = st.manifest.entry(&lv).map(|e| e.metrics.matches).unwrap_or(0);
        if matches <= scored {
            return Ok(None);
        }
        let z = self.logits_of(&lm, &val);
        let m_l = evaluate_full(&z.iter().map(|x| sigmoid(*x)).collect::<Vec<_>>(), &val).metrics;
        let m_nn = st.net.as_ref().map(|(v, n)| {
            let r = Self::residuals_of(n, &val);
            (v.clone(), evaluate_full(&z.iter().zip(&r).map(|(a, b)| sigmoid(a + b)).collect::<Vec<_>>(), &val).metrics)
        });
        let mut line = String::new();
        for (v, m) in std::iter::once((lv.clone(), m_l)).chain(m_nn) {
            if let Some(e) = st.manifest.checkpoints.iter_mut().find(|e| e.version == v) {
                line.push_str(&format!(
                    "{}{v} re-scored on {} test matches (was {}): {} -> {}",
                    if line.is_empty() { "" } else { "; " },
                    m.matches,
                    if e.metrics.matches > 0 { e.metrics.matches.to_string() } else { format!("{} examples", e.metrics.games) },
                    fmt_metrics(&e.metrics),
                    fmt_metrics(&m)
                ));
                e.metrics = m;
            }
        }
        self.store.save_manifest(&st.manifest)?;
        self.publish(&st);
        self.store.log(&line);
        Ok(Some(line))
    }

    /// A model is in force (continuous learning starts from it; the first one comes from `train_step`).
    pub fn has_model(&self) -> bool {
        self.state.lock().logistic.is_some()
    }

    /// Continuous learning, step 1 of 2: learns the games that arrived since the last call into the draft (both model
    /// steps, warm, with as many replayed examples as new ones). Nothing is served or saved; `check` decides. Runs
    /// unless the user is in champ select or a game (`activity`), and stops at the deadline. Before the first model it
    /// does nothing (`Waiting`): that one comes from `train_step`, on a big batch.
    pub fn absorb(&self, src: &dyn DataSource, max_records: usize, deadline: Option<Duration>, activity: Arc<dyn Fn() -> Activity + Send + Sync>) -> Result<AbsorbReport> {
        let t0 = Instant::now();
        let mut rep = AbsorbReport { outcome: Outcome::NoData, records: 0, samples: 0, replayed: 0, train_ms: 0.0, since_check: 0, since_accepted: 0 };
        let busy = |a: Activity| matches!(a, Activity::ChampSelect | Activity::InGame);
        if !self.learning.load(Ordering::SeqCst) {
            rep.outcome = Outcome::Disabled;
            return Ok(rep);
        }
        if busy(activity()) {
            rep.outcome = Outcome::NotIdle;
            return Ok(rep);
        }
        let mut st = self.state.lock();
        let Some((_, in_force)) = st.logistic.clone() else {
            rep.outcome = Outcome::Waiting;
            return Ok(rep);
        };
        self.stop.store(false, Ordering::SeqCst);
        self.pool_fill(&mut st, src)?;
        if st.stored_timelines.is_none() {
            st.stored_timelines = Some(src.timeline_count()?);
        }
        let start = st.draft.as_ref().map(|d| d.cursor).unwrap_or(st.manifest.cursor);
        let mut records = vec![];
        let mut cursor = start;
        while records.len() < max_records {
            let page = src.records_after(cursor, (max_records - records.len()).min(1000))?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map(|p| p.0).unwrap_or(cursor);
            records.extend(page.into_iter().map(|p| p.1));
        }
        if let Some(d) = &st.draft {
            rep.since_check = d.since_check;
            rep.since_accepted = d.records;
        }
        if records.is_empty() {
            return Ok(rep);
        }
        let co = &self.config.continuous;
        if records.len() < co.min_games {
            // Too few for a pass: nothing is read, they are learnt with the next ones.
            rep.outcome = Outcome::Waiting;
            return Ok(rep);
        }
        let mut lo = self.config.logistic.clone();
        lo.learning_rate *= co.lr_scale;
        lo.epochs = co.epochs;
        let mut no = self.config.net.clone();
        no.learning_rate *= co.lr_scale;
        no.epochs = co.epochs;
        let deadline = deadline.map(|d| t0 + d);
        let stop_flag = &self.stop;
        let act = activity.clone();
        let stop = move || stop_flag.load(Ordering::SeqCst) || busy(act()) || deadline.map(|d| Instant::now() > d).unwrap_or(false);
        let mut draft = match st.draft.take() {
            Some(d) => d,
            None => Draft {
                logistic: (*in_force).clone(),
                net: st.net.as_ref().map(|(_, n)| (**n).clone()),
                cursor: start,
                clock: st.manifest.clock.clone(),
                records: 0,
                since_check: 0,
                rejections: 0,
                last_check: Instant::now(),
                timeline: 0,
            },
        };
        for r in &records {
            if let Some(v) = r.version {
                draft.clock.observe(v);
            }
        }
        if let Some(p) = src.current_patch()? {
            draft.clock.observe_patch(p);
        }
        let (cal_records, train_records): (Vec<&MatchRecord>, Vec<&MatchRecord>) = records.iter().partition(|r| fnv(&r.match_id) % CALIB_HOLDOUT == 0);
        let per_record: Vec<Vec<Sample>> = train_records.iter().map(|r| samples_of(r, &self.data, &self.classes)).collect();
        let fresh: Vec<Sample> = per_record.iter().flatten().cloned().collect();
        let cal: Vec<Sample> = cal_records.iter().flat_map(|r| samples_of(r, &self.data, &self.classes)).collect();
        let n_timeline = records.iter().filter(|r| r.has_timeline()).count() as u64;
        let mut rng = Rng(self.config.seed ^ cursor.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let fresh_n = fresh.len();
        let mut train = fresh;
        let k = ((fresh_n as f64 * self.config.replay_share) as usize).min(st.replay.len());
        let mut pick: Vec<usize> = (0..st.replay.len()).collect();
        rng.shuffle(&mut pick);
        train.extend(pick[..k].iter().map(|i| st.replay[*i].clone()));
        let weights: Vec<f64> = train.iter().map(|s| sample_weight(s, &draft.clock, &self.config.weights)).collect();
        let mut order: Vec<usize> = (0..train.len()).collect();
        rng.shuffle(&mut order);
        // On copies, so an interrupted call leaves the draft as it was (its games are learnt again next time).
        let mut lm = draft.logistic.clone();
        let done = lm.train(&train, &weights, &order, &lo, self.backend.as_ref(), &stop).is_ok();
        let mut net = draft.net.clone();
        let done = done
            && match (&mut net, self.config.train_net) {
                (Some(n), true) => {
                    let base = self.logits_of(&lm, &train);
                    n.train(&train, &base, &weights, &order, &no, &stop).is_ok()
                }
                (None, true) => {
                    let mut n = SharedNet::new(self.config.net.seed ^ self.config.seed);
                    let base = self.logits_of(&lm, &train);
                    let ok = n.train(&train, &base, &weights, &order, &no, &stop).is_ok();
                    net = Some(n);
                    ok
                }
                _ => true,
            };
        rep.train_ms = t0.elapsed().as_secs_f64() * 1e3;
        if !done {
            rep.outcome = Outcome::Interrupted;
            st.draft = Some(draft);
            return Ok(rep);
        }
        draft.logistic = lm;
        draft.net = net;
        draft.timeline += n_timeline;
        // Step 3: wake it up once there are enough games with a timeline, then let it learn these games and a slice of
        // the stored backlog.
        let progress = (st.manifest.timeline_seen + draft.timeline).max(st.stored_timelines.unwrap_or(0));
        if !st.manifest.team_awake && progress >= self.config.team.threshold {
            st.manifest.team_awake = true;
            st.team_draft = Some(TeamNet::new(self.config.team.net.seed ^ self.config.seed));
            self.store.log(&format!("team network woke up: {progress} games with a timeline (threshold {})", self.config.team.threshold));
        }
        if st.manifest.team_awake {
            let mut pairs: Vec<(Sample, TeamSample)> = vec![];
            for (r, v) in train_records.iter().zip(&per_record) {
                pairs.extend(v.iter().filter_map(|s| team_sample(r, s, &self.data, &self.classes).map(|t| (s.clone(), t))));
            }
            let fresh_pairs = pairs.len();
            // The backlog: games already read by steps 1 and 2 (never the calibration ones), oldest first.
            let end = st.manifest.cursor;
            if st.manifest.team_cursor < end {
                let page = src.records_after(st.manifest.team_cursor, self.config.team.backlog_per_call)?;
                for (seq, r) in &page {
                    if *seq > end {
                        break;
                    }
                    st.manifest.team_cursor = *seq;
                    if fnv(&r.match_id) % CALIB_HOLDOUT == 0 || !r.has_timeline() {
                        continue;
                    }
                    pairs.extend(samples_of(r, &self.data, &self.classes).into_iter().filter_map(|s| team_sample(r, &s, &self.data, &self.classes).map(|t| (s, t))));
                }
            }
            let new_pairs = pairs.len();
            let k = new_pairs.min(st.team_replay.len());
            let mut pick: Vec<usize> = (0..st.team_replay.len()).collect();
            rng.shuffle(&mut pick);
            pairs.extend(pick[..k].iter().map(|i| st.team_replay[*i].clone()));
            if !pairs.is_empty() {
                let sits: Vec<Sample> = pairs.iter().map(|p| p.0.clone()).collect();
                let mut base = self.logits_of(&draft.logistic, &sits);
                if let Some(n) = &draft.net {
                    for (b, r) in base.iter_mut().zip(Self::residuals_of(n, &sits)) {
                        *b += r;
                    }
                }
                let labels: Vec<f64> = sits.iter().map(|s| s.label()).collect();
                let w: Vec<f64> = sits.iter().map(|s| sample_weight(s, &draft.clock, &self.config.weights)).collect();
                let teams: Vec<TeamSample> = pairs.iter().map(|p| p.1.clone()).collect();
                let mut ord: Vec<usize> = (0..teams.len()).collect();
                rng.shuffle(&mut ord);
                let mut tn = st.team_draft.clone().unwrap_or_else(|| TeamNet::new(self.config.team.net.seed ^ self.config.seed));
                if tn.train(&teams, &labels, &base, &w, &ord, &self.config.team.net, &stop).is_ok() {
                    st.team_draft = Some(tn);
                    st.team_seen += (fresh_pairs > 0) as u64;
                }
                // Reservoir of the new examples for replay.
                let cap = self.config.team.replay;
                for p in pairs.into_iter().take(new_pairs) {
                    if st.team_replay.len() < cap {
                        st.team_replay.push(p);
                    } else if cap > 0 {
                        let j = (rng.next_u64() % cap as u64) as usize;
                        st.team_replay[j] = p;
                    }
                }
            }
        }
        *self.team_status.lock() = Some(self.team_text(&st, progress));
        draft.cursor = cursor;
        draft.records += records.len();
        draft.since_check += records.len();
        Self::pool_add(&mut st, cal);
        self.remember(&mut st, &train[..fresh_n], &mut rng);
        rep.outcome = Outcome::Learned;
        rep.records = records.len();
        rep.samples = fresh_n;
        rep.replayed = k;
        rep.since_check = draft.since_check;
        rep.since_accepted = draft.records;
        *self.waiting.lock() = Some(self.draft_line(&st, &draft, None));
        st.draft = Some(draft);
        Ok(rep)
    }

    /// Adds calibration examples to the pool, one entry per match, dropping the oldest past `CAL_POOL_MATCHES`.
    fn pool_add(st: &mut State, cal: Vec<Sample>) {
        // A record's examples come together, so a new match starts wherever the match id changes.
        let mut by_game: Vec<(u64, Vec<Sample>)> = vec![];
        for s in cal {
            match by_game.last_mut() {
                Some(g) if g.0 == s.game => g.1.push(s),
                _ => by_game.push((s.game, vec![s])),
            }
        }
        for (_, v) in by_game {
            st.cal_pool.push_back(v);
        }
        while st.cal_pool.len() > CAL_POOL_MATCHES {
            st.cal_pool.pop_front();
        }
    }

    /// Once per run: the calibration records among the last `CAL_POOL_SCAN` records already read (those after the
    /// cursor come in through `absorb`).
    fn pool_fill(&self, st: &mut State, src: &dyn DataSource) -> Result<()> {
        if st.cal_pool_filled {
            return Ok(());
        }
        st.cal_pool_filled = true;
        let end = st.manifest.cursor;
        let mut at = end.saturating_sub(CAL_POOL_SCAN);
        let mut cal = vec![];
        while at < end {
            let page = src.records_after(at, 1000)?;
            let Some(last) = page.last().map(|p| p.0) else { break };
            for (seq, r) in page {
                if seq <= end && fnv(&r.match_id) % CALIB_HOLDOUT == 0 {
                    cal.extend(samples_of(&r, &self.data, &self.classes));
                }
            }
            at = last;
        }
        let n = st.cal_pool.len();
        Self::pool_add(st, cal);
        self.store.log(&format!("calibration pool: {} held-out matches from the store", st.cal_pool.len() - n));
        Ok(())
    }

    fn team_text(&self, st: &State, progress: u64) -> String {
        let th = self.config.team.threshold;
        if !st.manifest.team_awake {
            format!(
                "Team network: dormant, {} of {} games with a timeline ({:.0}%). It wakes up on its own at {}.",
                progress,
                th,
                100.0 * progress as f64 / th.max(1) as f64,
                th
            )
        } else {
            let backlog = st.manifest.cursor.saturating_sub(st.manifest.team_cursor);
            let force = st.team.as_ref().map(|(v, _)| format!("{v} in force")).unwrap_or_else(|| "none in force yet".into());
            format!("Team network: awake, learning ({force}); stored games still to go through: about {backlog}.")
        }
    }

    /// Step 3's line for the status (None before the first continuous-learning call of this run).
    fn team_line(&self) -> Option<String> {
        self.team_status.lock().clone()
    }

    /// The guardrail on a step 3 candidate, on top of steps 1 + 2 in force: it must beat them without it, and the
    /// team network in force if there is one. `val` is `val_records`' examples in order.
    fn judge_team(&self, st: &mut State, net: TeamNet, val: &[Sample], val_records: &[MatchRecord], n_records: usize, report: &mut UpdateReport) -> Result<bool> {
        let Some((lv, lm)) = st.logistic.clone() else { return Ok(false) };
        let te = Instant::now();
        let mut z = self.logits_of(&lm, val);
        if let Some((_, n)) = &st.net {
            for (a, r) in z.iter_mut().zip(Self::residuals_of(n, val)) {
                *a += r;
            }
        }
        let old = st.team.clone();
        // Residuals record by record (a team example is ten players: building them all at once would take hundreds of
        // megabytes for a full held-out sample).
        let pairs: Vec<Vec<(f64, f64)>> = val_records
            .par_iter()
            .map(|r| {
                samples_of(r, &self.data, &self.classes)
                    .iter()
                    .map(|s| match team_sample(r, s, &self.data, &self.classes) {
                        Some(t) => (net.residual(&t), old.as_ref().map(|o| o.1.residual(&t)).unwrap_or(0.0)),
                        None => (0.0, 0.0),
                    })
                    .collect()
            })
            .collect();
        let pairs: Vec<(f64, f64)> = pairs.into_iter().flatten().collect();
        if pairs.len() != val.len() {
            return Err(anyhow!("team network: {} held-out examples against {}", pairs.len(), val.len()));
        }
        let m_base = evaluate_full(&z.iter().map(|a| sigmoid(*a)).collect::<Vec<_>>(), val);
        let m_old = old.as_ref().map(|_| evaluate_full(&z.iter().zip(&pairs).map(|(a, p)| sigmoid(a + p.1)).collect::<Vec<_>>(), val));
        let m_new = evaluate_full(&z.iter().zip(&pairs).map(|(a, p)| sigmoid(a + p.0)).collect::<Vec<_>>(), val);
        report.eval_ms += te.elapsed().as_secs_f64() * 1e3;
        let mut why = self.config.guardrail.worse(&m_base.metrics, &m_new).map(|w| format!("vs steps 1 + 2: {w}"));
        if why.is_none() {
            if let (Some((ov, _)), Some(mo)) = (&old, &m_old) {
                why = self.config.guardrail.worse(&mo.metrics, &m_new).map(|w| format!("vs {ov}: {w}"));
            }
        }
        let reference = m_old.as_ref().map(|m| m.metrics.clone()).unwrap_or(m_base.metrics.clone());
        let mut step = StepReport { kind: TEAM_NN.into(), accepted: why.is_none(), version: None, before: reference.clone(), after: m_new.metrics.clone(), reason: why.clone() };
        let accepted = match why {
            None => {
                let v = Self::next_version(st);
                let (file, bytes) = self.store.write_model(&v, TEAM_NN, &checkpoint::encode_team(&net))?;
                st.manifest.checkpoints.push(Entry {
                    version: v.clone(),
                    kind: TEAM_NN.into(),
                    created_ms: bf_core::util::now_ms(),
                    metrics: m_new.metrics.clone(),
                    records_seen: st.manifest.records_seen + n_records as u64,
                    file,
                    bytes,
                    parent: Some(lv.clone()),
                });
                st.manifest.active_team = Some(v.clone());
                report.log.push(format!("{v} team network accepted on top of {}: {} -> {}", lv, fmt_metrics(&reference), fmt_metrics(&m_new.metrics)));
                step.version = Some(v.clone());
                st.team = Some((v, Arc::new(net)));
                true
            }
            Some(w) => {
                report.log.push(format!("team network update rejected: {w}"));
                false
            }
        };
        report.steps.push(step);
        Ok(accepted)
    }

    fn draft_line(&self, st: &State, d: &Draft, policy: Option<&CheckPolicy>) -> String {
        let every = policy.map(|p| p.every).unwrap_or(CheckPolicy::default().every);
        let v = st.logistic.as_ref().map(|(v, _)| v.as_str()).unwrap_or("the baseline");
        format!(
            "Learning game by game: {} new games since {v}; the next check is at {} games since the last one ({} so far).",
            d.records,
            every,
            d.since_check
        )
    }

    /// Continuous learning, step 2 of 2: when the draft has learnt `policy.every` games since the last check (or some
    /// games and `max_wait` has passed), the guardrail compares it with the models in force. Accepted: it is served
    /// and saved, and the store's cursor moves past its games. Rejected: it keeps learning; after `max_rejections`
    /// checks in a row it is reset to the model in force and its games count as read (they stay in the replay
    /// sample). None when no check was due.
    pub fn check(&self, src: &dyn DataSource, policy: &CheckPolicy) -> Result<Option<UpdateReport>> {
        let t0 = Instant::now();
        let mut st = self.state.lock();
        self.pool_fill(&mut st, src)?;
        let Some(mut d) = st.draft.take() else { return Ok(None) };
        let overdue = policy.max_wait.map(|w| d.last_check.elapsed() >= w).unwrap_or(false);
        if d.since_check == 0 || (d.since_check < policy.every && !overdue) {
            st.draft = Some(d);
            return Ok(None);
        }
        let val_records = src.validation_records()?;
        if val_records.len() < policy.min_validation {
            *self.waiting.lock() = Some(format!("Waiting for {} held-out games to check updates on ({} so far).", policy.min_validation, val_records.len()));
            st.draft = Some(d);
            return Ok(None);
        }
        let val: Vec<Sample> = val_records.iter().flat_map(|r| samples_of(r, &self.data, &self.classes)).collect();
        let mut report = UpdateReport {
            outcome: Outcome::Rejected,
            steps: vec![],
            records: d.since_check,
            samples: 0,
            replayed: 0,
            batches: 0,
            validation_samples: val.len(),
            train_ms: 0.0,
            eval_ms: 0.0,
            elapsed_ms: 0.0,
            rss_bytes: None,
            peak_rss_bytes: None,
            model_bytes: 0,
            backend: self.backend.name(),
            log: vec![],
        };
        let mut cand = d.logistic.clone();
        let cal: Vec<Sample> = st.cal_pool.iter().flatten().cloned().collect();
        let cal_w: Vec<f64> = cal.iter().map(|s| sample_weight(s, &d.clock, &self.config.weights)).collect();
        cand.fit_calibration(&cal, &cal_w, self.backend.as_ref());
        let detail = format!("{} games learnt one by one since the model in force ({} checks kept it)", d.records, d.rejections);
        let accepted = self.judge_logistic(&mut st, src, cand.clone(), &val, d.records, &detail, &mut report)?;
        let mut accepted_any = accepted;
        if accepted {
            d.logistic = cand;
            if let Some(n) = d.net.clone() {
                // The draft's network learnt on top of the draft's step 1, which is now the one in force.
                accepted_any |= self.judge_net(&mut st, n, &val, d.records, &mut report)?;
            }
        }
        if st.manifest.team_awake {
            if let Some(tn) = st.team_draft.clone().filter(|t| t.t > 0) {
                accepted_any |= self.judge_team(&mut st, tn, &val, &val_records, d.records, &mut report)?;
            }
        }
        drop(val_records);
        d.since_check = 0;
        d.last_check = Instant::now();
        if accepted {
            st.manifest.cursor = d.cursor;
            st.manifest.records_seen += d.records as u64;
            st.manifest.timeline_seen += d.timeline;
            st.manifest.clock = d.clock.clone();
            d.records = 0;
            d.timeline = 0;
            d.rejections = 0;
            st.draft = Some(d);
        } else {
            d.rejections += 1;
            if d.rejections >= policy.max_rejections {
                report.log.push(format!(
                    "the draft failed {} checks in a row: it starts again from the model in force; its {} games count as read",
                    d.rejections, d.records
                ));
                st.manifest.cursor = d.cursor;
                st.manifest.records_seen += d.records as u64;
                st.manifest.timeline_seen += d.timeline;
                st.manifest.clock = d.clock.clone();
            } else {
                st.draft = Some(d);
            }
        }
        self.store.prune(&mut st.manifest);
        self.store.save_manifest(&st.manifest)?;
        self.store.save_contexts(&st.contexts)?;
        *self.last_update.lock() = Instant::now();
        report.outcome = if accepted_any { Outcome::Accepted } else { Outcome::Rejected };
        *self.waiting.lock() = st.draft.as_ref().map(|d| self.draft_line(&st, d, Some(policy)));
        self.finish(&mut report, t0, &st);
        Ok(Some(report))
    }

    /// One guarded update (see the module notes). Call it only while the user is idle; it stops between mini-batches
    /// when `budget.activity` says otherwise, the deadline passes, or `interrupt` is called.
    pub fn train_step(&self, src: &dyn DataSource, budget: &Budget) -> Result<UpdateReport> {
        let t0 = Instant::now();
        let mut report = UpdateReport {
            outcome: Outcome::NoData,
            steps: vec![],
            records: 0,
            samples: 0,
            replayed: 0,
            batches: 0,
            validation_samples: 0,
            train_ms: 0.0,
            eval_ms: 0.0,
            elapsed_ms: 0.0,
            rss_bytes: None,
            peak_rss_bytes: None,
            model_bytes: 0,
            backend: self.backend.name(),
            log: vec![],
        };
        if !self.learning.load(Ordering::SeqCst) {
            report.outcome = Outcome::Disabled;
            return Ok(report);
        }
        if (budget.activity)() != Activity::Idle {
            report.outcome = Outcome::NotIdle;
            return Ok(report);
        }
        let mut st = self.state.lock();
        self.stop.store(false, Ordering::SeqCst);
        let deadline = budget.deadline.map(|d| t0 + d);
        let activity = budget.activity.clone();
        let stop_flag = &self.stop;
        let stop = move || stop_flag.load(Ordering::SeqCst) || activity() != Activity::Idle || deadline.map(|d| Instant::now() > d).unwrap_or(false);

        // New records.
        let mut records = vec![];
        let mut cursor = st.manifest.cursor;
        while records.len() < budget.max_records {
            let page = src.records_after(cursor, (budget.max_records - records.len()).min(1000))?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map(|p| p.0).unwrap_or(cursor);
            records.extend(page.into_iter().map(|p| p.1));
        }
        report.records = records.len();
        if records.is_empty() {
            return Ok(report);
        }
        // Wait for enough new records: nothing is read until then (the cursor is only saved at the end of a step).
        let have_model = st.logistic.is_some();
        let need = if have_model { budget.min_records } else { budget.first_min_records.max(budget.min_records) }.min(budget.max_records);
        if records.len() < need {
            let waited = self.last_update.lock().elapsed();
            let overdue = have_model && budget.max_wait.map(|w| waited >= w).unwrap_or(false);
            if !overdue {
                *self.waiting.lock() = Some(format!(
                    "Waiting for {} new games before the {} update ({} so far).",
                    need,
                    if have_model { "next" } else { "first" },
                    records.len()
                ));
                report.outcome = Outcome::Waiting;
                return Ok(report);
            }
        }
        let val_records = src.validation_records()?;
        if val_records.len() < budget.min_validation {
            *self.waiting.lock() = Some(format!(
                "Waiting for {} held-out games to check updates on ({} so far).",
                budget.min_validation,
                val_records.len()
            ));
            report.outcome = Outcome::Waiting;
            return Ok(report);
        }
        *self.waiting.lock() = None;
        let mut clock = st.manifest.clock.clone();
        for r in &records {
            if let Some(v) = r.version {
                clock.observe(v);
            }
        }
        if let Some(p) = src.current_patch()? {
            clock.observe_patch(p);
        }
        // About one record in CALIB_HOLDOUT (by match id, so the same data split the same way) is kept out of training
        // to fit the calibration; the validation sample stays untouched for the guardrail.
        let (cal_records, train_records): (Vec<&MatchRecord>, Vec<&MatchRecord>) = records.iter().partition(|r| fnv(&r.match_id) % CALIB_HOLDOUT == 0);
        let fresh: Vec<Sample> = train_records.iter().flat_map(|r| samples_of(r, &self.data, &self.classes)).collect();
        let cal: Vec<Sample> = cal_records.iter().flat_map(|r| samples_of(r, &self.data, &self.classes)).collect();
        let n_records = records.len();
        let n_timeline_read = records.iter().filter(|r| r.has_timeline()).count() as u64;
        drop(records);
        let val: Vec<Sample> = val_records.iter().flat_map(|r| samples_of(r, &self.data, &self.classes)).collect();
        drop(val_records);
        report.samples = fresh.len();
        report.validation_samples = val.len();

        // Training set: the new examples and a replay sample, shuffled; the same data give the same order.
        let step_seed = self.config.seed ^ st.manifest.next.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let mut rng = Rng(step_seed);
        let fresh_n = fresh.len();
        let mut train = fresh;
        let k = ((fresh_n as f64 * self.config.replay_share) as usize).min(st.replay.len());
        let mut pick: Vec<usize> = (0..st.replay.len()).collect();
        rng.shuffle(&mut pick);
        train.extend(pick[..k].iter().map(|i| st.replay[*i].clone()));
        report.replayed = k;
        let weights: Vec<f64> = train.iter().map(|s| sample_weight(s, &clock, &self.config.weights)).collect();
        let mut order: Vec<usize> = (0..train.len()).collect();
        rng.shuffle(&mut order);

        // Step 1.
        let tt = Instant::now();
        let mut cand = st.logistic.as_ref().map(|(_, m)| (**m).clone()).unwrap_or_default();
        let r1 = cand.train(&train, &weights, &order, &self.config.logistic, self.backend.as_ref(), &stop);
        report.train_ms += tt.elapsed().as_secs_f64() * 1e3;
        let batches = match r1 {
            Ok(b) => b,
            Err(b) => {
                report.batches = b;
                report.outcome = Outcome::Interrupted;
                report.log.push(format!("update interrupted after {b} mini-batches of step 1; nothing changed, {n_records} records will be read again"));
                self.finish(&mut report, t0, &st);
                return Ok(report);
            }
        };
        report.batches += batches;
        let cal_w: Vec<f64> = cal.iter().map(|s| sample_weight(s, &clock, &self.config.weights)).collect();
        cand.fit_calibration(&cal, &cal_w, self.backend.as_ref());
        Self::pool_add(&mut st, cal);
        let detail = format!("{} records, {} examples (+{} replayed), {} mini-batches", n_records, fresh_n, k, batches);
        let mut accepted_any = self.judge_logistic(&mut st, src, cand, &val, n_records, &detail, &mut report)?;

        // Step 2, on top of the step 1 now in force.
        let mut interrupted = false;
        if self.config.train_net {
            if let Some((_, lm)) = st.logistic.clone() {
                let tt = Instant::now();
                let base = self.logits_of(&lm, &train);
                let mut net = st.net.as_ref().map(|(_, n)| (**n).clone()).unwrap_or_else(|| SharedNet::new(self.config.net.seed ^ self.config.seed));
                let r2 = net.train(&train, &base, &weights, &order, &self.config.net, &stop);
                report.train_ms += tt.elapsed().as_secs_f64() * 1e3;
                match r2 {
                    Err(b) => {
                        report.batches += b;
                        interrupted = true;
                        report.log.push(format!("step 2 interrupted after {b} mini-batches; its update was dropped"));
                    }
                    Ok(b) => {
                        report.batches += b;
                        accepted_any |= self.judge_net(&mut st, net, &val, n_records, &mut report)?;
                    }
                }
            }
        }

        // The records count as read whatever the guardrails decided: they go into the replay sample and the contexts.
        st.manifest.cursor = cursor;
        st.manifest.records_seen += n_records as u64;
        st.manifest.timeline_seen += n_timeline_read;
        st.manifest.clock = clock;
        self.remember(&mut st, &train[..fresh_n], &mut rng);
        self.store.prune(&mut st.manifest);
        self.store.save_manifest(&st.manifest)?;
        self.store.save_contexts(&st.contexts)?;
        *self.last_update.lock() = Instant::now();
        report.outcome = if interrupted {
            Outcome::Interrupted
        } else if accepted_any {
            Outcome::Accepted
        } else {
            Outcome::Rejected
        };
        self.finish(&mut report, t0, &st);
        Ok(report)
    }

    /// The guardrail on a step 1 candidate: against the model in force (or the champion-role baseline before the
    /// first model). Accepted: committed, served from now on, and true.
    #[allow(clippy::too_many_arguments)]
    fn judge_logistic(&self, st: &mut State, src: &dyn DataSource, cand: LogisticModel, val: &[Sample], n_records: usize, detail: &str, report: &mut UpdateReport) -> Result<bool> {
        let te = Instant::now();
        let before = match &st.logistic {
            Some((_, m)) => evaluate(&self.logits_of(m, val).iter().map(|z| sigmoid(*z)).collect::<Vec<_>>(), val),
            None => evaluate(&baseline_predictions(src, val)?, val),
        };
        let z_cand = self.logits_of(&cand, val);
        let after_full = evaluate_full(&z_cand.iter().map(|z| sigmoid(*z)).collect::<Vec<_>>(), val);
        let after = after_full.metrics.clone();
        report.eval_ms += te.elapsed().as_secs_f64() * 1e3;
        let reason = self.config.guardrail.worse(&before, &after_full);
        let what_before = st.logistic.as_ref().map(|(v, _)| v.clone()).unwrap_or_else(|| "the champion-role win rate".into());
        let mut step = StepReport { kind: LOGISTIC.into(), accepted: reason.is_none(), version: None, before: before.clone(), after: after.clone(), reason: reason.clone() };
        let accepted = match &reason {
            None => {
                let v = self.commit_logistic(st, &cand, &after, n_records as u64)?;
                report.log.push(format!(
                    "{v} logistic accepted over {what_before}: {} -> {}; {detail}; calibration temperature {:.3} + {:.3} per 10 min",
                    fmt_metrics(&before),
                    fmt_metrics(&after),
                    cand.calib[0],
                    cand.calib[1]
                ));
                step.version = Some(v.clone());
                st.logistic = Some((v, Arc::new(cand)));
                true
            }
            Some(why) => {
                report.log.push(format!("logistic update rejected, keeping {what_before}: {why} ({} -> {}); {n_records} records", fmt_metrics(&before), fmt_metrics(&after)));
                false
            }
        };
        report.steps.push(step);
        Ok(accepted)
    }

    /// The guardrail on a step 2 candidate, on top of the step 1 in force: it must beat step 1 alone, and the network
    /// in force if there is one. A network in force that no longer beats step 1 stops being served.
    fn judge_net(&self, st: &mut State, net: SharedNet, val: &[Sample], n_records: usize, report: &mut UpdateReport) -> Result<bool> {
        let Some((lv, lm)) = st.logistic.clone() else { return Ok(false) };
        let te = Instant::now();
        let z_l = self.logits_of(&lm, val);
        let m_l_full = evaluate_full(&z_l.iter().map(|z| sigmoid(*z)).collect::<Vec<_>>(), val);
        let m_l = m_l_full.metrics.clone();
        let old = st.net.as_ref().map(|(v, n)| {
            let r = Self::residuals_of(n, val);
            (v.clone(), evaluate_full(&z_l.iter().zip(&r).map(|(z, r)| sigmoid(z + r)).collect::<Vec<_>>(), val))
        });
        let r_new = Self::residuals_of(&net, val);
        let m_new_full = evaluate_full(&z_l.iter().zip(&r_new).map(|(z, r)| sigmoid(z + r)).collect::<Vec<_>>(), val);
        let m_new = m_new_full.metrics.clone();
        report.eval_ms += te.elapsed().as_secs_f64() * 1e3;
        let mut why = self.config.guardrail.worse(&m_l, &m_new_full).map(|w| format!("vs step 1 {lv}: {w}"));
        if why.is_none() {
            if let Some((ov, om)) = &old {
                why = self.config.guardrail.worse(&om.metrics, &m_new_full).map(|w| format!("vs {ov}: {w}"));
            }
        }
        let reference = old.as_ref().map(|o| o.1.metrics.clone()).unwrap_or(m_l.clone());
        let mut step = StepReport { kind: SHARED_NN.into(), accepted: why.is_none(), version: None, before: reference.clone(), after: m_new.clone(), reason: why.clone() };
        let accepted = match why {
            None => {
                let v = self.commit_nn(st, &net, &m_new, &lv, n_records as u64)?;
                report.log.push(format!("{v} shared-nn accepted on top of {lv}: {} -> {}", fmt_metrics(&reference), fmt_metrics(&m_new)));
                step.version = Some(v.clone());
                st.net = Some((v, Arc::new(net)));
                true
            }
            Some(w) => {
                report.log.push(format!("shared-nn update rejected: {w}"));
                if let Some((ov, om)) = &old {
                    if let Some(w2) = self.config.guardrail.worse(&m_l, om) {
                        report.log.push(format!("{ov} no longer beats {lv} ({w2}); serving step 1 alone"));
                        st.net = None;
                        st.manifest.active_nn = None;
                    }
                }
                false
            }
        };
        report.steps.push(step);
        Ok(accepted)
    }

    fn finish(&self, report: &mut UpdateReport, t0: Instant, st: &State) {
        report.elapsed_ms = t0.elapsed().as_secs_f64() * 1e3;
        report.rss_bytes = bf_core::sys::own_rss();
        report.peak_rss_bytes = peak_rss();
        report.model_bytes = st.logistic.as_ref().map(|(_, m)| m.bytes()).unwrap_or(0) + st.net.as_ref().map(|(_, n)| n.bytes()).unwrap_or(0);
        report.backend = self.backend.name();
        let mem = |b: Option<u64>| b.map(|b| format!("{:.0} MB", b as f64 / 1_048_576.0)).unwrap_or_else(|| "?".into());
        report.log.push(format!(
            "step took {:.0} ms (train {:.0}, eval {:.0}) on {}; memory {} (peak {}), models {:.1} MB",
            report.elapsed_ms,
            report.train_ms,
            report.eval_ms,
            report.backend,
            mem(report.rss_bytes),
            mem(report.peak_rss_bytes),
            report.model_bytes as f64 / 1_048_576.0
        ));
        report.log.extend(self.backend.take_events());
        for l in &report.log {
            self.store.log(l);
        }
        self.publish(st);
    }

    fn next_version(st: &mut State) -> String {
        let v = format!("v{:06}", st.manifest.next);
        st.manifest.next += 1;
        v
    }

    fn commit_logistic(&self, st: &mut State, m: &LogisticModel, metrics: &Metrics, records: u64) -> Result<String> {
        let v = Self::next_version(st);
        let (file, bytes) = self.store.write_model(&v, LOGISTIC, &checkpoint::encode_logistic(m))?;
        st.manifest.checkpoints.push(Entry {
            version: v.clone(),
            kind: LOGISTIC.into(),
            created_ms: bf_core::util::now_ms(),
            metrics: metrics.clone(),
            records_seen: st.manifest.records_seen + records,
            file,
            bytes,
            parent: None,
        });
        st.manifest.active_logistic = Some(v.clone());
        Ok(v)
    }

    fn commit_nn(&self, st: &mut State, n: &SharedNet, metrics: &Metrics, parent: &str, records: u64) -> Result<String> {
        let v = Self::next_version(st);
        let (file, bytes) = self.store.write_model(&v, SHARED_NN, &checkpoint::encode_nn(n))?;
        st.manifest.checkpoints.push(Entry {
            version: v.clone(),
            kind: SHARED_NN.into(),
            created_ms: bf_core::util::now_ms(),
            metrics: metrics.clone(),
            records_seen: st.manifest.records_seen + records,
            file,
            bytes,
            parent: Some(parent.to_string()),
        });
        st.manifest.active_nn = Some(v.clone());
        Ok(v)
    }

    /// Reservoir sampling into the replay buffer, and the newest mid-game situations per champion-role.
    fn remember(&self, st: &mut State, fresh: &[Sample], rng: &mut Rng) {
        let cap = self.config.replay;
        for s in fresh {
            st.replay_seen += 1;
            if st.replay.len() < cap {
                st.replay.push(s.clone());
            } else if cap > 0 {
                let j = (rng.next_u64() % st.replay_seen) as usize;
                if j < cap {
                    st.replay[j] = s.clone();
                }
            }
        }
        let mut ctx = (*st.contexts).clone();
        for s in fresh {
            let x = &s.situation;
            // Mid-game examples (some items, the lane settled) or draft-only ones; not the very early ones.
            let m = x.state.minute;
            if !(m == 0.0 || (14.0..=32.0).contains(&m)) {
                continue;
            }
            let Some(role) = x.role else { continue };
            let v = ctx.entry(context_key(x.champion, role)).or_default();
            // Without the combat vector: a context stands for its champion-role in champ select, where the build is
            // what's being chosen (and the vector would weigh hundreds of bytes per context on disk).
            v.push(Situation { combat: vec![], ..x.clone() });
            if v.len() > self.config.contexts_per_pair {
                let extra = v.len() - self.config.contexts_per_pair;
                v.drain(..extra);
            }
        }
        st.contexts = Arc::new(ctx);
    }

    /// Bytes of every checkpoint file (the dashboard's Storage → checkpoints).
    pub fn disk_bytes(&self) -> u64 {
        bf_core::util::measure(&self.store.dir).0
    }

    /// The validation examples of a source (for measurements).
    pub fn samples(&self, records: &[bf_core::build::MatchRecord]) -> Vec<Sample> {
        records.iter().flat_map(|r| samples_of(r, &self.data, &self.classes)).collect()
    }
}

/// One record in this many is held out of training to fit the calibration.
const CALIB_HOLDOUT: u64 = 7;

/// FNV-1a of a match id: a stable split, whatever the platform.
fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}
