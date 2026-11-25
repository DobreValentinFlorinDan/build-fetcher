//! The League client (LCU) and the in-game Live Client API: loopback only (127.0.0.1), nothing else. Vanguard-safe:
//! no game memory is read and nothing is injected; both are the local HTTP APIs Riot documents for apps.

pub mod champ_select;
pub mod connector;
pub mod https;
pub mod importer;
pub mod live_client;
pub mod live_context;
pub mod simulate_live;
