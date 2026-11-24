#![recursion_limit = "512"]
//! Irelia Companion 1.0: the backend and engine (no UI, no Tauri). Each module ports the TS module it names.

pub mod app_config;
pub mod auto_eval;
pub mod codec;
pub mod config;
pub mod db;
pub mod db_sql;
pub mod engine;
pub mod gpu;
pub mod ddragon;
pub mod capture;
mod capture_read;
pub mod js;
pub mod jsmath;
mod jsmath_tables;
pub mod model;
pub mod patches;
pub mod progress;
pub mod riot;
pub mod scope;
pub mod scraper;
pub mod slim;
pub mod storage;
pub mod sys;
pub mod testkit;
pub mod testkit_model;
pub mod transfer;
pub mod types;
pub mod util;
pub mod zip;

/// Every program built on this crate (the app, the server, the tools) allocates with mimalloc: the model and the
/// evaluation make many small allocations, where it is faster than the system's allocator, above all on Windows.
/// It only changes where memory comes from, never a result.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
