//! The real learner (`LearnerHandle` and `BuildSource` over `bf_learn`), and the training cycle.
//!
//! The learner reads the store (`DataSource`), so it starts once the store is open. Until then builds answer "not
//! ready" and the status is empty. The training cycle runs on a thread of its own:
//!   - **The first model:** while the user is idle and learning is on, one `train_step` on a big batch (bounded in
//!     records and time, stopped between mini-batches as soon as the user leaves Idle).
//!   - **Then, continuously (1.2.0):** games are learnt as they arrive (`absorb`, a few milliseconds each), whenever
//!     the user is not in champ select or a game, idle or not. Every 150 games the draft is checked against the model
//!     in force (`check`) and served only if it is no worse.
//!   - **Idle only:** re-scoring the model in force (at most hourly), and the store's clean-up to the learning window
//!     and the disk budget after checks (at most hourly), with the bytes freed in the log.
//!
//! The store's baseline for tier B counts stays the champion-role win rate on purpose: the model's prediction already
//! includes the build options, so using it as the expected win would cancel the very effect the counts measure.

use crate::handles::LearnerHandle;
use crate::wire_store::RealStore;
use bf_core::build::{Activity, BuildRecommendation, BuildSource, DataSource, DraftContext, LiveAdvice, LiveContext, ModelStatus};
use bf_learn::{Budget, CheckPolicy, LearnConfig, Learner, Outcome, RecommendConfig, Recommender};
use bf_store::LearnWindow;
use parking_lot::{Mutex, RwLock};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Records per update: the learner's own advice is about 2,000 for a first update to beat the baseline.
const RECORDS_PER_STEP: usize = 2_000;
/// The first update waits for this many new games (on synthetic data, 500 were rejected several times).
const FIRST_MIN_RECORDS: usize = 2_000;
/// Batch updates after the first (no longer used by the cycle since 1.2.0, which learns continuously; kept for a
/// first update that gets this far without a model).
const MIN_RECORDS: usize = 500;
const MAX_WAIT: Duration = Duration::from_secs(6 * 3600);
/// Held-out matches the guardrail needs before it can judge an update (the store keeps 1 timeline game in 10).
const MIN_VALIDATION: usize = 100;
/// While waiting for data, the models in force are scored again on the grown held-out sample at most this often.
const RESCORE_EVERY: Duration = Duration::from_secs(3600);
/// One step may take this long before it stops between mini-batches (it is resumed at the next idle moment).
const STEP_DEADLINE: Duration = Duration::from_secs(120);
/// How often the cycle looks again when there is nothing to do or the user is busy.
const IDLE_POLL: Duration = Duration::from_secs(30);
/// Continuous learning: games learnt per call at most (a backlog, e.g. after a restart, goes in pieces), the time one
/// call may take, and how soon the cycle looks for new games again (the fetcher stores about one every few seconds).
const ABSORB_MAX: usize = 200;
const ABSORB_DEADLINE: Duration = Duration::from_secs(20);
const ABSORB_POLL: Duration = Duration::from_secs(5);
/// Games learnt between checks, and a check with fewer after this long.
const CHECK_EVERY: usize = 150;
const CHECK_MAX_WAIT: Duration = Duration::from_secs(3600);
/// Clean-up after checks runs at most this often.
const CLEANUP_EVERY: Duration = Duration::from_secs(3600);

fn activity_code(a: Activity) -> u8 {
    match a {
        Activity::Idle => 0,
        Activity::Busy => 1,
        Activity::ChampSelect => 2,
        Activity::InGame => 3,
    }
}

fn activity_of(c: u8) -> Activity {
    match c {
        0 => Activity::Idle,
        1 => Activity::Busy,
        2 => Activity::ChampSelect,
        _ => Activity::InGame,
    }
}

pub struct RealLearner {
    checkpoints: PathBuf,
    learner: OnceLock<Arc<Learner>>,
    builds: RwLock<Option<Arc<Recommender>>>,
    activity: Arc<AtomicU8>,
    last_cycle: Mutex<Vec<String>>,
}

impl RealLearner {
    pub fn new(checkpoints: PathBuf) -> Arc<RealLearner> {
        Arc::new(RealLearner { checkpoints, learner: OnceLock::new(), builds: RwLock::new(None), activity: Arc::new(AtomicU8::new(0)), last_cycle: Mutex::new(vec![]) })
    }

