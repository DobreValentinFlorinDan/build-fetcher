//! Build Fetcher's data (docs/DESIGN.md, "Agent A: store"): settings and the Irelia key import, the on-disk format,
//! ingestion with its data-quality filters, the fetch scheduler under the shared-key budget, and clean-up.
//!
//! Wiring it up (the server):
//! 1. `settings::load(&settings::settings_file())` for `Settings`; `settings::api_key(..)` for the key (never shown;
//!    `settings::key_status` is what the dashboard gets). Key routes: `irelia_key_available()`,
//!    `import_irelia_key()`, `set_key(pasted)`.
//! 2. `Arc::new(Store::open(&data_dir, static_data, StoreOptions::default())?)`; `Arc<Store>` is the learner's
//!    `bf_core::build::DataSource`. `store.set_baseline(Some(model))` hands the learner's win chances to the counts.
//! 3. `let f = Fetcher::new(Arc::new(RiotClient::new(key, limits, pacing)), store.clone(), settings, FetcherConfig::default());`
//!    `f.spawn(activity_rx)` (a `tokio::sync::watch::Receiver<Activity>`; champ select and in game pause it);
//!    `f.status()` → `FetchStatus`; `f.update_settings(..)`, `f.set_key_present(..)` after a key change
//!    (also `RiotClient::set_api_key`); `f.stop()`.
//! 4. After each training cycle: `store.cleanup(&LearnWindow { patches }, settings.disk_budget_bytes,
//!    &OtherParts::standard(&data_dir, Some(&logs_dir)))`; `store.storage_status(budget, &other)` → `StorageStatus`.
#![recursion_limit = "512"]

pub mod backup;
pub mod cleanup;
pub mod db;
pub mod key_import;
pub mod options;
pub mod parse;
pub mod record;
pub mod scheduler;
pub mod settings;
pub mod store;

pub use cleanup::{CleanupReport, LearnWindow, OtherParts};
pub use key_import::{import_irelia_key, irelia_key_available, set_key, KeyOffer};
pub use parse::Drop;

pub use settings::Settings;
pub use store::{Baseline, Ingest, Outcome, Store, StoreOptions};
pub use scheduler::{Fetcher, FetcherConfig, Work};
