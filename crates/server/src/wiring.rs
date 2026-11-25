//! Where the real store, key and learner are plugged in (`wire_store.rs`, `wire_learn.rs`). Both the headless
//! `bf-server` and the desktop app start through here. `BF_FAKES=1` starts the in-memory fakes instead (`fakes.rs`),
//! for working on the dashboard without a database or a key.

use crate::handles::Handles;
use crate::static_data::StaticSlot;
use crate::wire_learn::{LateBuilds, RealLearner};
use crate::wire_store::{logs_dir, RealKeys, RealStore};
use std::sync::Arc;

pub struct WiringContext<'a> {
    /// Data Dragon, filled in the background once loaded (None until then).
    pub static_data: StaticSlot,
    /// Progress text for the splash ("Opening the database…").
    pub starting: &'a (dyn Fn(&str) + Send + Sync),
}

pub async fn default_handles(ctx: &WiringContext<'_>) -> anyhow::Result<Handles> {
    if bf_core::util::env("BF_FAKES").as_deref() == Some("1") {
        (ctx.starting)("Starting with sample data…");
        return Ok(crate::fakes::handles(ctx.static_data.clone()));
    }
    (ctx.starting)("Opening the database…");
    let settings_file = bf_store::settings::settings_file();
    let data_dir = bf_core::config::config().data_dir;
    let store = RealStore::new(settings_file.clone(), data_dir.clone(), logs_dir(&settings_file));
    store.open_when_ready(ctx.static_data.clone());
    let learner = RealLearner::new(data_dir.join("checkpoints"));
    learner.attach(store.clone());
    let keys = RealKeys::new(settings_file, bf_store::key_import::irelia_config_dir(), store.clone());
    Ok(Handles { builds: Arc::new(LateBuilds(learner.clone())), store, learner, keys: Arc::new(keys) })
}
