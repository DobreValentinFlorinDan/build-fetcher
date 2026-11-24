//! Starting the server: the database (migrated), Data Dragon, the app and its HTTP listener. Shared by the
//! `irelia-server` binary and the desktop app.

use crate::app::{static_data, App};
use irelia::config::config;
use irelia::db::Db;
use std::sync::Arc;

pub struct ServeOptions {
    /// None: config's port (PORT, else 5174).
    pub port: Option<u16>,
    /// Take any free port when the preferred one is taken (the desktop app).
    pub any_port: bool,
    /// What the start is doing, for the splash.
    pub starting: Box<dyn Fn(&str) + Send + Sync>,
}

pub struct Server {
    pub port: u16,
    pub app: Arc<App>,
    pub done: tokio::task::JoinHandle<std::io::Result<()>>,
}

pub async fn run(o: ServeOptions) -> anyhow::Result<Server> {
    (o.starting)("Starting the app…");
    // Slim stored data cut short by a crash or a closed app: undone before anything opens the database.
    crate::extra::recover_slim_at_start(&o.starting).await;
    let cfg = config();
    let db = tokio::task::spawn_blocking({
        let p = cfg.db_path.clone();
        move || Db::open_default(&p)
    })
    .await??;
    eprintln!("[data] {}", cfg.data_dir.display());
    (o.starting)("Loading Data Dragon…");
    let data = static_data(None).await?;
    eprintln!("[ddragon] patch {} ({}), previous {}", data.patch, data.version, data.previous_patch);
    let app = App::start(db, data).await;
    crate::extra::attach(&app).await;
    let router = crate::routes::router(app.clone());
    let port = o.port.unwrap_or(cfg.port);
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) if o.any_port && e.kind() == std::io::ErrorKind::AddrInUse => tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?,
        Err(e) => return Err(e.into()),
    };
    let port = listener.local_addr()?.port();
    let done = tokio::spawn(async move { axum::serve(listener, router).await });
    Ok(Server { port, app, done })
}
