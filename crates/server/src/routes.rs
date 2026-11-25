//! The REST API and `/ws`, on 127.0.0.1 only. JSON is the contract's (`bf_core::build`, camelCase) plus the views in
//! `app.rs` and `handles.rs`. The API key is never in a body: only `mask_key`'s form, and every error text is scrubbed
//! of anything key-shaped before it is sent.

use crate::app::App;
use crate::handles::{KeyOffer, SettingsPatch};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bf_core::build::{DraftContext, RankBand, Role};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

pub const DEVELOPER_PORTAL: &str = "https://developer.riotgames.com";
pub const MIN_DISK_BUDGET: u64 = 256 * 1024 * 1024;
pub const MAX_DISK_BUDGET: u64 = 1024 * 1024 * 1024 * 1024;
pub const MAX_TIER_A: usize = 50;

type R = Response;

/// An error answer: `{ "error": "…" }`, its text scrubbed of key-shaped strings.
pub struct ApiError(StatusCode, String);

impl ApiError {
    pub fn bad(msg: impl ToString) -> ApiError {
        ApiError(StatusCode::BAD_REQUEST, msg.to_string())
    }
    fn conflict(msg: impl ToString) -> ApiError {
        ApiError(StatusCode::CONFLICT, msg.to_string())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": scrub(&self.1) }))).into_response()
    }
}

static KEY_SHAPED: LazyLock<regex_lite::Regex> = LazyLock::new(|| regex_lite::Regex::new(r"RGAPI-[A-Za-z0-9-]+").expect("regex"));

/// Replaces anything that looks like a Riot API key with its masked form. Belt and braces: no handle should ever put
/// a key in an error, but an answer must never carry one even if it did.
pub fn scrub(text: &str) -> String {
    KEY_SHAPED.replace_all(text, |c: &regex_lite::Captures| bf_core::config::mask_key(&c[0])).into_owned()
}

fn ok<T: Serialize>(v: T) -> R {
    Json(v).into_response()
}

pub fn router(app: Arc<App>, web_dir: Option<PathBuf>) -> Router {
    let r = Router::new()
        .route("/api/health", get(health))
        .route("/api/status", get(status))
        .route("/api/static", get(static_view))
        .route("/api/build", get(build))
        .route("/api/draft", get(draft))
        .route("/api/live", get(live))
        .route("/api/import", post(import))
        .route("/api/fetch", get(fetch))
        .route("/api/storage", get(storage))
        .route("/api/data-age", get(data_age))
        .route("/api/model", get(model))
        .route("/api/model/rollback", post(rollback))
        .route("/api/settings", get(settings_get).patch(settings_patch))
        .route("/api/key", post(key_paste))
        .route("/api/key/import-irelia", post(key_import))
        .route("/api/backup/export", post(backup_export))
        .route("/api/backup/import", post(backup_import))
        .route("/api/backup/pending", get(backup_pending))
        .route("/api/backup/cancel", post(backup_cancel))
        .route("/api/dev/simulate-live", post(simulate_live))
        .route("/api/dev/stop-simulation", post(stop_simulation))
        .route("/api/dev/client-event", post(client_event))
        .route("/ws", get(ws))
        .layer(axum::middleware::from_fn(loopback_only));
    let r = match web_dir.filter(|d| d.join("index.html").exists()) {
        // The dashboard and /overlay: index.html for any path that isn't a file (client-side routes).
        Some(d) => {
            let index = d.join("index.html");
            r.fallback_service(tower_http::services::ServeDir::new(&d).fallback(tower_http::services::ServeFile::new(index)))
        }
        None => r,
    };
    r.with_state(app)
}

fn is_loopback_host(h: &str) -> bool {
    let host = h.rsplit_once(':').filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit())).map(|(h, _)| h).unwrap_or(h);
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

/// Host and Origin, when present, name this PC.
pub fn loopback_headers(h: &axum::http::HeaderMap) -> bool {
    let ok = |name: header::HeaderName, check: fn(&str) -> bool| h.get(name).is_none_or(|v| v.to_str().map(check).unwrap_or(false));
    ok(header::HOST, is_loopback_host) && ok(header::ORIGIN, |o| o.strip_prefix("http://").is_some_and(is_loopback_host))
}

/// The server listens on 127.0.0.1, but a web page in the user's browser can still send it requests. A Host that
/// isn't loopback is a DNS-rebinding attempt; an Origin that isn't loopback is another site (CSRF, or reading /ws).
/// The dashboard (the app's window, or Vite on localhost:5173 in development) passes both.
async fn loopback_only(req: Request, next: Next) -> R {
    if !loopback_headers(req.headers()) {
        return (StatusCode::FORBIDDEN, Json(json!({ "error": "Only the app's own pages may use this server." }))).into_response();
    }
    next.run(req).await
}