    /// Opens the learner over the store once the store is open, and starts the training cycle.
    pub fn attach(self: &Arc<Self>, store: Arc<RealStore>) {
        let me = self.clone();
        let st = store.clone();
        store.with_store(move |db| {
            let data = db.static_data();
            let settings = bf_store::settings::load(&bf_store::settings::settings_file()).0;
            let config = LearnConfig { use_gpu: settings.gpu, ..LearnConfig::default() };
            let learner = match Learner::open(&me.checkpoints, data.clone(), config) {
                Ok(l) => Arc::new(l),
                Err(e) => {
                    crate::bf_log!("[learn] could not open the checkpoints: {}", crate::routes::scrub(&e.to_string()));
                    return;
                }
            };
            learner.set_learning(settings.learning);
            let source: Arc<dyn DataSource> = db.clone();
            *me.builds.write() = Some(Arc::new(Recommender::new(source, Some(learner.clone()), data, RecommendConfig::default())));
            let _ = me.learner.set(learner.clone());
            if let Err(e) = learner.refresh_team(&*db) {
                crate::bf_log!("[learn] counting games for the team network failed: {}", crate::routes::scrub(&e.to_string()));
            }
            crate::bf_log!("[learn] ready ({})", learner.status().backend);
            let cycle = me.clone();
            std::thread::Builder::new().name("learn".into()).spawn(move || cycle.run(learner, db, st)).ok();
        });
    }

    /// The training cycle (its own thread: training is CPU-bound and must never hold up the server's tasks).
    fn run(self: Arc<Self>, learner: Arc<Learner>, db: Arc<bf_store::Store>, store: Arc<RealStore>) {
        let mut last_rescore: Option<std::time::Instant> = None;
        let mut last_cleanup: Option<std::time::Instant> = None;
        let mut checked_since_cleanup = false;
        let policy = CheckPolicy { every: CHECK_EVERY, max_wait: Some(CHECK_MAX_WAIT), min_validation: MIN_VALIDATION, ..CheckPolicy::default() };
        loop {
            let settings = bf_store::settings::load(&bf_store::settings::settings_file()).0;
            learner.set_learning(settings.learning);
            let activity = activity_of(self.activity.load(Ordering::SeqCst));
            if !settings.learning {
                std::thread::sleep(IDLE_POLL);
                continue;
            }
            let idle = activity == Activity::Idle;
            let mut nothing_new = false;
            if !learner.has_model() {
                // The first model, from a big batch, while idle.
                if !idle {
                    std::thread::sleep(IDLE_POLL);
                    continue;
                }
                match self.first_model(&learner, &db) {
                    Some(Outcome::Accepted | Outcome::Rejected) => checked_since_cleanup = true,
                    Some(Outcome::NoData | Outcome::Waiting) => nothing_new = true,
                    _ => {}
                }
            } else {
                if matches!(activity, Activity::ChampSelect | Activity::InGame) {
                    std::thread::sleep(IDLE_POLL);
                    continue;
                }
                let act = self.activity.clone();
                let a = match learner.absorb(&*db, ABSORB_MAX, Some(ABSORB_DEADLINE), Arc::new(move || activity_of(act.load(Ordering::SeqCst)))) {
                    Ok(a) => a,
                    Err(e) => {
                        crate::bf_log!("[learn] learning new games failed: {}", crate::routes::scrub(&e.to_string()));
                        std::thread::sleep(IDLE_POLL);
                        continue;
                    }
                };
                nothing_new = a.outcome == Outcome::NoData;
                match learner.check(&*db, &policy) {
                    Ok(Some(report)) => {
                        crate::bf_log!(
                            "[learn] check after {} games: {:?}, {:.1} s (eval {:.1} s), memory {}",
                            report.records,
                            report.outcome,
                            report.elapsed_ms / 1000.0,
                            report.eval_ms / 1000.0,
                            mb(report.rss_bytes)
                        );
                        for line in &report.log {
                            crate::bf_log!("[learn] {line}");
                        }
                        *self.last_cycle.lock() = report.log.clone();
                        checked_since_cleanup = true;
                    }
                    Ok(None) => {}
                    Err(e) => crate::bf_log!("[learn] check failed: {}", crate::routes::scrub(&e.to_string())),
                }
            }
            // Idle only: re-score while nothing new arrives (at most hourly), and clean up after checks (at most hourly).
            if idle && nothing_new && last_rescore.is_none_or(|t: std::time::Instant| t.elapsed() >= RESCORE_EVERY) {
                last_rescore = Some(std::time::Instant::now());
                match learner.rescore(&*db) {
                    Ok(Some(line)) => crate::bf_log!("[learn] {line}"),
                    Ok(None) => {}
                    Err(e) => crate::bf_log!("[learn] re-scoring failed: {}", crate::routes::scrub(&e.to_string())),
                }
            }
            if idle && checked_since_cleanup && last_cleanup.is_none_or(|t: std::time::Instant| t.elapsed() >= CLEANUP_EVERY) {
                last_cleanup = Some(std::time::Instant::now());
                checked_since_cleanup = false;
                // Keep what the learner still needs, delete the rest, compact.
                match db.cleanup(&LearnWindow::default(), settings.disk_budget_bytes, &store.other_parts()) {
                    Ok(c) => crate::bf_log!(
                        "[cleanup] freed {:.1} MB ({} records, {} count rows; trimmed patches {:?}) in {} ms; {:.1} MB used{}",
                        c.freed_bytes as f64 / 1e6,
                        c.records_deleted,
                        c.count_rows_deleted,
                        c.patches_trimmed.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
                        c.millis,
                        c.after_bytes as f64 / 1e6,
                        if c.over_budget { " (still over the budget)" } else { "" }
                    ),
                    Err(e) => crate::bf_log!("[cleanup] failed: {}", crate::routes::scrub(&e.to_string())),
                }
            }
            std::thread::sleep(if nothing_new || !learner.has_model() { IDLE_POLL } else { ABSORB_POLL });
        }
    }

