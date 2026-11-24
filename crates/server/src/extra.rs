//! The storage, backup, Slim stored data, old patches and conversion parts (routes/storage.ts, routes/backup.ts,
//! transfers.ts, autoExport.ts, slimControl.ts, oldPatches.ts, convert.ts and convertWorker.ts), attached to the app.

use crate::app::App;
use crate::routes::{js_json, Extra, R};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use irelia::app_config::{app_config_file, read_app_config, update_app_config};
use irelia::config::{config, data_dir_locked_by, data_paths, default_data_dir, set_data_dir};
use irelia::db::{Db, OpenOptions};
use irelia::progress::{expected_ms, record_timing, Progress, Timing};
use irelia::riot::disk_cache::{close_cache_stores, CacheStore, DiskCache};
use irelia::slim;
use irelia::storage::{copy_data_dir, fmt_bytes, inspect_target, prune_ddragon, remove_owned_data, same_path, storage_stats, verify_copy, StatsOptions};
use irelia::util::now_ms;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, LazyLock, Weak};
use std::time::Duration;

fn ok(v: Value) -> R {
    js_json(StatusCode::OK, &v)
}
fn err(status: StatusCode, m: impl Into<String>) -> R {
    js_json(status, &json!({ "error": m.into() }))
}
fn s(v: &Value) -> String {
    match v {
        Value::String(x) => x.clone(),
        Value::Null => "undefined".into(),
        o => irelia::js::stringify(o),
    }
}

/// What a Slim run cut short left, rolled back at the start (shown on the Storage card).
static RECOVERED: LazyLock<Mutex<Option<Value>>> = LazyLock::new(|| Mutex::new(None));

/// Undoes a Slim stored data run cut short by a crash or a closed app, before anything opens the database.
pub async fn recover_slim_at_start(starting: &(dyn Fn(&str) + Send + Sync)) {
    let cfg = config();
    let (dir, cache) = (cfg.data_dir.clone(), cfg.riot_cache_dir.clone());
    let last = Arc::new(Mutex::new(String::new()));
    let r = {
        let last = last.clone();
        let tick = move |text: &str| {
            let step = text.split([':', '(']).next().unwrap_or("").trim().to_string();
            let mut l = last.lock();
            if *l != step {
                eprintln!("[slim] rolling back the run cut short: {step}");
                *l = step;
            }
        };
        // The splash hears the rollback's progress through the log lines; the start waits for it.
        let _ = starting;
        tokio::task::spawn_blocking(move || slim::recover_slim(&dir, &cache, &tick)).await
    };
    match r {
        Ok(Ok(Some(m))) => {
            let step = m["step"].as_str().unwrap_or("");
            if (step == "rolled-back" || step == "cancelled") && m.get("note").is_some() {
                eprintln!("[slim] {}", s(&m["note"]));
                *RECOVERED.lock() = Some(m);
            }
        }
        Ok(Err(e)) => eprintln!("[slim] could not recover the last run: {e}"),
        _ => {}
    }
}

pub async fn attach(app: &Arc<App>) {
    if let Some(m) = RECOVERED.lock().take() {
        let _ = app.db.lock().set_meta(slim::SLIM_META, &irelia::js::stringify(&m));
    }
    let parts = Parts::new(app);
    *PARTS.lock() = Some(parts.clone());
    *app.extra.lock() = Some(parts.clone());
    parts.start();
}

// ================================================================== the parts

pub struct Parts {
    app: Weak<App>,
    storage_busy: AtomicBool,
    cache_files: AtomicI64,
    moving: Mutex<Option<Progress>>,
    pub old: OldPatches,
    pub conversion: Conversion,
    pub slim: SlimControl,
    pub backup: Backup,
}

impl Parts {
    fn new(app: &Arc<App>) -> Arc<Parts> {
        let w = Arc::downgrade(app);
        let slim_status = SlimControl::initial(app);
        Arc::new(Parts {
            app: w.clone(),
            storage_busy: AtomicBool::new(false),
            cache_files: AtomicI64::new(0),
            moving: Mutex::new(None),
            old: OldPatches::new(w.clone()),
            conversion: Conversion::new(w.clone()),
            slim: SlimControl { app: w.clone(), status: Mutex::new(slim_status), stop: Arc::new(AtomicBool::new(false)), running: AtomicBool::new(false) },
            backup: Backup::new(w),
        })
    }

    fn app(&self) -> Option<Arc<App>> {
        self.app.upgrade()
    }

    fn start(self: &Arc<Self>) {
        let Some(app) = self.app() else { return };
        // The conversion after 30 s; old patches after a minute, then hourly; auto backup after 30 s, then hourly.
        let me = self.clone();
        app.rt.spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            me.conversion.check();
            me.backup.check().await;
        });
        let me = self.clone();
        app.rt.spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            loop {
                me.old.maintain(&me).await;
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        });
        let me = self.clone();
        app.rt.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                me.backup.check().await;
            }
        });
    }

    /// Every thread with the database or the Riot cache open stops (`what` is said on the Data tab)...
    pub async fn pause(&self, what: &str) {
        let Some(app) = self.app() else { return };
        app.scraper.terminate(what).await;
        let m = app.models.clone();
        let _ = tokio::task::spawn_blocking(move || m.terminate()).await;
        app.jobs.cancel_all(what).await;
        self.conversion.pause().await;
    }

    /// ...and starts again on the (new) data folder.
    pub fn resume(&self) {
        let Some(app) = self.app() else { return };
        app.scraper.respawn();
        app.models.respawn();
        app.request_build(60);
        self.conversion.resume();
    }

    fn blocked_other(&self) -> Option<String> {
        if self.old.busy() {
            return Some("Old patches are being archived, restored or cleared from the cache.".into());
        }
        if self.slim.running.load(Ordering::SeqCst) {
            return Some("Slim stored data is running.".into());
        }
        None
    }

    fn conflict(&self) -> Option<String> {
        if self.storage_busy.load(Ordering::SeqCst) {
            return Some("The data folder is being moved or the database compacted.".into());
        }
        self.blocked_other()
    }

    fn stats(&self) -> Value {
        let Some(app) = self.app() else { return json!({}) };
        let cfg = config();
        let busy = self.storage_busy.load(Ordering::SeqCst);
        let mut st = storage_stats(&cfg.data_dir, &StatsOptions {
            default_data_dir: default_data_dir(),
            config_file: app_config_file(),
            locked_by: data_dir_locked_by().map(String::from),
            ddragon_version: app.data().version.clone(),
            busy,
        })
        .unwrap_or_else(|e| json!({ "error": e.to_string() }));
        let cat = |k: &str| st["categories"].as_array().and_then(|a| a.iter().find(|c| c["key"] == k).cloned());
        if let Some(n) = cat("riotCache").and_then(|c| c["files"].as_i64()) {
            self.cache_files.store(n, Ordering::SeqCst);
        }
        let f = |c: Option<Value>, k: &str| c.and_then(|c| c[k].as_f64()).unwrap_or(0.0);
        st["estimates"] = json!({
            "compact": expected_ms("compact", f(cat("database"), "bytes"), None),
            "clear-cache": expected_ms("clear-cache", f(cat("riotCache"), "files"), None),
            "clear-expired": expected_ms("clear-expired", 1.0, None),
            "ddragon-old": expected_ms("ddragon-old", 1.0, None),
            "move": expected_ms("move", st["totalBytes"].as_f64().unwrap_or(0.0), None),
        });
        let conversion = self.conversion.status.lock().clone();
        let db = app.db.lock();
        let suggest = !busy && !db.auto_vacuum() && conversion.as_ref().map(|c| c["done"] == true).unwrap_or(true);
        if !busy {
            st["tables"] = json!(db.table_stats().unwrap_or_default().iter().map(|t| json!({ "name": t.name, "rows": t.rows, "bytes": t.bytes })).collect::<Vec<_>>());
        }
        st["conversion"] = conversion.unwrap_or(Value::Null);
        st["suggestCompact"] = if suggest { json!({ "freeBytes": db.free_bytes() }) } else { Value::Null };
        st
    }

    /// A Storage action, shown with its time left, its time kept for the next estimate once done.
    async fn as_action<T: Send + 'static>(self: &Arc<Self>, action: &'static str, text: &'static str, size: f64, work: impl FnOnce(Arc<dyn Fn(f64) + Send + Sync>) -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
        let app = self.app().ok_or_else(|| anyhow::anyhow!("closing"))?;
        let tracker = Arc::new(Progress::new(expected_ms(action, size, None)));
        let fraction = Arc::new(Mutex::new(None::<f64>));
        let show = {
            let (a, t, f) = (app.clone(), tracker.clone(), fraction.clone());
            Arc::new(move || a.broadcast(json!({ "type": "storage-action", "action": { "action": action, "text": text, "progress": t.info(*f.lock()) } })))
        };
        show();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let tick: Arc<dyn Fn(f64) + Send + Sync> = {
            let (s2, f) = (show.clone(), fraction.clone());
            Arc::new(move |x: f64| {
                *f.lock() = Some(x);
                s2();
            })
        };
        let r = tokio::task::spawn_blocking(move || work(tick)).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        if r.is_ok() {
            record_timing(action, Timing { size, ms: (now_ms() - tracker.started_at) as f64, at: now_ms() as f64, ..Default::default() });
        }
        app.broadcast(json!({ "type": "storage-action", "action": null }));
        r
    }
}

