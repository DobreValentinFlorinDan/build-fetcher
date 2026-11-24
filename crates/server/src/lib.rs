//! Irelia Companion 1.0 server: the same REST + /ws API as 0.22.0's Express server (server/index.ts).

pub mod app;
pub mod audit;
pub mod extra;
pub mod game_mode;
pub mod jobs;
pub mod lcu;
pub mod matchup;
pub mod model_worker;
pub mod routes;
pub mod scraper;
pub mod serve;
pub mod system;