    /// The first model: one guarded batch update, when enough games and held-out matches exist (the model status says
    /// what it waits for meanwhile). None when the update failed.
    fn first_model(&self, learner: &Learner, db: &bf_store::Store) -> Option<Outcome> {
        let act = self.activity.clone();
        let budget = Budget {
            max_records: RECORDS_PER_STEP,
            deadline: Some(STEP_DEADLINE),
            activity: Arc::new(move || activity_of(act.load(Ordering::SeqCst))),
            min_records: MIN_RECORDS,
            first_min_records: FIRST_MIN_RECORDS,
            max_wait: Some(MAX_WAIT),
            min_validation: MIN_VALIDATION,
        };
        let report = match learner.train_step(db, &budget) {
            Ok(r) => r,
            Err(e) => {
                crate::bf_log!("[learn] update failed: {}", crate::routes::scrub(&e.to_string()));
                return None;
            }
        };
        if !matches!(report.outcome, Outcome::NoData | Outcome::Waiting | Outcome::NotIdle | Outcome::Disabled) {
            crate::bf_log!(
                "[learn] {:?}: {} records, {} examples, {:.1} s (train {:.1} s, eval {:.1} s), memory {} (peak {}), {}",
                report.outcome,
                report.records,
                report.samples,
                report.elapsed_ms / 1000.0,
                report.train_ms / 1000.0,
                report.eval_ms / 1000.0,
                mb(report.rss_bytes),
                mb(report.peak_rss_bytes),
                report.backend
            );
            for line in &report.log {
                crate::bf_log!("[learn] {line}");
            }
            *self.last_cycle.lock() = report.log.clone();
        }
        Some(report.outcome)
    }
}

fn mb(b: Option<u64>) -> String {
    b.map(|b| format!("{:.0} MB", b as f64 / 1e6)).unwrap_or_else(|| "?".into())
}

impl LearnerHandle for RealLearner {
    fn status(&self) -> ModelStatus {
        match self.learner.get() {
            Some(l) => l.status(),
            None => ModelStatus { log: vec!["The learner starts once the database is open.".into()], ..ModelStatus::default() },
        }
    }

    fn rollback(&self, version: &str) -> anyhow::Result<ModelStatus> {
        let l = self.learner.get().ok_or_else(|| anyhow::anyhow!("The learner is not ready yet."))?;
        l.rollback(version)?;
        crate::bf_log!("[learn] rolled back to {version}");
        Ok(l.status())
    }

    fn set_learning(&self, on: bool) {
        if let Some(l) = self.learner.get() {
            l.set_learning(on);
            if !on {
                // Stop an update in progress at its next mini-batch; nothing it did so far is kept.
                l.interrupt();
            }
        }
        crate::bf_log!("[learn] learning {}", if on { "resumed" } else { "paused" });
    }

    fn set_activity(&self, activity: Activity) {
        self.activity.store(activity_code(activity), Ordering::SeqCst);
        if matches!(activity, Activity::ChampSelect | Activity::InGame) {
            // Champ select or a game: stop at the next mini-batch, whatever the step's own check sees. (A first-model
            // update also stops when the PC is merely in use: its own check asks for Idle.)
            if let Some(l) = self.learner.get() {
                l.interrupt();
            }
        }
    }
}

/// The recommender once it exists; before that, a plain "not ready" error the dashboard shows.
pub struct LateBuilds(pub Arc<RealLearner>);

impl BuildSource for LateBuilds {
    fn recommend(&self, ctx: &DraftContext) -> anyhow::Result<BuildRecommendation> {
        let r = self.0.builds.read().clone().ok_or_else(|| anyhow::anyhow!("Build Fetcher is still starting (Data Dragon and the database)."))?;
        r.recommend(ctx)
    }
    fn live(&self, ctx: &LiveContext) -> anyhow::Result<LiveAdvice> {
        let r = self.0.builds.read().clone().ok_or_else(|| anyhow::anyhow!("Build Fetcher is still starting (Data Dragon and the database)."))?;
        r.live(ctx)
    }
}
