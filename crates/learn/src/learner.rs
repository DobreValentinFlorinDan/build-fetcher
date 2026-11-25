//! The learner: incremental, guarded updates of both model steps from the store's new records.
//!
//! The server calls `train_step` in the background while the user is idle. Each call reads the records after the
//! last one trained on, mixes them with a replay sample of earlier examples, continues training the current models
//! (warm start), evaluates the candidates on the store's held-out validation records, and keeps a candidate only if
//! no metric got worse (`Guardrail`); otherwise the previous checkpoint stays in force. Every call writes one line
//! per model to the learning log.

use crate::backend::{select_backend, Backend};
use crate::checkpoint::{self, Entry, Manifest, Store, LOGISTIC, SHARED_NN};
use crate::features::{sample_weight, samples_of, sigmoid, ItemClasses, Rng, Sample, Situation, WeightOptions};
use crate::logistic::{LogisticModel, TrainOptions};
use crate::metrics::{evaluation, fmt_metrics, Evaluation, Guardrail};
use crate::nn::{NetOptions, SharedNet};
use anyhow::{anyhow, Result};
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
    /// Not started: the user isn't idle.
    NotIdle,
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
}

impl Served {
    pub fn logit(&self, s: &Situation) -> f64 {
        self.logistic.logit(s) + self.net.as_ref().map(|n| n.residual(s)).unwrap_or(0.0)
    }
    pub fn predict(&self, s: &Situation) -> f64 {
        sigmoid(self.logit(s))
    }
    /// "v12" (step 1) or "v12+v13" (step 2 on top of step 1).
    pub fn version(&self) -> String {
        match &self.net_version {
            Some(n) => format!("{}+{}", self.logistic_version, n),
            None => self.logistic_version.clone(),
        }
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
        let contexts = Arc::new(store.load_contexts());
        let classes = ItemClasses::new(&data);
        let l = Learner {
            store,
            data,
            classes,
            config,
            backend,
            state: Mutex::new(State { manifest: manifest.clone(), logistic, net, replay: vec![], replay_seen: 0, contexts }),
            manifest: RwLock::new(manifest),
            served: RwLock::new(None),
            stop: AtomicBool::new(false),
            learning: AtomicBool::new(true),
            last_update: Mutex::new(Instant::now()),
            waiting: Mutex::new(None),
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
            log: self.waiting.lock().iter().cloned().chain(self.store.log_tail(50)).collect(),
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
        if e.kind == LOGISTIC {
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
        let te = Instant::now();
        let before = match &st.logistic {
            Some((_, m)) => evaluate(&self.logits_of(m, &val).iter().map(|z| sigmoid(*z)).collect::<Vec<_>>(), &val),
            None => evaluate(&baseline_predictions(src, &val)?, &val),
        };
        let z_cand = self.logits_of(&cand, &val);
        let after_full = evaluate_full(&z_cand.iter().map(|z| sigmoid(*z)).collect::<Vec<_>>(), &val);
        let after = after_full.metrics.clone();
        report.eval_ms += te.elapsed().as_secs_f64() * 1e3;
        let reason = self.config.guardrail.worse(&before, &after_full);
        let what_before = st.logistic.as_ref().map(|(v, _)| v.clone()).unwrap_or_else(|| "the champion-role win rate".into());
        let mut accepted_any = false;
        let mut step = StepReport { kind: LOGISTIC.into(), accepted: reason.is_none(), version: None, before: before.clone(), after: after.clone(), reason: reason.clone() };
        match &reason {
            None => {
                let v = self.commit_logistic(&mut st, &cand, &after, n_records as u64)?;
                report.log.push(format!(
                    "{v} logistic accepted over {what_before}: {} -> {}; {} records, {} examples (+{} replayed), {} mini-batches; calibration temperature {:.3} + {:.3} per 10 min",
                    fmt_metrics(&before),
                    fmt_metrics(&after),
                    n_records,
                    fresh_n,
                    k,
                    batches,
                    cand.calib[0],
                    cand.calib[1]
                ));
                step.version = Some(v.clone());
                st.logistic = Some((v, Arc::new(cand)));
                accepted_any = true;
            }
            Some(why) => report.log.push(format!(
                "logistic update rejected, keeping {what_before}: {why} ({} -> {}); {} records",
                fmt_metrics(&before),
                fmt_metrics(&after),
                n_records
            )),
        }
        report.steps.push(step);

        // Step 2, on top of the step 1 now in force.
        let mut interrupted = false;
        if self.config.train_net {
            if let Some((lv, lm)) = st.logistic.clone() {
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
                        let te = Instant::now();
                        let z_l = self.logits_of(&lm, &val);
                        let m_l_full = evaluate_full(&z_l.iter().map(|z| sigmoid(*z)).collect::<Vec<_>>(), &val);
                        let m_l = m_l_full.metrics.clone();
                        let old = st.net.as_ref().map(|(v, n)| {
                            let r = Self::residuals_of(n, &val);
                            (v.clone(), evaluate_full(&z_l.iter().zip(&r).map(|(z, r)| sigmoid(z + r)).collect::<Vec<_>>(), &val))
                        });
                        let r_new = Self::residuals_of(&net, &val);
                        let m_new_full = evaluate_full(&z_l.iter().zip(&r_new).map(|(z, r)| sigmoid(z + r)).collect::<Vec<_>>(), &val);
                        let m_new = m_new_full.metrics.clone();
                        report.eval_ms += te.elapsed().as_secs_f64() * 1e3;
                        // It must beat step 1 alone, and the network in force if there is one.
                        let mut why = self.config.guardrail.worse(&m_l, &m_new_full).map(|w| format!("vs step 1 {lv}: {w}"));
                        if why.is_none() {
                            if let Some((ov, om)) = &old {
                                why = self.config.guardrail.worse(&om.metrics, &m_new_full).map(|w| format!("vs {ov}: {w}"));
                            }
                        }
                        let reference = old.as_ref().map(|o| o.1.metrics.clone()).unwrap_or(m_l.clone());
                        let mut step = StepReport { kind: SHARED_NN.into(), accepted: why.is_none(), version: None, before: reference.clone(), after: m_new.clone(), reason: why.clone() };
                        match why {
                            None => {
                                let v = self.commit_nn(&mut st, &net, &m_new, &lv, n_records as u64)?;
                                report.log.push(format!("{v} shared-nn accepted on top of {lv}: {} -> {}", fmt_metrics(&reference), fmt_metrics(&m_new)));
                                step.version = Some(v.clone());
                                st.net = Some((v, Arc::new(net)));
                                accepted_any = true;
                            }
                            Some(w) => {
                                report.log.push(format!("shared-nn update rejected: {w}"));
                                // A network in force that no longer beats the new step 1 stops being served.
                                if let Some((ov, om)) = &old {
                                    if let Some(w2) = self.config.guardrail.worse(&m_l, om) {
                                        report.log.push(format!("{ov} no longer beats {lv} ({w2}); serving step 1 alone"));
                                        st.net = None;
                                        st.manifest.active_nn = None;
                                    }
                                }
                            }
                        }
                        report.steps.push(step);
                    }
                }
            }
        }

        // The records count as read whatever the guardrails decided: they go into the replay sample and the contexts.
        st.manifest.cursor = cursor;
        st.manifest.records_seen += n_records as u64;
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
            v.push(x.clone());
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
