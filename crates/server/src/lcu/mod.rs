//! The League client (LCU) and the in-game Live Client API: loopback only (127.0.0.1), nothing else. Vanguard-safe:
//! no game memory is read and nothing is injected.

pub mod connector;
pub mod https;
pub mod importer;
pub mod live_client;
pub mod simulate_live;
