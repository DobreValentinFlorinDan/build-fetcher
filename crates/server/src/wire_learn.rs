//! The real learner (`LearnerHandle` and `BuildSource` over `bf_learn`), and the training cycle.
//!
//! The learner reads the store (`DataSource`), so it starts once the store is open. Until then builds answer "not
//! ready" and the status is empty. The training cycle runs on a thread of its own:
//!   wait until the user is idle and learning is on → one `train_step` (bounded in records and time, stopped between
//!   mini-batches as soon as the user leaves Idle) → after an update that saw new records, the store's clean-up to the
//!   learning window and the disk budget, with the bytes freed in the log.
//!
//! The store's baseline for tier B counts stays the champion-role win rate on purpose: the model's prediction already
//! includes the build options, so using it as the expected win would cancel the very effect the counts measure.

use crate::handles::LearnerHandle;
use crate::wire_store::RealStore;
use bf_core::build::{Activity, BuildRecommendation, BuildSource, DataSource, DraftContext, LiveAdvice, LiveContext, ModelStatus};
use bf_learn::{Budget, LearnConfig, Learner, Outcome, RecommendConfig, Recommender};
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
/// Later updates wait for this many, so each one has enough evidence to beat the model in force...
const MIN_RECORDS: usize = 500;
/// ...or go ahead with fewer after this long (slow fetching: a shared key, few regions).
const MAX_WAIT: Duration = Duration::from_secs(6 * 3600);
/// Held-out matches the guardrail needs before it can judge an update (the store keeps 1 timeline game in 10).
const MIN_VALIDATION: usize = 100;
/// One step may take this long before it stops between mini-batches (it is resumed at the next idle moment).
const STEP_DEADLINE: Duration = Duration::from_secs(120);
/// How often the cycle looks again when there is nothing to do or the user is busy.
const IDLE_POLL: Duration = Duration::from_secs(30);

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
            crate::bf_log!("[learn] ready ({})", learner.status().backend);
            let cycle = me.clone();
            std::thread::Builder::new().name("learn".into()).spawn(move || cycle.run(learner, db, st)).ok();
        });
    }

    /// The training cycle (its own thread: training is CPU-bound and must never hold up the server's tasks).
    fn run(self: Arc<Self>, learner: Arc<Learner>, db: Arc<bf_store::Store>, store: Arc<RealStore>) {
        loop {
            let settings = bf_store::settings::load(&bf_store::settings::settings_file()).0;
            learner.set_learning(settings.learning);
            if !settings.learning || activity_of(self.activity.load(Ordering::SeqCst)) != Activity::Idle {
                std::thread::sleep(IDLE_POLL);
                continue;
            }
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
            let report = match learner.train_step(&*db, &budget) {
                Ok(r) => r,
                Err(e) => {
                    crate::bf_log!("[learn] update failed: {}", crate::routes::scrub(&e.to_string()));
                    std::thread::sleep(IDLE_POLL);
                    continue;
                }
            };
            // Nothing to learn yet (no or too few new records, learning off, not idle): wait without a log line every poll.
            // The dashboard's model status says what the learner waits for.
            if matches!(report.outcome, Outcome::NoData | Outcome::Waiting | Outcome::NotIdle | Outcome::Disabled) {
                std::thread::sleep(IDLE_POLL);
                continue;
            }
            let mb = |b: Option<u64>| b.map(|b| format!("{:.0} MB", b as f64 / 1e6)).unwrap_or_else(|| "?".into());
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
            match report.outcome {
                Outcome::Accepted | Outcome::Rejected => {
                    // A training cycle ended: keep what the learner still needs, delete the rest, compact.
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
                Outcome::NoData | Outcome::Waiting | Outcome::NotIdle | Outcome::Disabled => std::thread::sleep(IDLE_POLL),
                Outcome::Interrupted => std::thread::sleep(Duration::from_secs(5)),
            }
        }
    }
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
        if activity != Activity::Idle {
            // Champ select or a game: stop at the next mini-batch, whatever the step's own check sees.
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
