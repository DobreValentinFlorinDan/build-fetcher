//! Where the real store and learner are plugged in. Until integration this returns the in-memory fakes and the stub
//! `BuildSource` (`fakes.rs`); the lead replaces the body of `default_handles` with `bf_store` / `bf_learn`
//! implementations of the traits in `handles.rs` (and `bf_learn`'s `BuildSource`). Both the headless `bf-server` and
//! the desktop app start through here, so this is the only place to change.

use crate::handles::Handles;
use crate::static_data::StaticSlot;

pub struct WiringContext<'a> {
    /// Data Dragon, filled in the background once loaded (None until then).
    pub static_data: StaticSlot,
    /// Progress text for the splash ("Opening the database…").
    pub starting: &'a (dyn Fn(&str) + Send + Sync),
}

pub async fn default_handles(ctx: &WiringContext<'_>) -> anyhow::Result<Handles> {
    (ctx.starting)("Starting…");
    Ok(crate::fakes::handles(ctx.static_data.clone()))
}