// ---------------------------------------------------------------- status

async fn health() -> R {
    ok(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
}

/// The handles may read the disk: their calls run off the async threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ApiError::from(anyhow::Error::from(e)))
}

async fn status(State(app): State<Arc<App>>) -> Result<R, ApiError> {
    Ok(ok(blocking(move || app.status()).await?))
}

async fn static_view(State(app): State<Arc<App>>) -> R {
    let d = app.static_data();
    ok(crate::static_data::view(d.as_deref()))
}

async fn fetch(State(app): State<Arc<App>>) -> Result<R, ApiError> {
    Ok(ok(blocking(move || app.h.store.fetch_status()).await?))
}

async fn storage(State(app): State<Arc<App>>) -> Result<R, ApiError> {
    Ok(ok(blocking(move || app.h.store.storage_status()).await?))
}

async fn data_age(State(app): State<Arc<App>>) -> Result<R, ApiError> {
    Ok(ok(blocking(move || app.h.store.volumes()).await?))
}

async fn model(State(app): State<Arc<App>>) -> Result<R, ApiError> {
    Ok(ok(blocking(move || app.h.learner.status()).await?))
}

#[derive(Deserialize)]
struct RollbackBody {
    version: String,
}

async fn rollback(State(app): State<Arc<App>>, Json(b): Json<RollbackBody>) -> Result<R, ApiError> {
    let r = blocking(move || app.h.learner.rollback(&b.version)).await?.map_err(|e| ApiError::conflict(format!("{e:#}")))?;
    Ok(ok(r))
}

// ---------------------------------------------------------------- backup

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportBody {
    /// Where to write the zip (from the desktop app's save dialog); none: Downloads, with a dated name.
    path: Option<String>,
    #[serde(default)]
    include_key: bool,
}

#[derive(Deserialize)]
struct BackupImportBody {
    path: String,
}

/// The user's Downloads folder (else their home), where an export goes when no path is given.
fn default_export_path() -> std::path::PathBuf {
    let home = bf_core::app_config::home_dir();
    let downloads = home.join("Downloads");
    (if downloads.is_dir() { downloads } else { home }).join(bf_store::backup::default_file_name())
}

fn absolute(p: &str) -> Result<std::path::PathBuf, ApiError> {
    let path = std::path::PathBuf::from(p.trim());
    if !path.is_absolute() {
        return Err(ApiError::bad("Give the full path of the file (for example C:\\Users\\you\\Downloads\\backup.zip)."));
    }
    Ok(path)
}

async fn backup_export(State(app): State<Arc<App>>, Json(b): Json<ExportBody>) -> Result<R, ApiError> {
    let path = match b.path.as_deref().filter(|p| !p.trim().is_empty()) {
        Some(p) => absolute(p)?,
        None => default_export_path(),
    };
    let r = blocking(move || app.h.store.export_backup(&path, b.include_key)).await?.map_err(|e| ApiError::bad(format!("{e:#}")))?;
    Ok(ok(r))
}

async fn backup_import(State(app): State<Arc<App>>, Json(b): Json<BackupImportBody>) -> Result<R, ApiError> {
    let path = absolute(&b.path)?;
    let r = blocking(move || app.h.store.stage_import(&path)).await?.map_err(|e| ApiError::bad(format!("{e:#}")))?;
    Ok(ok(r))
}

async fn backup_pending(State(app): State<Arc<App>>) -> Result<R, ApiError> {
    Ok(ok(blocking(move || app.h.store.pending_import()).await?))
}

async fn backup_cancel(State(app): State<Arc<App>>) -> Result<R, ApiError> {
    blocking(move || app.h.store.cancel_import()).await??;
    Ok(ok(serde_json::json!({ "cancelled": true })))
}

// ---------------------------------------------------------------- builds

#[derive(Deserialize)]
struct BuildQuery {
    champion: i64,
    role: Option<String>,
    /// The lane opponent's champion id.
    vs: Option<i64>,
    /// Comma-separated champion ids.
    allies: Option<String>,
    enemies: Option<String>,
}

fn parse_role(s: Option<&str>) -> Result<Option<Role>, ApiError> {
    match s.map(str::trim).filter(|s| !s.is_empty() && *s != "auto") {
        None => Ok(None),
        Some(r) => Role::from_riot(r).map(Some).ok_or_else(|| ApiError::bad(format!("Unknown role {r:?}"))),
    }
}

fn parse_ids(s: Option<&str>) -> Result<Vec<i64>, ApiError> {
    s.unwrap_or("").split(',').map(str::trim).filter(|x| !x.is_empty()).map(|x| x.parse().map_err(|_| ApiError::bad("Champion ids are numbers"))).collect()
}

