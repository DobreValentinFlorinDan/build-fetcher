//! The app on a temporary data folder, for tests that go through its HTTP API as the dashboard does. The settings and
//! data folders are process-wide (the config is read once), so a test binary starts one app.
#![allow(dead_code)]

use irelia::db::Db;
use irelia::testkit::load_test_static_data;
use irelia_server::app::App;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Running {
    pub app: Arc<App>,
    pub base: String,
    pub client: reqwest::Client,
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
    _tmp: tempfile::TempDir,
}

/// Starts the app after `setup` filled its data folder (the database open, before the app opens it).
pub async fn start(setup: impl FnOnce(&Db, &Path)) -> Running {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::env::set_var("IRELIA_DATA_DIR", &data_dir);
    std::env::set_var("IRELIA_CONFIG_DIR", &config_dir);
    {
        let db = Db::open_default(&data_dir.join("irelia.db")).unwrap();
        setup(&db, &data_dir);
    }
    let db = Db::open_default(&data_dir.join("irelia.db")).unwrap();
    let app = App::start(db, (*load_test_static_data()).clone()).await;
    irelia_server::extra::attach(&app).await;
    let router = irelia_server::routes::router(app.clone());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Running { app, base: format!("http://127.0.0.1:{port}"), client: reqwest::Client::new(), data_dir, config_dir, _tmp: tmp }
}

impl Running {
    pub async fn get(&self, path: &str) -> Value {
        self.client.get(format!("{}{path}", self.base)).send().await.unwrap().json().await.unwrap()
    }
    pub async fn send(&self, method: reqwest::Method, path: &str, body: Value) -> (u16, Value) {
        let r = self.client.request(method, format!("{}{path}", self.base)).json(&body).send().await.unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }
    pub async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, body).await
    }
    pub async fn put(&self, path: &str, body: Value) -> (u16, Value) {
        self.send(reqwest::Method::PUT, path, body).await
    }
    /// Polls `path` until `done` says so (at most `secs`).
    pub async fn until(&self, path: &str, secs: u64, done: impl Fn(&Value) -> bool) -> Value {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            let v = self.get(path).await;
            if done(&v) {
                return v;
            }
            assert!(std::time::Instant::now() < end, "timed out waiting on {path}: {v}");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
}
