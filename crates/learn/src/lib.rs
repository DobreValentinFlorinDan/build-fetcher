//! Build Fetcher's learning (docs/DESIGN.md, Agent B): win models trained continuously from the store's match
//! records, with guardrails and versioned checkpoints, and the recommendations the server shows.
//!
//! What the server calls:
//!
//! ```ignore
//! let learner = Arc::new(Learner::open(data_dir.join("checkpoints"), static_data.clone(), LearnConfig::default())?);
//! let builds: Arc<dyn BuildSource> = Arc::new(Recommender::new(store.clone(), Some(learner.clone()), static_data, RecommendConfig::default()));
//! // in the background, while Activity::Idle:
//! let report = learner.train_step(store.as_ref(), &Budget { max_records: 2000, deadline: Some(Duration::from_secs(60)), activity })?;
//! learner.status();            // ModelStatus for the dashboard
//! learner.rollback("v000012")?; // Settings → Roll back model
//! learner.interrupt();          // champ select started: stop between mini-batches
//! ```
//!
//! - `features`: situations (one player at one minute) from `MatchRecord`s, sample weights, deterministic hashing/RNG.
//! - `logistic`: model step 1, per champion-role with shrinkage toward a global model.
//! - `nn`: model step 2, a small shared network on top of step 1.
//! - `metrics`: log loss, Brier, calibration per 10-minute window; the guardrail.
//! - `checkpoint`: versioned files, manifest, learning log.
//! - `backend`: the processor and a checked accelerator (GPU) with fallback.
//! - `learner`: `Learner::train_step`, `status`, `rollback`.
//! - `recommend`: `Recommender`, the `BuildSource`.
//! - `synthetic`: matches with planted effects and an in-memory `DataSource` (tests, measurements).

pub mod backend;
pub mod checkpoint;
pub mod features;
pub mod learner;
pub mod logistic;
pub mod metrics;
pub mod nn;
pub mod recommend;
pub mod synthetic;

pub use learner::{AbsorbReport, Budget, CheckPolicy, ContinuousOptions, LearnConfig, Learner, Outcome, Served, StepReport, UpdateReport};
pub use recommend::{RecommendConfig, Recommender};