impl Extra for Parts {
    fn set_busy(&self, busy: bool) {
        self.conversion.set_busy(busy);
        self.slim.set_busy(busy);
    }
    fn blocked_for_evaluation(&self) -> bool {
        self.storage_busy.load(Ordering::SeqCst) || self.old.busy() || self.slim.running.load(Ordering::SeqCst)
    }
    fn api_paused(&self) -> bool {
        self.storage_busy.load(Ordering::SeqCst) || self.slim.status.lock()["step"] == "rollback"
    }
    fn slim_running(&self) -> bool {
        self.slim.status.lock()["running"] == true
    }
    fn old_patches_busy(&self) -> bool {
        self.old.busy()
    }
    fn routes(&self) -> Router<Arc<App>> {
        Router::new()
            .route("/api/storage/stats", get(storage_stats_route))
            .route("/api/storage/inspect", get(storage_inspect))
            .route("/api/storage/clear", post(storage_clear))
            .route("/api/storage/migrate", post(storage_migrate))
            .route("/api/storage/slim", get(slim_get).post(slim_post))
            .route("/api/old-patches", get(old_get).put(old_put))
            .route("/api/db/purge-stale", post(purge_stale))
            .route("/api/backup", get(backup_get))
            .route("/api/backup/folder", put(backup_folder))
            .route("/api/backup/now", post(backup_now))
            .route("/api/backup/check", post(backup_check))
            .route("/api/import/inspect", get(import_inspect))
            .route("/api/import", post(import_post))
            .route("/api/import/dismiss", post(import_dismiss))
    }
    fn ws_initial(&self) -> Vec<Value> {
        let mut v = vec![];
        if let Some(c) = self.conversion.status.lock().clone() {
            v.push(json!({ "type": "conversion", "status": c }));
        }
        v.push(json!({ "type": "slim", "status": self.slim.status.lock().clone() }));
        v.push(json!({ "type": "backup", "status": self.backup.status() }));
        v
    }
    fn estimates(&self) -> serde_json::Map<String, Value> {
        self.old.estimates()
    }
}

/// The parts of the app running in this process (one app per process).
static PARTS: LazyLock<Mutex<Option<Arc<Parts>>>> = LazyLock::new(|| Mutex::new(None));

fn parts(_app: &App) -> Option<Arc<Parts>> {
    PARTS.lock().clone()
}

/// For tests: the app's storage parts (conversion, old patches, Slim, backups).
#[doc(hidden)]
pub fn parts_for_tests(app: &App) -> Option<Arc<Parts>> {
    parts(app)
}

/// For tests: the old patches' periodic check now (it runs a minute after the start, then hourly).
#[doc(hidden)]
pub async fn maintain_old_patches_now(app: &App) {
    if let Some(p) = parts(app) {
        p.old.maintain(&p).await;
    }
}

/// For tests: the data folder being moved or compacted (what blocks the old patches and Slim).
#[doc(hidden)]
pub fn set_storage_busy(app: &App, busy: bool) {
    if let Some(p) = parts(app) {
        p.storage_busy.store(busy, Ordering::SeqCst);
    }
}

// ================================================================== storage (routes/storage.ts)

async fn storage_stats_route(State(app): State<Arc<App>>) -> R {
    match parts(&app) {
        Some(p) => ok(tokio::task::spawn_blocking(move || p.stats()).await.unwrap_or(Value::Null)),
        None => err(StatusCode::SERVICE_UNAVAILABLE, "starting"),
    }
}

async fn storage_inspect(Query(q): Query<HashMap<String, String>>) -> R {
    ok(inspect_target(q.get("dir").map(|s| s.as_str()).unwrap_or(""), &config().data_dir))
}

