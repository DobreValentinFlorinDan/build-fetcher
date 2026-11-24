//! Build Fetcher's server: REST + `/ws` on 127.0.0.1 for the dashboard and the overlay; the League client (LCU) for
//! the activity, champ select and the import; the Live Client for in-game advice. Recommendations come from a
//! `bf_core::build::BuildSource`; the store's and the learner's status through the traits in `handles.rs`, wired in
//! `wiring.rs`.

pub mod activity;
pub mod app;
pub mod fakes;
pub mod game_mode;
pub mod handles;
pub mod lcu;
pub mod logging;
pub mod routes;
pub mod serve;
pub mod static_data;
pub mod wiring;

#[cfg(test)]
mod api_tests;
