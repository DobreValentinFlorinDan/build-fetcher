#![recursion_limit = "512"]
//! Build Fetcher's shared foundation (no UI, no Tauri), carried over from irelia-companion 2.2.0: the zstd codec,
//! correctly rounded math (`jsmath`), JSON number formatting (`js`), paths and settings, the Riot web API client with
//! its GCRA rate limiter, Data Dragon for every champion, item knowledge, the CUDA loader, and the shared types the
//! store, the learner and the server exchange (`build`).

pub mod app_config;
pub mod build;
pub mod codec;
pub mod config;
pub mod ddragon;
pub mod engine;
pub mod gpu;
pub mod js;
pub mod jsmath;
mod jsmath_tables;
pub mod riot;
pub mod sys;
pub mod testkit;
pub mod util;

/// Every program built on this crate allocates with mimalloc (many small allocations in ingestion and training; it
/// only changes where memory comes from, never a result).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