async fn storage_clear(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> R {
    let Some(p) = parts(&app) else { return err(StatusCode::SERVICE_UNAVAILABLE, "starting") };
    let target = s(&body.map(|b| b.0["target"].clone()).unwrap_or(Value::Null));
    let targets = ["riot-cache", "riot-expired", "ddragon-old", "compact-db"];
    if !targets.contains(&target.as_str()) {
        return err(StatusCode::BAD_REQUEST, format!("target must be one of {}", targets.join(", ")));
    }
    if let Some(b) = p.conflict() {
        return err(StatusCode::CONFLICT, b);
    }
    let cfg = config();
    let r: anyhow::Result<(i64, i64)> = match target.as_str() {
        "riot-cache" => {
            let dir = cfg.riot_cache_dir.clone();
            p.as_action("clear-cache", "Clearing the API cache…", p.cache_files.load(Ordering::SeqCst).max(1) as f64, move |tick| DiskCache::prune(&dir, false, &mut |f| tick(f))).await
        }
        "riot-expired" => {
            let dir = cfg.riot_cache_dir.clone();
            p.as_action("clear-expired", "Clearing expired API answers…", 1.0, move |_| DiskCache::prune(&dir, true, &mut |_| {})).await
        }
        "ddragon-old" => {
            let dir = cfg.ddragon_cache_dir.clone();
            let version = app.data().version.clone();
            let history: Vec<String> = irelia::ddragon::history_versions(&app.data().versions.clone().unwrap_or_else(|| vec![version.clone()]), irelia::model::weights::HISTORY_PATCHES)
                .into_iter()
                .map(|(_, v)| v)
                .collect();
            p.as_action("ddragon-old", "Removing old Data Dragon patches…", 1.0, move |_| prune_ddragon(&dir, &version, &history).map(|(f, b)| (f as i64, b as i64))).await
        }
        _ => {
            let files = [cfg.db_path.clone(), PathBuf::from(format!("{}-wal", cfg.db_path.display()))];
            let before: u64 = files.iter().map(|f| irelia::util::measure(f).1).sum();
            p.storage_busy.store(true, Ordering::SeqCst);
            p.pause("the database is compacted").await;
            let a = app.clone();
            let r = p
                .as_action("compact", "Compacting the database…", before as f64, move |_| {
                    let db = a.db.lock();
                    let ids = db.normalize_participants()?;
                    db.conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL; VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")?;
                    db.set_meta(TIMELINE_CURSOR, "0")?;
                    let done = ids["done"] == true;
                    drop(db);
                    if done {
                        *a.db.lock() = Db::open_default(&config().db_path)?;
                    }
                    Ok(())
                })
                .await;
            p.storage_busy.store(false, Ordering::SeqCst);
            p.resume();
            r.map(|_| {
                let after: u64 = files.iter().map(|f| irelia::util::measure(f).1).sum();
                (0, before.saturating_sub(after) as i64)
            })
        }
    };
    match r {
        Ok((files, bytes)) => {
            let stats = tokio::task::spawn_blocking(move || p.stats()).await.unwrap_or(Value::Null);
            ok(json!({ "freedBytes": bytes, "files": files, "stats": stats }))
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn storage_progress(app: &App, p: &Parts, phase: &str, done: u64, total: u64, text: &str) {
    let mut v = json!({ "phase": phase, "doneBytes": done, "totalBytes": total, "text": text });
    if let Some(t) = p.moving.lock().as_ref() {
        v["progress"] = t.info(if total > 0 { Some(done as f64 / total as f64) } else { None });
    }
    app.broadcast(json!({ "type": "storage", "progress": v }));
}

async fn storage_migrate(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> R {
    let Some(p) = parts(&app) else { return err(StatusCode::SERVICE_UNAVAILABLE, "starting") };
    let b = body.map(|b| b.0).unwrap_or(json!({}));
    let mode = s(&b["mode"]);
    if !["move", "copy", "adopt"].contains(&mode.as_str()) {
        return err(StatusCode::BAD_REQUEST, "mode must be one of move, copy, adopt");
    }
    if let Some(by) = data_dir_locked_by() {
        return err(StatusCode::CONFLICT, format!("The data folder is pinned by the {by}."));
    }
    if let Some(c) = p.conflict() {
        return err(StatusCode::CONFLICT, c);
    }
    let from = config().data_dir;
    let check = inspect_target(&s(&b["targetDir"]), &from);
    let problems: Vec<String> = check["problems"].as_array().map(|a| a.iter().map(s).collect()).unwrap_or_default();
    if !problems.is_empty() {
        return err(StatusCode::BAD_REQUEST, problems.join(" "));
    }
    if mode != "adopt" && check["hasData"] == true {
        return err(StatusCode::CONFLICT, "That folder already contains Irelia Companion data. Use it as-is (\"Use existing data\") or pick an empty folder.");
    }
    let to = PathBuf::from(s(&check["path"]));
    let required = check["requiredBytes"].as_f64().unwrap_or(0.0);
    p.storage_busy.store(true, Ordering::SeqCst);
    *p.moving.lock() = Some(Progress::new(if mode == "adopt" { None } else { expected_ms("move", required, None) }));
    storage_progress(&app, &p, "preparing", 0, required as u64, "Pausing fetching and the model…");
    p.pause("the data folder moves").await;
    close_cache_stores();
    let (a, p2, mode2, to2, from2) = (app.clone(), p.clone(), mode.clone(), to.clone(), from.clone());
    let result = tokio::task::spawn_blocking(move || -> Result<(u64, Vec<String>), (bool, String)> {
        let (app, p, mode, to, from) = (a, p2, mode2, to2, from2);
        let mut copied = 0u64;
        let mut copy_started = false;
        let r: anyhow::Result<()> = (|| {
            if mode == "adopt" {
                std::fs::create_dir_all(&to)?;
            } else {
                let rows = app.db.lock().row_counts()?;
                copy_started = true;
                let db = &app.db;
                let copy = copy_data_dir(&from, &to, &|f| db.lock().vacuum_into(f), &mut |phase, done, total, text| storage_progress(&app, &p, phase, done, total, text))?;
                copied = copy.total_bytes;
                storage_progress(&app, &p, "verifying", copied, copied, "Verifying the copy…");
                verify_copy(&from, &to, &copy.files, Some(&rows), copy.cache_entries)?;
            }
            Ok(())
        })();
        if let Err(e) = r {
            if copy_started {
                let _ = remove_owned_data(&to);
            }
            return Err((false, e.to_string()));
        }
        storage_progress(&app, &p, "switching", copied, copied, "Switching to the new folder…");
        let next = Db::open_default(&data_paths(&to).db_path).map_err(|e| (false, e.to_string()))?;
        update_app_config(vec![("dataDir", if same_path(&to, &default_data_dir()) { None } else { Some(json!(to.to_string_lossy())) })]);
        set_data_dir(&to);
        *app.db.lock() = next;
        let mut leftovers = vec![];
        if mode == "move" {
            storage_progress(&app, &p, "cleaning", copied, copied, "Removing the old copy…");
            leftovers = remove_owned_data(&from).unwrap_or_else(|_| vec!["(could not remove the old files; delete them by hand)".into()]);
        }
        Ok((copied, leftovers))
    })
    .await
    .unwrap_or_else(|e| Err((false, e.to_string())));
    let resp = match result {
        Ok((copied, leftovers)) => {
            let text = if mode == "adopt" {
                format!("Now using the data in {}", to.display())
            } else {
                format!("{} {} to {}", if mode == "move" { "Moved" } else { "Copied" }, fmt_bytes(copied as f64), to.display())
            };
            if mode != "adopt" {
                if let Some(t) = p.moving.lock().as_ref() {
                    record_timing("move", Timing { size: copied as f64, ms: (now_ms() - t.started_at) as f64, at: now_ms() as f64, ..Default::default() });
                }
            }
            storage_progress(&app, &p, "done", copied, copied, &text);
            p.storage_busy.store(false, Ordering::SeqCst);
            *p.moving.lock() = None;
            p.resume();
            let p3 = p.clone();
            let stats = tokio::task::spawn_blocking(move || p3.stats()).await.unwrap_or(Value::Null);
            ok(json!({ "ok": true, "mode": mode, "from": from.to_string_lossy(), "dataDir": to.to_string_lossy(), "copiedBytes": copied, "leftovers": leftovers, "stats": stats }))
        }
        Err((switched, msg)) => {
            storage_progress(&app, &p, "error", 0, 0, &msg);
            p.storage_busy.store(false, Ordering::SeqCst);
            *p.moving.lock() = None;
            p.resume();
            err(StatusCode::INTERNAL_SERVER_ERROR, if switched { format!("Switched to {}, but: {msg}", to.display()) } else { format!("Nothing was changed: {msg}") })
        }
    };
    resp
}

async fn purge_stale(State(app): State<Arc<App>>) -> R {
    let Some(p) = parts(&app) else { return err(StatusCode::SERVICE_UNAVAILABLE, "starting") };
    if p.slim.status.lock()["running"] == true {
        return err(StatusCode::CONFLICT, "Slim stored data is running: try again once it is done.");
    }
    if p.old.busy() {
        return err(StatusCode::CONFLICT, "Old patches are being archived, restored or cleared from the cache.");
    }
    let data = app.data();
    let keep = irelia::model::run_eval::model_patches(&app.db.lock(), &data, irelia::model::weights::MEMORY_PATCHES).unwrap_or_default();
    let size = app.db.lock().stale_count(&keep) as f64;
    let tracker = Progress::new(if size > 0.0 { expected_ms("purge", size, None) } else { None });
    app.broadcast(json!({ "type": "storage-action", "action": { "action": "purge", "text": "Deleting the games of old patches…", "progress": tracker.info(None) } }));
    tokio::time::sleep(Duration::from_millis(30)).await;
    let a = app.clone();
    let r = tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
        let db = a.db.lock();
        let removed = db.purge_stale(&keep)?;
        if removed > 0 {
            db.reclaim()?;
        }
        Ok(removed)
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("{e}")));
    app.broadcast(json!({ "type": "storage-action", "action": null }));
    match r {
        Ok(removed) => {
            if size > 0.0 {
                record_timing("purge", Timing { size, ms: (now_ms() - tracker.started_at) as f64, at: now_ms() as f64, ..Default::default() });
            }
            ok(json!({ "removed": removed }))
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ================================================================== old patches (oldPatches.ts)

const TRIMMED_TO: &str = "cache_trimmed_to";
const BATCH_GAP_MS: u64 = 150;

pub struct OldPatches {
    app: Weak<App>,
    working: Mutex<Option<&'static str>>,
    error: Mutex<Option<String>>,
    last: Mutex<Option<String>>,
    queue: tokio::sync::Mutex<()>,
    progress: Mutex<Option<(Progress, Option<f64>, i64)>>,
}

impl OldPatches {
    fn new(app: Weak<App>) -> OldPatches {
        OldPatches { app, working: Mutex::new(None), error: Mutex::new(None), last: Mutex::new(None), queue: tokio::sync::Mutex::new(()), progress: Mutex::new(None) }
    }
    pub fn busy(&self) -> bool {
        self.working.lock().is_some()
    }
    fn archive_on(&self, has_archive: bool) -> bool {
        read_app_config().get("archiveOldPatches").and_then(|v| v.as_bool()).unwrap_or(has_archive)
    }
    pub fn status(&self) -> Value {
        let Some(app) = self.app.upgrade() else { return Value::Null };
        let archived = app.db.lock().archive_summary().unwrap_or_default();
        let mut s = json!({
            "archive": self.archive_on(!archived.is_empty()),
            "trimCache": read_app_config().get("trimCacheToWindow") == Some(&json!(true)),
            "working": *self.working.lock(),
        });
        if let (Some(_), Some((t, f, _))) = (*self.working.lock(), self.progress.lock().as_ref()) {
            s["progress"] = t.info(*f);
        }
        s["archived"] = json!(archived.iter().map(|(p, g, b)| json!({ "patch": p, "games": g, "bytes": b })).collect::<Vec<_>>());
        s["last"] = json!(*self.last.lock());
        s["error"] = json!(*self.error.lock());
        s
    }
    fn changed(&self) {
        if let Some(app) = self.app.upgrade() {
            app.broadcast(json!({ "type": "old-patches", "status": self.status() }));
        }
    }
    fn tick(&self, fraction: Option<f64>) {
        let tell = {
            let mut p = self.progress.lock();
            let Some(x) = p.as_mut() else { return };
            x.1 = fraction;
            if now_ms() - x.2 < 500 {
                false
            } else {
                x.2 = now_ms();
                true
            }
        };
        if tell {
            self.changed();
        }
    }
    fn window(&self) -> Vec<String> {
        let Some(app) = self.app.upgrade() else { return vec![] };
        let mut w = irelia::model::run_eval::model_patches(&app.db.lock(), &app.data(), irelia::model::weights::MEMORY_PATCHES).unwrap_or_default();
        w.sort();
        w
    }
    pub fn estimates(&self) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        if let Some(app) = self.app.upgrade() {
            let w = self.window();
            let db = app.db.lock();
            m.insert("archive".into(), json!(expected_ms("archive", db.stale_count(&w) as f64, None)));
            m.insert("restore".into(), json!(expected_ms("restore", db.archive_summary().unwrap_or_default().iter().map(|x| x.1 as f64).sum(), None)));
            m.insert("trim".into(), json!(expected_ms("trim", 1.0, None)));
        }
        m
    }

    async fn run(&self, parts: &Parts, kind: &'static str, quiet: bool) -> Value {
        let _q = self.queue.lock().await;
        if parts.storage_busy.load(Ordering::SeqCst) || parts.slim.running.load(Ordering::SeqCst) {
            if !quiet {
                *self.error.lock() = Some("The data folder is being moved or the database compacted. Try again in a moment.".into());
            }
            return self.status();
        }
        let Some(app) = self.app.upgrade() else { return Value::Null };
        let action = match kind {
            "archiving" => "archive",
            "restoring" => "restore",
            _ => "trim",
        };
        let size = match kind {
            "archiving" => {
                // The window first: it reads the database itself (its lock isn't reentrant).
                let window = self.window();
                app.db.lock().stale_count(&window) as f64
            }
            "restoring" => app.db.lock().archive_summary().unwrap_or_default().iter().map(|x| x.1 as f64).sum(),
            _ => 1.0,
        };
        let started = now_ms();
        *self.progress.lock() = Some((Progress::new(expected_ms(action, size, None)), if kind == "trimming" { None } else { Some(0.0) }, 0));
        *self.working.lock() = Some(kind);
        self.changed();
        let r: anyhow::Result<String> = match kind {
            "archiving" => {
                let keep = self.window();
                let mut n = 0usize;
                loop {
                    let a = app.clone();
                    let k = keep.clone();
                    let done = tokio::task::spawn_blocking(move || a.db.lock().archive_batch(&k, 200)).await.unwrap_or(Ok(0));
                    match done {
                        Ok(0) => break,
                        Ok(d) => {
                            n += d;
                            self.tick(Some(n as f64 / size.max(1.0)));
                            tokio::time::sleep(Duration::from_millis(BATCH_GAP_MS)).await;
                        }
                        Err(e) => {
                            *self.error.lock() = Some(e.to_string());
                            break;
                        }
                    }
                }
                if n > 0 {
                    let _ = app.db.lock().reclaim();
                }
                Ok(if n > 0 { format!("Archived {} matches of patches older than the window.", irelia::js::thousands(n as f64)) } else { "Nothing older than the window to archive.".into() })
            }
            "restoring" => {
                let mut n = 0usize;
                loop {
                    let a = app.clone();
                    let done = tokio::task::spawn_blocking(move || a.db.lock().restore_batch(None, 200)).await.unwrap_or(Ok(0));
                    match done {
                        Ok(0) => break,
                        Ok(d) => {
                            n += d;
                            self.tick(Some(n as f64 / size.max(1.0)));
                            tokio::time::sleep(Duration::from_millis(BATCH_GAP_MS)).await;
                        }
                        Err(e) => {
                            *self.error.lock() = Some(e.to_string());
                            break;
                        }
                    }
                }
                Ok(if n > 0 { format!("Restored {} archived matches.", irelia::js::thousands(n as f64)) } else { "The archive was empty.".into() })
            }
            _ => {
                let a = app.clone();
                let window = self.window();
                tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
                    let data = a.data();
                    let (old, patch_of) = {
                        let db = a.db.lock();
                        (irelia::model::service::before_window(&db, &data, irelia::model::weights::MEMORY_PATCHES)?, db.match_patches()?)
                    };
                    let (files, bytes) = DiskCache::prune_matches(&config().riot_cache_dir, &mut |id, version| {
                        let patch = patch_of.get(id).cloned().or_else(|| version().map(|v| irelia::ddragon::to_patch(&v)));
                        patch.map(|p| old(&p) == Some(true)).unwrap_or(false)
                    })?;
                    a.db.lock().set_meta(TRIMMED_TO, &window.join(","))?;
                    Ok(if files > 0 {
                        format!("Deleted {} cached files of old patches ({} MB).", irelia::js::thousands(files as f64), irelia::js::to_fixed(bytes as f64 / 1e6, 1))
                    } else {
                        "No cached files of old patches.".into()
                    })
                })
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("{e}")))
            }
        };
        match r {
            Ok(text) => {
                *self.last.lock() = Some(text);
                if self.error.lock().is_none() {
                    record_timing(action, Timing { size, ms: (now_ms() - started) as f64, at: now_ms() as f64, ..Default::default() });
                }
            }
            Err(e) => {
                *self.error.lock() = Some(e.to_string());
                eprintln!("[old patches] {kind} failed {e}");
            }
        }
        *self.working.lock() = None;
        *self.progress.lock() = None;
        let s = self.status();
        self.changed();
        s
    }

    async fn maintain(&self, parts: &Parts) {
        let Some(app) = self.app.upgrade() else { return };
        if parts.conflict().is_some() || app.inner.lock().matchup.phase != "idle" {
            return;
        }
        let window = self.window();
        let (has_archive, archivable, trimmed) = {
            let db = app.db.lock();
            (!db.archive_summary().unwrap_or_default().is_empty(), db.has_archivable(&window), db.get_meta(TRIMMED_TO))
        };
        if self.archive_on(has_archive) && archivable {
            *self.error.lock() = None;
            self.run(parts, "archiving", true).await;
        }
        if read_app_config().get("trimCacheToWindow") == Some(&json!(true)) && trimmed.as_deref() != Some(window.join(",").as_str()) {
            self.run(parts, "trimming", true).await;
        }
    }
}

async fn old_get(State(app): State<Arc<App>>) -> R {
    match parts(&app) {
        Some(p) => ok(p.old.status()),
        None => err(StatusCode::SERVICE_UNAVAILABLE, "starting"),
    }
}

async fn old_put(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> R {
    let Some(p) = parts(&app) else { return err(StatusCode::SERVICE_UNAVAILABLE, "starting") };
    let b = body.map(|b| b.0).unwrap_or(json!({}));
    let (archive, trim) = (b.get("archive").cloned(), b.get("trimCache").cloned());
    if archive.as_ref().map(|a| !a.is_boolean()).unwrap_or(false) || trim.as_ref().map(|a| !a.is_boolean()).unwrap_or(false) {
        return err(StatusCode::BAD_REQUEST, "archive and trimCache must be true or false");
    }
    let mut status = p.old.status();
    if let Some(a) = archive.and_then(|a| a.as_bool()) {
        update_app_config(vec![("archiveOldPatches", Some(json!(a)))]);
        status = p.old.run(&p, if a { "archiving" } else { "restoring" }, false).await;
    }
    if let Some(t) = trim.and_then(|t| t.as_bool()) {
        update_app_config(vec![("trimCacheToWindow", Some(json!(t)))]);
        if t {
            status = p.old.run(&p, "trimming", false).await;
        } else {
            status = p.old.status();
        }
    }
    ok(status)
}

// ================================================================== the conversion (convert.ts, convertWorker.ts)

pub const TIMELINE_CURSOR: &str = "timelines_z_cursor";

pub struct Conversion {
    app: Weak<App>,
    pub status: Mutex<Option<Value>>,
    running: Mutex<Option<Arc<AtomicBool>>>,
    busy: AtomicBool,
    paused: AtomicBool,
    shown_at: AtomicI64,
}

fn legacy_cache_left(cache_dir: &Path) -> bool {
    irelia::riot::disk_cache::KINDS.iter().any(|k| cache_dir.join(k).is_dir())
}

impl Conversion {
    fn new(app: Weak<App>) -> Conversion {
        Conversion { app, status: Mutex::new(None), running: Mutex::new(None), busy: AtomicBool::new(false), paused: AtomicBool::new(false), shown_at: AtomicI64::new(0) }
    }

    pub fn needed(&self) -> bool {
        let st = self.status.lock().clone();
        let unreadable_only = st
            .as_ref()
            .map(|s| s["running"] == false && s["cache"].is_object() && s["cache"]["done"] == s["cache"]["total"] && s["cache"]["skipped"].as_i64().unwrap_or(0) > 0)
            .unwrap_or(false);
        let Some(app) = self.app.upgrade() else { return false };
        let cursor: i64 = app.db.lock().get_meta(TIMELINE_CURSOR).and_then(|v| v.parse().ok()).unwrap_or(0);
        (legacy_cache_left(&config().riot_cache_dir) && !unreadable_only) || app.db.lock().plain_timelines_after(cursor)
    }

    /// Whether earlier versions' data is still being converted.
    pub fn working(&self) -> bool {
        self.running.lock().is_some() || (self.needed() && self.status.lock().as_ref().map(|s| s["error"].is_null()).unwrap_or(true))
    }

    pub fn check(&self) {
        if self.running.lock().is_some() || self.busy.load(Ordering::SeqCst) || self.paused.load(Ordering::SeqCst) || !self.needed() {
            return;
        }
        let flag = Arc::new(AtomicBool::new(true));
        *self.running.lock() = Some(flag.clone());
        let w = self.app.clone();
        std::thread::Builder::new()
            .name("convert".into())
            .spawn(move || {
                let show = |s: &Value| {
                    if let Some(app) = w.upgrade() {
                        if let Some(p) = parts(&app) {
                            p.conversion.show(s.clone());
                        }
                    }
                };
                let mut status = json!({ "running": true, "cache": null, "timelines": null, "error": null, "done": false });
                let cfg = config();
                if let Err(e) = convert_cache(&cfg.riot_cache_dir, &mut status, &flag, &show) {
                    status["error"] = json!(e.to_string());
                }
                if flag.load(Ordering::SeqCst) {
                    if let Err(e) = convert_timelines(&cfg.db_path, &mut status, &flag, &show) {
                        let prev = status["error"].as_str().map(|p| format!("{p}; {e}")).unwrap_or_else(|| e.to_string());
                        status["error"] = json!(prev);
                    }
                }
                if flag.load(Ordering::SeqCst) {
                    status["done"] = json!(status["error"].is_null());
                    status["running"] = json!(false);
                    show(&status);
                    if let Some(app) = w.upgrade() {
                        if let Some(p) = parts(&app) {
                            *p.conversion.running.lock() = None;
                            if !status["error"].is_null() {
                                eprintln!("[convert] stopped: {}", s(&status["error"]));
                            }
                        }
                    }
                }
            })
            .ok();
    }

    fn show(&self, s: Value) {
        let mut v = s.clone();
        if self.busy.load(Ordering::SeqCst) {
            v["paused"] = json!(true);
        }
        *self.status.lock() = Some(v.clone());
        if s["running"] == true && s.get("paused").is_none() && now_ms() - self.shown_at.load(Ordering::SeqCst) < 500 {
            return;
        }
        self.shown_at.store(now_ms(), Ordering::SeqCst);
        if let Some(app) = self.app.upgrade() {
            app.broadcast(json!({ "type": "conversion", "status": v }));
        }
    }

    fn stop(&self) {
        if let Some(f) = self.running.lock().take() {
            f.store(false, Ordering::SeqCst);
        }
    }

    pub fn set_busy(&self, busy: bool) {
        if self.busy.swap(busy, Ordering::SeqCst) == busy {
            return;
        }
        if busy {
            self.stop();
            let st = self.status.lock().clone();
            if let Some(mut st) = st.filter(|s| s["done"] != true && s["error"].is_null()) {
                st["running"] = json!(false);
                self.show(st);
            }
        } else {
            self.check();
        }
    }

    pub async fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
        self.stop();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.check();
    }
}

fn convert_cache(root: &Path, status: &mut Value, run: &AtomicBool, show: &dyn Fn(&Value)) -> anyhow::Result<()> {
    let mut kinds: Vec<&str> = irelia::riot::disk_cache::KINDS.to_vec();
    kinds.sort_by_key(|k| if *k == "match" || *k == "timeline" { 0 } else { 1 });
    let list = |kinds: &[&str]| -> Vec<(String, String, String, PathBuf)> {
        let mut out = vec![];
        for kind in kinds {
            let Ok(rd) = std::fs::read_dir(root.join(kind)) else { continue };
            for scope in rd.flatten() {
                let Ok(files) = std::fs::read_dir(scope.path()) else { continue };
                for f in files.flatten() {
                    let name = f.file_name().to_string_lossy().to_string();
                    if let Some(key) = name.strip_suffix(".json.gz") {
                        if f.path().is_file() {
                            out.push((kind.to_string(), scope.file_name().to_string_lossy().to_string(), key.to_string(), f.path()));
                        }
                    }
                }
            }
        }
        out
    };
    let files = list(&kinds);
    if files.is_empty() {
        return Ok(());
    }
    let total = files.len();
    let newest = |k: &str| files.iter().filter(|f| f.0 == k).max_by(|a, b| a.2.cmp(&b.2)).cloned();
    let store = CacheStore::open_default(root)?;
    status["cache"] = json!({ "done": 0, "total": total, "skipped": 0 });
    show(status);
    if let (Some(m), Some(t), None) = (newest("match"), newest("timeline"), store.dicts.current()) {
        if let (Ok(mb), Ok(tb)) = (std::fs::read(&m.3), std::fs::read(&t.3)) {
            if let (Ok(mr), Ok(tr)) = (irelia::db::gunzip(&mb), irelia::db::gunzip(&tb)) {
                let _ = store.ensure_dictionary(&mr, &tr);
            }
        }
    }
    let mut done = 0usize;
    for batch in files.chunks(40) {
        if !run.load(Ordering::SeqCst) {
            return Ok(());
        }
        let read: Vec<(&(String, String, String, PathBuf), i64, Option<Vec<u8>>)> = batch
            .iter()
            .filter_map(|f| {
                let meta = std::fs::metadata(&f.3).ok()?;
                let mtime = irelia::util::mtime_ms(&meta).round() as i64;
                let raw = std::fs::read(&f.3).ok().and_then(|b| irelia::db::gunzip(&b).ok());
                Some((f, mtime, raw))
            })
            .collect();
        let blobs: Vec<Option<Vec<u8>>> = read.iter().map(|(f, _, raw)| raw.as_ref().and_then(|r| store.encode(&f.0, r).ok())).collect();
        let mut gone = vec![];
        store.db.execute_batch("BEGIN IMMEDIATE")?;
        let r: anyhow::Result<()> = (|| {
            for (j, (f, mtime, raw)) in read.iter().enumerate() {
                let Some(raw) = raw else {
                    status["cache"]["skipped"] = json!(status["cache"]["skipped"].as_i64().unwrap_or(0) + 1);
                    continue;
                };
                let have = store.read(&f.0, &f.1, &f.2)?;
                if have.as_ref().map(|h| h.0 < *mtime).unwrap_or(true) {
                    let blob = blobs[j].as_ref().ok_or_else(|| anyhow::anyhow!("{} {} could not be encoded: its file is kept", f.0, f.2))?;
                    store.put(&f.0, &f.1, &f.2, blob, *mtime)?;
                    let back = store.read(&f.0, &f.1, &f.2)?;
                    if back.map(|b| b.1 != *raw).unwrap_or(true) {
                        return Err(anyhow::anyhow!("{} {} did not read back the same: its file is kept", f.0, f.2));
                    }
                }
                gone.push(f.3.clone());
            }
            Ok(())
        })();
        match r {
            Ok(()) => store.db.execute_batch("COMMIT")?,
            Err(e) => {
                let _ = store.db.execute_batch("ROLLBACK");
                return Err(e);
            }
        }
        for g in gone {
            let _ = std::fs::remove_file(g);
        }
        done = (done + read.len()).min(total);
        status["cache"]["done"] = json!(done);
        show(status);
        std::thread::sleep(Duration::from_millis(25));
    }
    for kind in irelia::riot::disk_cache::KINDS {
        if let Ok(rd) = std::fs::read_dir(root.join(kind)) {
            for scope in rd.flatten() {
                let _ = std::fs::remove_dir(scope.path());
            }
        }
        let _ = std::fs::remove_dir(root.join(kind));
    }
    let _ = store.reclaim();
    Ok(())
}

fn convert_timelines(db_path: &Path, status: &mut Value, run: &AtomicBool, show: &dyn Fn(&Value)) -> anyhow::Result<()> {
    let db = Db::open(db_path, OpenOptions { migrate: false, busy_timeout_ms: 30_000 })?;
    let mut cursor: i64 = db.get_meta(TIMELINE_CURSOR).and_then(|v| v.parse().ok()).unwrap_or(0);
    let last = db.last_timeline_row();
    if cursor >= last {
        return Ok(());
    }
    status["timelines"] = json!({ "fraction": if last > 0 { cursor as f64 / last as f64 } else { 1.0 }, "converted": 0 });
    show(status);
    let mut batches = 0;
    loop {
        if !run.load(Ordering::SeqCst) {
            return Ok(());
        }
        let (next, converted) = db.compress_timelines(cursor, 100)?;
        let Some(next) = next else { break };
        cursor = next;
        db.set_meta(TIMELINE_CURSOR, &cursor.to_string())?;
        status["timelines"]["converted"] = json!(status["timelines"]["converted"].as_i64().unwrap_or(0) + converted as i64);
        status["timelines"]["fraction"] = json!(f64::min(1.0, cursor as f64 / db.last_timeline_row().max(1) as f64));
        show(status);
        batches += 1;
        if batches % 20 == 0 {
            let _ = db.reclaim();
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    db.set_meta(TIMELINE_CURSOR, &cursor.max(db.last_timeline_row()).to_string())?;
    let _ = db.reclaim();
    status["timelines"]["fraction"] = json!(1);
    Ok(())
}

// ================================================================== Slim stored data (slimControl.ts, slimWorker.ts)

pub struct SlimControl {
    app: Weak<App>,
    pub status: Mutex<Value>,
    stop: Arc<AtomicBool>,
    pub running: AtomicBool,
}

fn message(e: &anyhow::Error) -> String {
    e.to_string()
}

impl SlimControl {
    fn initial(app: &Arc<App>) -> Value {
        let dir = config().data_dir;
        let manifest = match slim::read_manifest(&dir) {
            Some(mut m) => {
                let b = slim::backup_bytes(&dir);
                if b > 0.0 {
                    m["backupBytes"] = json!(b);
                }
                Some(m)
            }
            None => app.db.lock().get_meta(slim::SLIM_META).and_then(|v| serde_json::from_str::<Value>(&v).ok()).map(|mut last| {
                if last["step"] == "verified" {
                    last["step"] = json!("kept");
                    last["note"] = json!("The backup folder is gone: the slimmer data is kept.");
                }
                last
            }),
        };
        json!({ "running": false, "step": null, "part": null, "label": null, "manifest": manifest, "error": null })
    }

    fn show(&self, patch: Value) {
        let s = {
            let mut st = self.status.lock();
            for (k, v) in patch.as_object().unwrap() {
                st[k] = v.clone();
            }
            st.clone()
        };
        if let Some(app) = self.app.upgrade() {
            app.broadcast(json!({ "type": "slim", "status": s }));
        }
    }

    fn finish(&self, m: Value, error: Option<String>) {
        if let Some(app) = self.app.upgrade() {
            let _ = app.db.lock().set_meta(slim::SLIM_META, &irelia::js::stringify(&m));
        }
        self.running.store(false, Ordering::SeqCst);
        self.show(json!({ "running": false, "stopping": false, "step": null, "part": null, "label": null, "count": null, "progress": null, "manifest": m, "error": error }));
    }

    fn refusal(&self, parts: &Parts) -> Option<String> {
        let app = self.app.upgrade()?;
        if self.running.load(Ordering::SeqCst) {
            return Some("Slim stored data is already running.".into());
        }
        if app.inner.lock().matchup.phase != "idle" {
            return Some("Not during champ select or a game: try again after it.".into());
        }
        if parts.storage_busy.load(Ordering::SeqCst) {
            return Some("The data folder is being moved or the database compacted.".into());
        }
        if parts.old.busy() {
            return Some("Old patches are being archived, restored or cleared from the cache.".into());
        }
        if parts.conversion.working() {
            return Some("Stored data is still being converted to the smaller format: Slim stored data runs once that is done.".into());
        }
        if app.jobs.running_job("evaluate").is_some() || app.jobs.running_job("audit").is_some() {
            return Some("Evaluate builds or the data audit is running: Slim stored data can start once it is done.".into());
        }
        if slim::read_manifest(&config().data_dir).map(|m| m["step"] == "verified").unwrap_or(false) {
            return Some("The last run's backup is still there: choose Roll back or Keep the slimmer data first.".into());
        }
        None
    }

    async fn pause_writers(&self, parts: &Parts) {
        let Some(app) = self.app.upgrade() else { return };
        app.models.release();
        app.scraper.terminate("Slim stored data runs").await;
        app.jobs.cancel_all("Slim stored data runs").await;
        parts.conversion.pause().await;
    }
    fn resume_writers(&self, parts: &Parts) {
        if let Some(app) = self.app.upgrade() {
            app.scraper.respawn();
            parts.conversion.resume();
        }
    }

    async fn start(&self, parts: &Arc<Parts>) -> anyhow::Result<Value> {
        if let Some(why) = self.refusal(parts) {
            return Err(anyhow::anyhow!(why));
        }
        let app = self.app.upgrade().ok_or_else(|| anyhow::anyhow!("closing"))?;
        let mut m = json!({ "version": 1, "startedAt": now_ms(), "updatedAt": now_ms(), "step": "backup", "plan": null, "db": null, "cache": null });
        self.running.store(true, Ordering::SeqCst);
        self.stop.store(false, Ordering::SeqCst);
        self.show(json!({ "running": true, "step": "backup", "part": null, "label": "Checking the free space…", "count": null, "progress": null, "error": null, "manifest": m }));
        self.pause_writers(parts).await;
        let p = parts.clone();
        let stop = self.stop.clone();
        let data = app.data();
        tokio::task::spawn_blocking(move || {
            let cfg = config();
            let dir = cfg.data_dir.clone();
            let phase: Arc<Mutex<Option<(String, Progress)>>> = Arc::new(Mutex::new(None));
            let phase_ms: Arc<Mutex<HashMap<String, i64>>> = Arc::new(Mutex::new(HashMap::new()));
            let progress = |step: &str, ph: &str, part: Option<f64>, label: &str, count: Option<Value>| {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let key = format!("{step}|{ph}");
                let info = {
                    let mut cur = phase.lock();
                    if cur.as_ref().map(|c| c.0 != key).unwrap_or(true) {
                        if let Some((k, pr)) = cur.as_ref() {
                            phase_ms.lock().insert(k.clone(), now_ms() - pr.started_at);
                        }
                        let expected = if step == "verify" { phase_ms.lock().get(&format!("baseline|{ph}")).map(|x| *x as f64) } else { None };
                        *cur = Some((key, Progress::new(expected)));
                    }
                    cur.as_ref().unwrap().1.info(part)
                };
                p.slim.show(json!({ "step": step, "part": part, "label": label, "count": count, "progress": info }));
            };
            let ticks = |step: &'static str, prefix: &'static str| {
                let progress = &progress;
                move |ph: &str, label: &str, part: Option<f64>, count: Option<Value>| {
                    let l = if prefix.is_empty() { label.to_string() } else { format!("{prefix}: {label}…") };
                    progress(step, ph, part, &l, count)
                }
            };
            let stopped = || stop.load(Ordering::SeqCst) || p.app().map(|a| a.inner.lock().matchup.phase != "idle").unwrap_or(true);
            let outcome: Result<Vec<String>, (String, bool)> = (|| {
                let db = Db::open(&cfg.db_path, OpenOptions { migrate: false, busy_timeout_ms: 30_000 }).map_err(|e| (message(&e), false))?;
                let store = CacheStore::open_default(&cfg.riot_cache_dir).map_err(|e| (message(&e), false))?;
                let plan = slim::plan_slim(&db, &store, &data).map_err(|e| (message(&e), false))?;
                m["plan"] = plan.plan.clone();
                if let Some(problem) = slim::space_problem(&dir, plan.plan["neededBytes"].as_f64().unwrap_or(0.0)) {
                    return Err((problem, true));
                }
                let step = |m: &mut Value, s: &str| -> Result<(), (String, bool)> {
                    m["step"] = json!(s);
                    slim::write_manifest(&dir, m, Some(&db)).map_err(|e| (message(&e), false))
                };
                step(&mut m, "backup")?;
                let t = ticks("backup", "");
                slim::backup(&dir, &db, &store, &plan, &mut m, &t).map_err(|e| (message(&e), false))?;
                if stopped() {
                    return Err(("you stopped it".into(), false));
                }
                step(&mut m, "baseline")?;
                let t = ticks("baseline", "Recording the results before");
                let before = slim::results(&db, &data, 1, 200, &t, &stopped).map_err(|e| (message(&e), false))?;
                m["baseline"] = json!(slim::save_baseline(&dir, &before).map_err(|e| (message(&e), false))?);
                slim::write_manifest(&dir, &mut m, Some(&db)).map_err(|e| (message(&e), false))?;
                step(&mut m, "apply")?;
                let t = ticks("apply", "");
                slim::apply(&dir, &db, &store, &mut m, &t, irelia::sys::cores().saturating_sub(1).clamp(1, 8), &stopped).map_err(|e| (message(&e), false))?;
                if std::env::var("IRELIA_SLIM_TEST_DIFFERENCE").as_deref() == Ok("1") {
                    let _ = db.conn.execute_batch("UPDATE irelia_games SET win = 1 - win WHERE match_id IN (SELECT match_id FROM irelia_games ORDER BY match_id LIMIT 8)");
                }
                step(&mut m, "verify")?;
                let t = ticks("verify", "Checking the results after");
                let after = slim::results(&db, &data, 1, 200, &t, &stopped).map_err(|e| (message(&e), false))?;
                let base = slim::read_baseline(&dir).map_err(|e| (message(&e), false))?;
                let after: Value = serde_json::from_str(&irelia::js::stringify(&after)).unwrap_or(Value::Null);
                let diff = slim::differences(&base, &after, 10);
                if diff.is_empty() {
                    step(&mut m, "verified")?;
                } else {
                    m["differences"] = json!(diff);
                    slim::write_manifest(&dir, &mut m, Some(&db)).map_err(|e| (message(&e), false))?;
                }
                Ok(diff)
            })();
            let rt = p.app().map(|a| a.rt.clone());
            if let Some(rt) = rt {
                let p2 = p.clone();
                rt.spawn(async move { p2.slim.ended(&p2, m, outcome).await });
            }
        });
        Ok(self.status.lock().clone())
    }

    async fn ended(&self, parts: &Arc<Parts>, m: Value, outcome: Result<Vec<String>, (String, bool)>) {
        let dir = config().data_dir;
        let failure: Option<(String, bool)> = match (&outcome, self.stop.load(Ordering::SeqCst)) {
            (_, true) => Some(("you stopped it".to_string(), false)),
            (Err((e, refused)), false) => Some((e.clone(), *refused)),
            (Ok(_), false) => None,
        };
        if let Some((error, refused)) = failure {
            if refused {
                self.resume_writers(parts);
                let mut v = m.clone();
                v["step"] = json!("cancelled");
                v["note"] = json!(error.clone());
                self.finish(v, Some(error));
                return;
            }
            let latest = slim::read_manifest(&dir).unwrap_or_else(|| m.clone());
            let step = latest["step"].as_str().unwrap_or("").to_string();
            if step == "apply" || step == "verify" {
                self.roll_back_file(parts, latest, format!("Stopped while slimming ({error}): rolled back."), None).await;
                return;
            }
            let cache = config().riot_cache_dir;
            let d2 = dir.clone();
            let r = tokio::task::spawn_blocking(move || slim::restore_entries(&d2, &cache, &|_| {}).map(|_| slim::remove_backup(&d2))).await;
            if let Ok(Err(e)) = r {
                eprintln!("[slim] clean-up: {e}");
            }
            self.resume_writers(parts);
            let mut v = latest;
            v["step"] = json!("cancelled");
            v["note"] = json!(format!("Stopped before slimming ({error}): nothing was changed."));
            self.finish(v, None);
            return;
        }
        let diff = outcome.unwrap_or_default();
        if diff.is_empty() {
            let mut v = m.clone();
            if let Some(a) = m.get("applied") {
                v["freedBytes"] = json!(m["plan"]["timelineBytes"].as_f64().unwrap_or(0.0) + a["savedBytes"].as_f64().unwrap_or(0.0));
            }
            v["backupBytes"] = json!(slim::backup_bytes(&dir));
            if let Some(app) = self.app.upgrade() {
                let _ = slim::write_manifest(&dir, &mut v, Some(&app.db.lock()));
            }
            self.resume_writers(parts);
            self.finish(v, None);
            return;
        }
        self.show(json!({ "step": "rollback", "part": null, "label": "A result differed: rolling back…", "count": null, "progress": null }));
        let note = format!(
            "Rolled back by itself: {} result{} differed ({}).",
            diff.len(),
            if diff.len() == 1 { "" } else { "s" },
            diff.iter().take(3).cloned().collect::<Vec<_>>().join("; ")
        );
        self.roll_back_file(parts, m, note, Some(diff)).await;
    }

    async fn roll_back_file(&self, parts: &Arc<Parts>, m: Value, note: String, differences: Option<Vec<String>>) {
        let Some(app) = self.app.upgrade() else { return };
        let m2 = m.clone();
        let mm = app.models.clone();
        let _ = tokio::task::spawn_blocking(move || mm.terminate()).await;
        let a = app.clone();
        let r = tokio::task::spawn_blocking(move || -> anyhow::Result<(i64, String)> {
            // The database closes for the file to be replaced: an in-memory one stands in meanwhile.
            let held = std::mem::replace(&mut *a.db.lock(), Db::open_default(Path::new(":memory:"))?);
            drop(held);
            close_cache_stores();
            let cfg = config();
            let r = slim::rollback_file(&cfg.data_dir, &cfg.riot_cache_dir, &m2, &|_| {});
            *a.db.lock() = Db::open_default(&cfg.db_path)?;
            r
        })
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("{e}")));
        app.models.respawn();
        app.request_build(60);
        match r {
            Ok((entries, sha)) => {
                slim::remove_backup(&config().data_dir);
                self.resume_writers(parts);
                let mut v = m.clone();
                v["step"] = json!("rolled-back");
                if let Some(d) = differences {
                    v["differences"] = json!(d);
                }
                v["note"] = json!(format!(
                    "{note} The database matches its backup (SHA-256 {}…); {} cache entries are back.",
                    &sha[..12.min(sha.len())],
                    irelia::js::thousands(entries as f64)
                ));
                self.finish(v, None);
            }
            Err(e) => {
                self.resume_writers(parts);
                self.finish(m, Some(format!("The rollback failed: {e}. The backup is kept in {}.", slim::slim_dir(&config().data_dir).display())));
            }
        }
    }

    fn stop_run(&self) -> anyhow::Result<Value> {
        if !self.running.load(Ordering::SeqCst) {
            return Err(anyhow::anyhow!("Slim stored data is not running."));
        }
        self.stop.store(true, Ordering::SeqCst);
        self.show(json!({ "stopping": true, "part": null, "count": null, "progress": null, "label": "Stopping: undoing what it did so far…" }));
        Ok(self.status.lock().clone())
    }

    pub fn set_busy(&self, busy: bool) {
        if busy && self.running.load(Ordering::SeqCst) {
            self.stop.store(true, Ordering::SeqCst);
        }
    }

    async fn rollback(&self, parts: &Arc<Parts>) -> anyhow::Result<Value> {
        let dir = config().data_dir;
        let m = slim::read_manifest(&dir).filter(|m| m["step"] == "verified").ok_or_else(|| anyhow::anyhow!("There is no verified run to roll back."))?;
        let app = self.app.upgrade().ok_or_else(|| anyhow::anyhow!("closing"))?;
        if app.inner.lock().matchup.phase != "idle" {
            return Err(anyhow::anyhow!("Not during champ select or a game: try again after it."));
        }
        self.running.store(true, Ordering::SeqCst);
        self.show(json!({ "running": true, "step": "rollback", "part": null, "label": "Rolling back…", "count": null, "progress": null, "error": null }));
        self.pause_writers(parts).await;
        let a = app.clone();
        let r = tokio::task::spawn_blocking(move || {
            let cfg = config();
            slim::rollback_rows(&cfg.data_dir, &cfg.riot_cache_dir, &a.db.lock())
        })
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("{e}")));
        match r {
            Ok((entries, archived, since)) => {
                slim::remove_backup(&dir);
                let mut v = m.clone();
                v["step"] = json!("rolled-back");
                v["note"] = json!(format!(
                    "Rolled back: {} cache entries and {} archived matches are as they were (checked by SHA-256){}.",
                    irelia::js::thousands(entries as f64),
                    irelia::js::thousands(archived as f64),
                    if since > 0 { format!("; {since} archived matches restored to the tables since keep their cleared columns") } else { String::new() }
                ));
                self.finish(v, None);
            }
            Err(e) => self.finish(m, Some(format!("The rollback failed: {e}. The backup is kept."))),
        }
        self.resume_writers(parts);
        Ok(self.status.lock().clone())
    }

    fn keep(&self) -> anyhow::Result<Value> {
        let dir = config().data_dir;
        let m = slim::read_manifest(&dir).filter(|m| m["step"] == "verified").ok_or_else(|| anyhow::anyhow!("There is no verified run to keep."))?;
        slim::remove_backup(&dir);
        let mut v = m;
        v["step"] = json!("kept");
        v["note"] = json!("Kept the slimmer data: the backup was deleted.");
        self.finish(v, None);
        Ok(self.status.lock().clone())
    }
}


