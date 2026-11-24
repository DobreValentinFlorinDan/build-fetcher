//! Build Fetcher's data (docs/DESIGN.md, "Agent A: store"): settings and the Irelia key import, the on-disk format,
//! ingestion with its data-quality filters, the fetch scheduler under the shared-key budget, and clean-up.
//!
//! Wiring it up (the server):
//! 1. `settings::load(&settings::settings_file())` and `settings::api_key(..)`;
//! 2. `Store::open(&data_dir, static_data, StoreOptions::default())`, kept in an `Arc`;
//! 3. `Arc<Store>` is the learner's `bf_core::build::DataSource`;
//! 4. `Fetcher::new(api, store, settings).spawn(activity_rx)` starts fetching; `Fetcher::status()` feeds the
//!    dashboard; the activity watch pauses it in champ select and in game;
//! 5. after each training cycle, `store.cleanup(&window, budget, &checkpoints_dir)`; `store.storage_status(..)`.
#![recursion_limit = "512"]

pub mod cleanup;
pub mod db;
pub mod key_import;
pub mod options;
pub mod parse;
pub mod record;
pub mod scheduler;
pub mod settings;
pub mod store;

pub use cleanup::{CleanupReport, LearnWindow};
pub use key_import::{import_irelia_key, irelia_key_available, set_key, KeyOffer};
pub use parse::Drop;

pub use settings::Settings;
pub use store::{Baseline, Ingest, Outcome, Store, StoreOptions};
pub use scheduler::{Fetcher, FetcherConfig, Work};