/// GET /api/build?champion=103&role=MIDDLE&vs=134: any champion, browsed from the dashboard.
async fn build(State(app): State<Arc<App>>, Query(q): Query<BuildQuery>) -> Result<R, ApiError> {
    if q.champion <= 0 {
        return Err(ApiError::bad("Pick a champion"));
    }
    if let Some(d) = app.static_data() {
        if d.champion(q.champion).is_none() {
            return Err(ApiError::bad(format!("No champion {}", q.champion)));
        }
    }
    let role = parse_role(q.role.as_deref())?;
    let mut enemies: Vec<(i64, Option<Role>)> = parse_ids(q.enemies.as_deref())?.into_iter().map(|c| (c, None)).collect();
    if let Some(vs) = q.vs.filter(|v| *v > 0) {
        enemies.retain(|e| e.0 != vs);
        enemies.insert(0, (vs, role));
    }
    let allies = parse_ids(q.allies.as_deref())?.into_iter().map(|c| (c, None)).collect();
    let ctx = DraftContext { champion: q.champion, role, allies, enemies, patch: app.patch() };
    Ok(ok(app.recommend(ctx).await?))
}

async fn draft(State(app): State<Arc<App>>) -> R {
    ok(app.draft.lock().clone())
}

async fn live(State(app): State<Arc<App>>) -> R {
    ok(app.live.lock().clone())
}

#[derive(Deserialize)]
struct ImportBody {
    champion: i64,
    role: Option<String>,
}

async fn import(State(app): State<Arc<App>>, Json(b): Json<ImportBody>) -> Result<R, ApiError> {
    if b.champion <= 0 {
        return Err(ApiError::bad("Pick a champion"));
    }
    let role = parse_role(b.role.as_deref())?;
    let r = app.import(b.champion, role).await.map_err(|e| ApiError::conflict(format!("{e:#}")))?;
    Ok(ok(r))
}

// ---------------------------------------------------------------- settings and the key

/// The key as the dashboard sees it: masked, whether Riot refused it, and whether Irelia Companion's can be used.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyView {
    pub masked: String,
    pub set: bool,
    pub error: Option<String>,
    pub irelia_offer: KeyOffer,
    /// Where to get or renew a key (shown with a 401/403).
    pub portal: &'static str,
}

fn key_view(app: &App) -> KeyView {
    let masked = app.h.keys.key_masked();
    let offer = match app.h.keys.irelia_offer() {
        KeyOffer::Unreadable { reason } => KeyOffer::Unreadable { reason: scrub(&reason) },
        o => o,
    };
    KeyView { set: !masked.is_empty(), masked: scrub(&masked), error: app.h.keys.key_error().map(|e| scrub(&e)), irelia_offer: offer, portal: DEVELOPER_PORTAL }
}

fn settings_view(app: &App) -> Value {
    json!({ "settings": app.h.store.settings(), "key": key_view(app) })
}

async fn settings_get(State(app): State<Arc<App>>) -> Result<R, ApiError> {
    Ok(ok(blocking(move || settings_view(&app)).await?))
}

/// The dashboard's patch is checked here before the store sees it.
pub fn validate_patch(p: &SettingsPatch, known_champion: &dyn Fn(i64) -> bool) -> Result<SettingsPatch, ApiError> {
    let mut p = p.clone();
    if let Some(r) = &mut p.regions {
        let mut seen = vec![];
        r.retain(|x| if seen.contains(x) { false } else { seen.push(*x); true });
        if r.is_empty() {
            return Err(ApiError::bad("Choose at least one region."));
        }
    }
    if p.rank_floor == Some(RankBand::Mine) {
        return Err(ApiError::bad("The rank floor is Master+ or Diamond."));
    }
    if let Some(b) = p.disk_budget_bytes {
        if !(MIN_DISK_BUDGET..=MAX_DISK_BUDGET).contains(&b) {
            return Err(ApiError::bad("The disk budget is between 256 MB and 1 TB."));
        }
    }
    if let Some(t) = &mut p.tier_a {
        let mut seen = vec![];
        t.retain(|x| if seen.contains(x) { false } else { seen.push(*x); true });
        if t.len() > MAX_TIER_A {
            return Err(ApiError::bad(format!("At most {MAX_TIER_A} tier A champions.")));
        }
        if let Some(bad) = t.iter().find(|c| !known_champion(**c)) {
            return Err(ApiError::bad(format!("No champion {bad}")));
        }
    }
    Ok(p)
}

