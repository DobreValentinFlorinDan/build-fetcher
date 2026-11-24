//! Starting the server: the handles (`wiring.rs`), Data Dragon in the background, the League client, and the HTTP
//! listener on 127.0.0.1. Shared by the headless `bf-server` binary and the desktop app.

use crate::app::App;
use crate::handles::Handles;
use crate::static_data::{empty_slot, load_in_background};
use crate::wiring::{default_handles, WiringContext};
use std::path::PathBuf;
use std::sync::Arc;

pub struct ServeOptions {
    /// None: `bf_core::config`'s port (PORT, else 5184).
    pub port: Option<u16>,
    /// Take any free port when the preferred one is taken (the desktop app).
    pub any_port: bool,
    /// The built dashboard (index.html, assets); None or missing: the API only (development uses Vite).
    pub web_dir: Option<PathBuf>,
    /// What the start is doing, for the splash.
    pub starting: Box<dyn Fn(&str) + Send + Sync>,
    /// Given handles (tests, the lead's experiments); None: `wiring::default_handles`.
    pub handles: Option<Handles>,
}

pub struct Server {
    pub port: u16,
    pub app: Arc<App>,
    pub done: tokio::task::JoinHandle<std::io::Result<()>>,
}

/// ./dist next to the working directory (the desktop app sets it to its install folder), or BF_WEB_DIR.
pub fn default_web_dir() -> PathBuf {
    bf_core::util::env("BF_WEB_DIR").map(PathBuf::from).unwrap_or_else(|| bf_core::util::resolve("dist"))
}

pub async fn run(o: ServeOptions) -> anyhow::Result<Server> {
    let slot = empty_slot();
    let handles = match o.handles {
        Some(h) => h,
        None => default_handles(&WiringContext { static_data: slot.clone(), starting: &*o.starting }).await?,
    };
    let app = App::new(handles, slot.clone(), Some(crate::game_mode::GameMode::default()));
    (o.starting)("Starting the server…");
    let router = crate::routes::router(app.clone(), o.web_dir);
    let port = o.port.unwrap_or_else(|| bf_core::config::config().port);
    // Loopback only: nothing outside this PC can reach the server.
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) if o.any_port && e.kind() == std::io::ErrorKind::AddrInUse => tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?,
        Err(e) => return Err(e.into()),
    };
    let port = listener.local_addr()?.port();
    let ready = app.clone();
    load_in_background(slot, move |_| ready.static_ready());
    app.start();
    // The activity reaches the store and the learner through `App::activity_changed`; tell them where it starts.
    app.activity_changed(app.activity.current());
    let done = tokio::spawn(async move { axum::serve(listener, router).await });
    Ok(Server { port, app, done })
}