async fn slim_get(State(app): State<Arc<App>>) -> R {
    match parts(&app) {
        Some(p) => ok(p.slim.status.lock().clone()),
        None => err(StatusCode::SERVICE_UNAVAILABLE, "starting"),
    }
}

async fn slim_post(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> R {
    let Some(p) = parts(&app) else { return err(StatusCode::SERVICE_UNAVAILABLE, "starting") };
    let action = body.and_then(|b| b.0.get("action").cloned()).map(|a| s(&a)).unwrap_or_else(|| "start".into());
    let r = match action.as_str() {
        "start" => p.slim.start(&p).await,
        "rollback" => p.slim.rollback(&p).await,
        "keep" => p.slim.keep(),
        "stop" => p.slim.stop_run(),
        _ => return err(StatusCode::BAD_REQUEST, "action must be start, stop, rollback or keep"),
    };
    match r {
        Ok(v) => ok(v),
        Err(e) => err(StatusCode::CONFLICT, e.to_string()),
    }
}

// ================================================================== backup (routes/backup.ts, transfers.ts, autoExport.ts)

const KEEP_POOL_EXPORTS: usize = 3;
const WEEK: i64 = 7 * 24 * 3_600_000;

pub struct Backup {
    app: Weak<App>,
    transfer: Mutex<Option<Value>>,
    running: tokio::sync::Mutex<()>,
    busy: AtomicBool,
    offers: Mutex<Vec<Value>>,
    offer_cache: Mutex<HashMap<String, (i64, Option<Value>)>>,
    last_error: Mutex<Option<String>>,
}

fn fmt_mb(b: f64) -> String {
    if b >= 1024f64.powi(3) {
        format!("{} GB", irelia::js::to_fixed(b / 1024f64.powi(3), 1))
    } else {
        format!("{} MB", irelia::js::num_to_string(f64::max(1.0, irelia::js::round(b / 1024f64.powi(2)))))
    }
}

impl Backup {
    fn new(app: Weak<App>) -> Backup {
        Backup {
            app,
            transfer: Mutex::new(None),
            running: tokio::sync::Mutex::new(()),
            busy: AtomicBool::new(false),
            offers: Mutex::new(vec![]),
            offer_cache: Mutex::new(HashMap::new()),
            last_error: Mutex::new(None),
        }
    }

    fn settings(&self) -> Option<Value> {
        read_app_config().get("autoExport").cloned().filter(|s| s["folder"].as_str().map(|f| !f.is_empty()).unwrap_or(false))
    }

    pub fn status(&self) -> Value {
        let auto = self.settings().map(|s| json!({ "folder": s["folder"], "last": s.get("last").cloned().unwrap_or(Value::Null), "error": *self.last_error.lock() }));
        json!({ "transfer": *self.transfer.lock(), "autoExport": auto, "offers": *self.offers.lock() })
    }

    fn changed(&self) {
        if let Some(app) = self.app.upgrade() {
            app.broadcast(json!({ "type": "backup", "status": self.status() }));
        }
    }

    /// Runs a transfer; `wait`: after the one running, else refused while one runs.
    async fn run(&self, kind: &'static str, path: PathBuf, wait: bool) -> anyhow::Result<Value> {
        if self.busy.load(Ordering::SeqCst) && !wait {
            let label = self.transfer.lock().as_ref().and_then(|t| t["label"].as_str().map(String::from)).unwrap_or_else(|| "A transfer".into());
            return Err(anyhow::anyhow!("{label} is still running: wait for it to finish."));
        }
        let _g = self.running.lock().await;
        self.busy.store(true, Ordering::SeqCst);
        let label = if kind == "import" {
            format!("Importing {}", path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default())
        } else {
            "Backing up the pool".to_string()
        };
        *self.transfer.lock() = Some(json!({ "kind": kind, "label": label, "fraction": 0, "startedAt": now_ms(), "finishedAt": null, "error": null, "result": null }));
        self.changed();
        let w = self.app.clone();
        let label2 = label.clone();
        let p2 = path.clone();
        let r = tokio::task::spawn_blocking(move || {
            let last = AtomicI64::new(0);
            let progress = |l: &str, f: Option<f64>| {
                if let Some(app) = w.upgrade() {
                    if let Some(p) = parts(&app) {
                        if let Some(t) = p.backup.transfer.lock().as_mut() {
                            t["label"] = json!(format!("{label2} · {l}"));
                            t["fraction"] = json!(f);
                        }
                        if now_ms() - last.load(Ordering::SeqCst) > 300 {
                            last.store(now_ms(), Ordering::SeqCst);
                            p.backup.changed();
                        }
                    }
                }
            };
            let data_dir = config().data_dir;
            if kind == "import" {
                irelia::transfer::import_export(&data_dir, &p2, true, &progress)
            } else {
                irelia::transfer::export_pool(&data_dir, &p2, &irelia::model::eval_base::app_version(), &progress)
            }
        })
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("{e}")));
        {
            let mut t = self.transfer.lock();
            let mut v = t.clone().unwrap_or(json!({}));
            v["label"] = json!(label);
            v["finishedAt"] = json!(now_ms());
            match &r {
                Ok(x) => {
                    v["fraction"] = json!(1);
                    v["result"] = json!(if kind == "import" {
                        format!("Added {} new games to the pool", irelia::js::thousands(x["added"].as_f64().unwrap_or(0.0)))
                    } else {
                        format!(
                            "Saved {} ({}, {} games{})",
                            Path::new(x["file"].as_str().unwrap_or("")).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default(),
                            fmt_mb(x["bytes"].as_f64().unwrap_or(0.0)),
                            irelia::js::thousands(x["games"].as_f64().unwrap_or(0.0)),
                            if x["evaluation"] == true { ", with this computer's last evaluation" } else { "" }
                        )
                    });
                    v["file"] = x.get("file").cloned().unwrap_or(Value::Null);
                }
                Err(e) => v["error"] = json!(e.to_string()),
            }
            *t = Some(v);
        }
        self.busy.store(false, Ordering::SeqCst);
        self.changed();
        r
    }

    async fn back_up(&self) -> anyhow::Result<()> {
        let s = self.settings().ok_or_else(|| anyhow::anyhow!("Choose a backup folder first."))?;
        let folder = PathBuf::from(s["folder"].as_str().unwrap_or(""));
        let r = self.run("export", folder.clone(), true).await;
        match r {
            Ok(_) => {
                *self.last_error.lock() = None;
                let mut next = s.clone();
                next["last"] = json!(now_ms());
                update_app_config(vec![("autoExport", Some(next))]);
                irelia::transfer::prune_exports(&folder, irelia::transfer::POOL_EXPORT, KEEP_POOL_EXPORTS, Some(&irelia::transfer::hostname()));
                self.changed();
                Ok(())
            }
            Err(e) => {
                *self.last_error.lock() = Some(e.to_string());
                self.changed();
                Err(e)
            }
        }
    }

    async fn check(&self) {
        let Some(s) = self.settings() else {
            self.offers.lock().clear();
            self.changed();
            return;
        };
        if now_ms() - s["last"].as_i64().unwrap_or(0) >= WEEK {
            if let Err(e) = self.back_up().await {
                eprintln!("[auto-backup] {e}");
            }
        }
        self.scan();
    }

    fn scan(&self) {
        let Some(s) = self.settings() else {
            self.offers.lock().clear();
            self.changed();
            return;
        };
        let folder = PathBuf::from(s["folder"].as_str().unwrap_or(""));
        let mut files: Vec<String> = std::fs::read_dir(&folder)
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|f| irelia::transfer::export_owner(f).is_some()).collect())
            .unwrap_or_default();
        files.sort();
        files.reverse();
        let mut offers = vec![];
        if let Some(first) = files.first() {
            let file = folder.join(first);
            if let Ok(meta) = std::fs::metadata(&file) {
                let mtime = irelia::util::mtime_ms(&meta) as i64;
                let key = file.to_string_lossy().to_string();
                let hit = self.offer_cache.lock().get(&key).cloned().filter(|h| h.0 == mtime);
                let offer = match hit {
                    Some(h) => h.1,
                    None => {
                        let o = irelia::transfer::inspect_export(&file).ok().and_then(|(m, _)| {
                            (m["machine"].as_str() != Some(irelia::transfer::hostname().as_str())).then(|| {
                                json!({ "file": key, "kind": "pool", "name": m["name"], "exportedAt": m["exportedAt"], "games": m["games"], "bytes": meta.len() })
                            })
                        });
                        self.offer_cache.lock().insert(key.clone(), (mtime, o.clone()));
                        o
                    }
                };
                if let Some(o) = offer {
                    let seen = s["seen"][first.as_str()].as_f64();
                    if !seen.map(|x| x >= o["exportedAt"].as_f64().unwrap_or(0.0)).unwrap_or(false) {
                        offers.push(o);
                    }
                }
            }
        }
        *self.offers.lock() = offers;
        self.changed();
    }

    fn seen(&self, file: &str, exported_at: f64) {
        let Some(mut s) = read_app_config().get("autoExport").cloned() else { return };
        let name = Path::new(file).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
        if !s["seen"].is_object() {
            s["seen"] = json!({});
        }
        s["seen"][name] = json!(exported_at);
        update_app_config(vec![("autoExport", Some(s))]);
        self.offers.lock().retain(|o| o["file"].as_str() != Some(file));
        self.changed();
    }

    fn configure(&self, folder: Option<PathBuf>) {
        let cur = read_app_config().get("autoExport").cloned();
        let v = folder.map(|f| {
            let mut v = json!({ "folder": irelia::util::resolve(f).to_string_lossy() });
            if let Some(last) = cur.as_ref().and_then(|c| c.get("last")) {
                v["last"] = last.clone();
            }
            v["seen"] = cur.as_ref().and_then(|c| c.get("seen").cloned()).unwrap_or(json!({}));
            v
        });
        update_app_config(vec![("autoExport", v)]);
        self.offer_cache.lock().clear();
        *self.last_error.lock() = None;
    }
}