async fn settings_patch(State(app): State<Arc<App>>, body: axum::body::Bytes) -> Result<R, ApiError> {
    let patch: SettingsPatch = serde_json::from_slice(&body).map_err(|e| ApiError::bad(format!("Settings: {e}")))?;
    let data = app.static_data();
    let patch = validate_patch(&patch, &|c| c > 0 && data.as_ref().is_none_or(|d| d.champion(c).is_some()))?;
    let me = app.clone();
    let learning = patch.learning;
    blocking(move || me.h.store.update_settings(&patch)).await?.map_err(ApiError::from)?;
    if let Some(on) = learning {
        let me = app.clone();
        blocking(move || me.h.learner.set_learning(on)).await?;
    }
    Ok(ok(blocking(move || settings_view(&app)).await?))
}

#[derive(Deserialize)]
struct KeyBody {
    key: String,
}

/// A pasted key's shape: "RGAPI-" and the rest of a Riot key (letters, digits, dashes). The message never repeats it.
pub fn check_key_shape(key: &str) -> Result<(), ApiError> {
    let k = key.trim();
    let shape = k.starts_with("RGAPI-") && (16..=100).contains(&k.len()) && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if shape {
        Ok(())
    } else {
        Err(ApiError::bad("That doesn't look like a Riot API key (they start with RGAPI-)."))
    }
}

async fn key_paste(State(app): State<Arc<App>>, body: axum::body::Bytes) -> Result<R, ApiError> {
    // Not Json<KeyBody>: axum's own rejection text could quote the body.
    let b: KeyBody = serde_json::from_slice(&body).map_err(|_| ApiError::bad("Send { \"key\": \"RGAPI-…\" }."))?;
    let key = b.key.trim().to_string();
    check_key_shape(&key)?;
    let me = app.clone();
    blocking(move || me.h.keys.set_key(&key)).await?.map_err(ApiError::from)?;
    crate::bf_log!("[key] a new key was set");
    Ok(ok(blocking(move || key_view(&app)).await?))
}

async fn key_import(State(app): State<Arc<App>>) -> Result<R, ApiError> {
    let me = app.clone();
    blocking(move || me.h.keys.import_irelia_key()).await?.map_err(|e| ApiError::conflict(format!("{e:#}")))?;
    crate::bf_log!("[key] Irelia Companion's key is used");
    Ok(ok(blocking(move || key_view(&app)).await?))
}

// ---------------------------------------------------------------- development

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SimulateBody {
    start_at: Option<f64>,
    lane: Option<String>,
    champion: Option<String>,
}

/// Synthetic Live Client data for the overlay and the live panel, without League (a development aid).
async fn simulate_live(State(app): State<Arc<App>>, body: Option<Json<SimulateBody>>) -> R {
    let b = body.map(|b| b.0).unwrap_or(SimulateBody { start_at: None, lane: None, champion: None });
    let lane = if b.lane.as_deref() == Some("MIDDLE") { "MIDDLE" } else { "TOP" };
    let champion = b.champion.filter(|c| !c.is_empty()).unwrap_or_else(|| if lane == "MIDDLE" { "Ahri" } else { "Aatrox" }.into());
    app.simulate_live(b.start_at.unwrap_or(600.0).clamp(0.0, 3600.0), lane, champion);
    ok(json!({ "ok": true }))
}

async fn stop_simulation(State(app): State<Arc<App>>) -> R {
    ok(json!({ "ok": true, "stopped": app.stop_simulation() }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClientEvent {
    uri: String,
    event_type: Option<String>,
    data: Value,
}

/// A League client event as its websocket delivers one (development: champ select without League).
async fn client_event(State(app): State<Arc<App>>, Json(e): Json<ClientEvent>) -> R {
    let lcu = app.lcu.lock().clone();
    match lcu {
        Some(l) => {
            l.inject_event(&e.uri, e.event_type.as_deref().unwrap_or("Update"), &e.data);
            ok(json!({ "ok": true }))
        }
        None => ApiError::conflict("The League client connector isn't running.").into_response(),
    }
}

// ---------------------------------------------------------------- /ws

async fn ws(State(app): State<Arc<App>>, upgrade: WebSocketUpgrade) -> R {
    upgrade.on_upgrade(move |socket| socket_loop(app, socket))
}

/// The current state first (status, client, draft, game), then every event as it happens.
async fn socket_loop(app: Arc<App>, mut socket: WebSocket) {
    let mut rx = app.events.subscribe();
    let me = app.clone();
    let status = tokio::task::spawn_blocking(move || me.status()).await.ok();
    let first = [
        json!({ "type": "status", "status": status }),
        json!({ "type": "lcu", "state": app.lcu_view() }),
        json!({ "type": "draft", "draft": app.draft.lock().clone() }),
        json!({ "type": "live", "state": app.live.lock().clone().map(|l| json!(l)).unwrap_or(json!({ "active": false })) }),
    ];
    for ev in first {
        if socket.send(Message::Text(ev.to_string())).await.is_err() {
            return;
        }
    }
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Ok(text) => {
                    if socket.send(Message::Text(text)).await.is_err() {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                _ => {}
            },
        }
    }
}