fn absolute(v: &Value, what: &str) -> Result<PathBuf, String> {
    match v.as_str() {
        Some(p) if Path::new(p).is_absolute() => Ok(irelia::util::resolve(p)),
        _ => Err(format!("Choose {what}.")),
    }
}

async fn backup_get(State(app): State<Arc<App>>) -> R {
    match parts(&app) {
        Some(p) => ok(p.backup.status()),
        None => err(StatusCode::SERVICE_UNAVAILABLE, "starting"),
    }
}

async fn backup_folder(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> R {
    let Some(p) = parts(&app) else { return err(StatusCode::SERVICE_UNAVAILABLE, "starting") };
    let b = body.map(|b| b.0).unwrap_or(json!({}));
    let folder = match &b["folder"] {
        Value::Null => None,
        Value::String(x) if x.is_empty() => None,
        v => match absolute(v, "a folder") {
            Ok(f) => Some(f),
            Err(e) => return err(StatusCode::BAD_REQUEST, e),
        },
    };
    if let Some(f) = &folder {
        if let Err(e) = std::fs::create_dir_all(f) {
            return err(StatusCode::BAD_REQUEST, e.to_string());
        }
    }
    p.backup.configure(folder);
    let p2 = p.clone();
    app.rt.spawn(async move { p2.backup.check().await });
    ok(p.backup.status())
}

async fn backup_now(State(app): State<Arc<App>>) -> R {
    let Some(p) = parts(&app) else { return err(StatusCode::SERVICE_UNAVAILABLE, "starting") };
    let p2 = p.clone();
    app.rt.spawn(async move {
        let _ = p2.backup.back_up().await;
    });
    ok(p.backup.status())
}

async fn backup_check(State(app): State<Arc<App>>) -> R {
    let Some(p) = parts(&app) else { return err(StatusCode::SERVICE_UNAVAILABLE, "starting") };
    let p2 = p.clone();
    app.rt.spawn(async move { p2.backup.check().await });
    ok(p.backup.status())
}

async fn import_inspect(Query(q): Query<HashMap<String, String>>) -> R {
    let file = match absolute(&q.get("file").map(|f| json!(f)).unwrap_or(Value::Null), "a ZIP file") {
        Ok(f) => f,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    match irelia::transfer::inspect_export(&file) {
        Ok((m, _)) => ok(json!({
            "file": file.to_string_lossy(), "name": m["name"], "exportedAt": m["exportedAt"], "games": m["games"], "patches": m["patches"],
            "machine": m["machine"], "bytes": std::fs::metadata(&file).map(|x| x.len()).unwrap_or(0),
        })),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

async fn import_post(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> R {
    let Some(p) = parts(&app) else { return err(StatusCode::SERVICE_UNAVAILABLE, "starting") };
    let b = body.map(|b| b.0).unwrap_or(json!({}));
    let file = match absolute(&b["file"], "a ZIP file") {
        Ok(f) => f,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    if b["confirm"] != true {
        return err(StatusCode::BAD_REQUEST, "Importing adds games to the pool and changes its builds: confirm it first.");
    }
    if p.backup.busy.load(Ordering::SeqCst) {
        let label = p.backup.transfer.lock().as_ref().and_then(|t| t["label"].as_str().map(String::from)).unwrap_or_else(|| "A transfer".into());
        return err(StatusCode::BAD_REQUEST, format!("{label} is still running."));
    }
    let (p2, a) = (p.clone(), app.clone());
    app.rt.spawn(async move {
        if let Ok(r) = p2.backup.run("import", file.clone(), false).await {
            if let Some(at) = r["exportedAt"].as_f64() {
                p2.backup.seen(&file.to_string_lossy(), at);
            }
            let note = if r["evaluation"] == true { a.adopt_evaluation() } else { None };
            if a.inner.lock().matchup.phase != "game" {
                a.request_build(60);
            }
            if let Some(more) = note {
                if let Some(t) = p2.backup.transfer.lock().as_mut() {
                    let mut c = more.chars();
                    let cap = c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default();
                    t["result"] = json!(format!("{}. {cap}.", t["result"].as_str().unwrap_or("Done")));
                }
                p2.backup.changed();
            }
        }
    });
    ok(p.backup.status())
}

async fn import_dismiss(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> R {
    let Some(p) = parts(&app) else { return err(StatusCode::SERVICE_UNAVAILABLE, "starting") };
    let b = body.map(|b| b.0).unwrap_or(json!({}));
    let file = match absolute(&b["file"], "a ZIP file") {
        Ok(f) => f,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let at = irelia::js::to_number(&b["exportedAt"]);
    p.backup.seen(&file.to_string_lossy(), if at.is_finite() && at != 0.0 { at } else { now_ms() as f64 });
    ok(p.backup.status())
}
