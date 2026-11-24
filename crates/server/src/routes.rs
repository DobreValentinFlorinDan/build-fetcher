//! Port of `server/index.ts`'s REST API and `/ws`: the same paths, bodies and answers as 0.22.0.

use crate::app::App;
use crate::jobs::{JobKind, CANCELLED};
use crate::lcu::importer::{import_build, ImportOptions, LcuApi, SendFuture};
use crate::model_worker::Request;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use irelia::app_config::{read_app_config, update_app_config};
use irelia::config::config;
use irelia::model::eval_history::{eval_history, HISTORY_LIMIT};
use irelia::model::personal::link_recommendations;
use irelia::model::service::{current_memory, current_mode, current_scoring, current_swaps, eval_dir};
use irelia::types::to_lane;
use irelia::util::now_ms;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// What the storage, backup, Slim, old-patches and conversion parts add (routes, status, and what they block).
pub trait Extra: Send + Sync {
    fn set_busy(&self, busy: bool);
    /// The data folder moving or compacting, old patches archiving, or Slim running: no evaluation starts.
    fn blocked_for_evaluation(&self) -> bool;
    /// Everything but storage waits (503).
    fn api_paused(&self) -> bool;
    fn slim_running(&self) -> bool;
    fn old_patches_busy(&self) -> bool;
    fn routes(&self) -> Router<Arc<App>>;
    fn ws_initial(&self) -> Vec<Value>;
    fn estimates(&self) -> serde_json::Map<String, Value>;
}

pub type R = Response;

/// JSON as JSON.stringify writes it (1 rather than 1.0, integer-like keys first), as 0.22.0 answered.
pub fn js_json(status: StatusCode, v: &Value) -> R {
    (status, [(axum::http::header::CONTENT_TYPE, "application/json; charset=utf-8")], irelia::js::stringify(v)).into_response()
}
fn err(status: StatusCode, msg: impl Into<String>) -> R {
    js_json(status, &json!({ "error": msg.into() }))
}
fn ok(v: Value) -> R {
    js_json(StatusCode::OK, &v)
}

/// A model-thread request as an HTTP answer: a timeout or error becomes a 503 / 500 with the reason.
fn answer(r: Result<Value, String>) -> R {
    match r {
        Ok(v) => ok(v),
        Err(m) => err(if m.contains("busy") || m.contains("restarting") { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::INTERNAL_SERVER_ERROR }, m),
    }
}

const SLIM_RUNNING: &str = "Slim stored data is running: try again once it is done.";

fn extra(app: &App) -> Option<Arc<dyn Extra>> {
    app.extra.lock().clone()
}

pub fn router(app: Arc<App>) -> Router {
    let mut r: Router<Arc<App>> = Router::new()
        .route("/api/health", get(health))
        .route("/api/meta", get(meta))
        .route("/api/matchup", get(get_matchup).put(put_matchup))
        .route("/api/recommendation", get(recommendation))
        .route("/api/recommendation/draft", get(recommendation_draft))
        .route("/api/model", get(model))
        .route("/api/model/mode", put(model_mode))
        .route("/api/model/memory", put(model_memory))
        .route("/api/model/scoring", put(model_scoring))
        .route("/api/model/swaps", put(model_swaps))
        .route("/api/model/evaluations", get(evaluations))
        .route("/api/model/evaluate", post(evaluate))
        .route("/api/jobs/cancel", post(cancel_job))
        .route("/api/personal", get(personal))
        .route("/api/personal/review/:match_id", get(review))
        .route("/api/lcu/import", post(lcu_import))
        .route("/api/state", get(state))
        .route("/api/scraper/start", post(scraper_start))
        .route("/api/scraper/stop", post(scraper_stop))
        .route("/api/db/stats", get(db_stats))
        .route("/api/db/audit", post(db_audit))
        .route("/api/db/backfill", post(db_backfill))
        .route("/api/estimates", get(estimates))
        .route("/api/system", get(system))
        .route("/api/dev/simulate-draft", post(simulate_draft))
        .route("/api/dev/simulate-live", post(simulate_live))
        .route("/api/dev/client-event", post(client_event))
        .route("/api/settings", get(settings_get).put(settings_put))
        .route("/api/gpu", get(gpu_get).put(gpu_put))
        .route("/api/gpu/test", post(gpu_test))
        .route("/api/gpu/log", axum::routing::delete(gpu_clear))
        .route("/ws", get(ws));
    if let Some(x) = extra(&app) {
        r = r.merge(x.routes());
    }
    let dist = dist_dir();
    let r = r.layer(axum::middleware::from_fn_with_state(app.clone(), paused_guard));
    let r = if dist.join("index.html").exists() {
        let index = dist.join("index.html");
        r.fallback_service(tower_http::services::ServeDir::new(&dist).fallback(tower_http::services::ServeFile::new(index)))
    } else {
        r
    };
    r.with_state(app)
}

/// The built dashboard: IRELIA_DIST, else ./dist.
pub fn dist_dir() -> std::path::PathBuf {
    std::env::var("IRELIA_DIST").map(std::path::PathBuf::from).unwrap_or_else(|_| irelia::util::resolve("dist"))
}

/// While the data folder is moved (or the database compacted), everything but storage waits.
async fn paused_guard(State(app): State<Arc<App>>, req: axum::extract::Request, next: axum::middleware::Next) -> R {
    let path = req.uri().path().to_string();
    if path.starts_with("/api") && !path.starts_with("/api/storage") && path != "/api/health" {
        if let Some(x) = extra(&app) {
            if x.api_paused() {
                return err(StatusCode::SERVICE_UNAVAILABLE, "The data folder is being moved or the database compacted. Try again in a moment.");
            }
        }
    }
    next.run(req).await
}

async fn health(State(app): State<Arc<App>>) -> R {
    ok(json!({ "ok": true, "patch": app.data().patch, "dataDir": config().data_dir.to_string_lossy() }))
}

async fn meta(State(app): State<Arc<App>>) -> R {
    let data = app.data();
    let mut champions: Vec<Value> = data.champions.values().map(|c| json!({ "id": c.id, "key": c.key, "name": c.name, "tags": c.tags })).collect();
    champions.sort_by(|a, b| locale_compare(a["name"].as_str().unwrap_or(""), b["name"].as_str().unwrap_or("")));
    let mut summoners = serde_json::Map::new();
    for (id, s) in &data.summoners {
        summoners.insert(id.to_string(), serde_json::to_value(s).unwrap_or(Value::Null));
    }
    ok(json!({
        "version": data.version, "patch": data.patch, "previousPatch": data.previous_patch, "champions": champions,
        "items": app.plan_items_json(), "runes": serde_json::to_value(&data.runes).unwrap_or(Value::Null),
        "summoners": summoners, "spells": serde_json::to_value(&data.spells).unwrap_or(Value::Null),
    }))
}

/// String#localeCompare for names (ICU root order, close enough): punctuation before letters, letters by base letter
/// whatever their case, then lower case first.
fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    let key = |s: &str| -> Vec<(u8, char)> { s.chars().map(|c| if c.is_alphanumeric() { (1, c.to_lowercase().next().unwrap_or(c)) } else { (0, c) }).collect() };
    key(a).cmp(&key(b)).then_with(|| b.cmp(a))
}

async fn get_matchup(State(app): State<Arc<App>>) -> R {
    let i = app.inner.lock();
    ok(json!({ "matchup": i.matchup.to_json(), "build": i.build }))
}

fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        other => irelia::js::stringify(other),
    }
}

async fn put_matchup(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> R {
    let b = body.map(|b| b.0).unwrap_or(json!({}));
    if let Some(l) = b.get("lane") {
        let Some(lane) = to_lane(Some(&js_string(l))) else { return err(StatusCode::BAD_REQUEST, format!("Unknown lane \"{}\": use TOP or MIDDLE", js_string(l))) };
        app.session.lock().set_lane(lane);
    }
    if b.get("follow") == Some(&json!(true)) {
        app.session.lock().set_opponent(None);
    } else if let Some(o) = b.get("opponentId") {
        let id = if o.is_null() { None } else { Some(irelia::js::to_number(o)) };
        if let Some(i) = id {
            if i.fract() != 0.0 || !app.data().champions.contains_key(&(i as i64)) {
                return err(StatusCode::BAD_REQUEST, format!("Unknown champion {}", js_string(o)));
            }
        }
        app.session.lock().set_opponent(Some(id.map(|i| i as i64)));
    }
    if let Some(r) = b.get("regions") {
        let regions = r.as_array().map(|a| a.iter().map(|x| js_string(x).to_uppercase()).collect());
        app.session.lock().set_regions(regions);
    }
    app.refresh_matchup();
    let m = app.inner.lock().matchup.to_json();
    ok(json!({ "matchup": m }))
}

fn q_num(v: Option<&String>) -> Option<f64> {
    v.filter(|s| !s.is_empty()).map(|s| irelia::js::to_number(&Value::String(s.clone())))
}

async fn recommendation(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> R {
    let enemy = q_num(q.get("enemy")).filter(|x| *x != 0.0 && x.fract() == 0.0).map(|x| x as i64);
    let lane = q.get("lane").filter(|s| !s.is_empty()).map(|l| to_lane(Some(l)));
    if let Some(None) = lane {
        return err(StatusCode::BAD_REQUEST, format!("Unknown lane \"{}\": use TOP or MIDDLE", q["lane"]));
    }
    answer(app.models.request(Request::Recommendation { enemy, lane: lane.flatten() }, 30_000).await)
}

async fn recommendation_draft(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> R {
    let lane_s = q.get("lane").cloned().unwrap_or_else(|| "TOP".into());
    let Some(lane) = to_lane(Some(&lane_s)) else { return err(StatusCode::BAD_REQUEST, format!("Unknown lane \"{lane_s}\": use TOP or MIDDLE")) };
    let data = app.data();
    let enemy = q_num(q.get("enemy")).filter(|x| x.fract() == 0.0 && data.champions.contains_key(&(*x as i64))).map(|x| x as i64);
    let ids = |k: &str| -> Vec<i64> {
        q.get(k)
            .filter(|s| !s.is_empty())
            .map(|s| s.split(',').map(|x| irelia::js::to_number(&Value::String(x.into()))).filter(|x| x.fract() == 0.0 && data.champions.contains_key(&(*x as i64))).map(|x| x as i64).collect())
            .unwrap_or_default()
    };
    let regions = q.get("regions").filter(|s| !s.is_empty()).map(|s| s.split(',').map(|r| r.trim().to_uppercase()).filter(|r| !r.is_empty()).collect());
    answer(app.models.request(Request::Draft { lane, opponent: enemy, enemies: ids("enemies"), allies: ids("allies"), regions }, 30_000).await)
}

async fn model(State(app): State<Arc<App>>) -> R {
    ok(app.models.status())
}

async fn model_mode(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> R {
    let mode = body.map(|b| b.0["mode"].clone()).unwrap_or(Value::Null);
    if !(mode.is_null() || mode == "draft" || mode == "baseline") {
        return err(StatusCode::BAD_REQUEST, "mode must be \"draft\", \"baseline\" or null");
    }
    update_app_config(vec![("buildMode", if mode.is_null() { None } else { Some(mode) })]);
    app.models.evaluated();
    app.request_build(0);
    ok(json!({ "mode": current_mode() }))
}

fn switches_saved(app: &Arc<App>) {
    let status = app.models.options_changed();
    if !status["info"].is_null() {
        app.broadcast(json!({ "type": "model", "status": status }));
    }
    if let Some(a) = app.auto_eval.lock().clone() {
        app.rt.spawn_blocking(move || a.check());
    }
}

async fn model_memory(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> R {
    let memory = body.map(|b| b.0["memory"].clone()).unwrap_or(Value::Null);
    if !(memory.is_null() || memory == "on" || memory == "off") {
        return err(StatusCode::BAD_REQUEST, "memory must be \"on\", \"off\" or null");
    }
    if !memory.is_null() && read_app_config().get("patchMemory").is_none() {
        update_app_config(vec![
            ("pregameScoring", Some(json!(if current_scoring().is_some() { "on" } else { "off" }))),
            ("lateSwaps", Some(json!(if current_swaps() { "on" } else { "off" }))),
        ]);
    }
    update_app_config(vec![("patchMemory", if memory.is_null() { None } else { Some(memory.clone()) })]);
    switches_saved(&app);
    ok(json!({ "memory": memory }))
}

async fn model_scoring(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> R {
    let scoring = body.map(|b| b.0["scoring"].clone()).unwrap_or(Value::Null);
    if scoring != "on" && scoring != "off" {
        return err(StatusCode::BAD_REQUEST, "scoring must be \"on\" or \"off\"");
    }
    let chosen = read_app_config().get("patchMemory").cloned();
    let following = chosen.is_none();
    let memory = chosen.unwrap_or_else(|| json!(if current_memory().is_some() { "on" } else { "off" }));
    let mut patch = vec![("pregameScoring", Some(scoring.clone())), ("patchMemory", Some(memory.clone()))];
    if following {
        patch.push(("lateSwaps", Some(json!(if current_swaps() { "on" } else { "off" }))));
    }
    update_app_config(patch);
    switches_saved(&app);
    ok(json!({ "scoring": scoring, "memory": memory }))
}

async fn model_swaps(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> R {
    let swaps = body.map(|b| b.0["swaps"].clone()).unwrap_or(Value::Null);
    if swaps != "on" && swaps != "off" {
        return err(StatusCode::BAD_REQUEST, "swaps must be \"on\" or \"off\"");
    }
    let chosen = read_app_config().get("patchMemory").cloned();
    let following = chosen.is_none();
    let memory = chosen.unwrap_or_else(|| json!(if current_memory().is_some() { "on" } else { "off" }));
    let mut patch = vec![("lateSwaps", Some(swaps.clone())), ("patchMemory", Some(memory.clone()))];
    if following {
        patch.push(("pregameScoring", Some(json!(if current_scoring().is_some() { "on" } else { "off" }))));
    }
    update_app_config(patch);
    switches_saved(&app);
    ok(json!({ "swaps": swaps, "memory": memory }))
}

async fn evaluations() -> R {
    ok(Value::Array(eval_history(&eval_dir(), HISTORY_LIMIT)))
}

async fn evaluate(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> R {
    if extra(&app).map(|x| x.slim_running()).unwrap_or(false) {
        return err(StatusCode::CONFLICT, SLIM_RUNNING);
    }
    let b = body.map(|b| b.0).unwrap_or(json!({}));
    let quick = b["quick"] == true;
    let asked = if b["path"].is_null() { "auto".to_string() } else { js_string(&b["path"]) };
    if asked != "auto" && asked != "full" {
        return err(StatusCode::BAD_REQUEST, "path must be \"auto\" or \"full\"");
    }
    let plan = if asked == "auto" && !quick { app.auto_eval.lock().clone().and_then(|a| a.plan()) } else { None };
    if plan.as_ref().map(|p| p.path == "none").unwrap_or(false) && app.jobs.running_job("evaluate").is_none() {
        return err(StatusCode::CONFLICT, "Up to date: no new games since the last evaluation. Full evaluation evaluates every game again.");
    }
    let path: &'static str = if quick { "full" } else if asked == "full" { "full" } else { "auto" };
    let planned: &'static str = if plan.as_ref().map(|p| p.path == "update").unwrap_or(false) { "update" } else { "full" };
    match app.evaluate(JobKind::Evaluate { quick, auto: None, path, planned }).await {
        Ok(mut r) => {
            r["mode"] = json!(current_mode());
            ok(r)
        }
        Err(m) => err(if m == CANCELLED { StatusCode::CONFLICT } else { StatusCode::INTERNAL_SERVER_ERROR }, m),
    }
}

async fn cancel_job(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> R {
    let job = body.map(|b| b.0["job"].clone()).unwrap_or(Value::Null);
    let Some(name) = job.as_str().filter(|j| *j == "evaluate" || *j == "audit") else { return err(StatusCode::BAD_REQUEST, "job must be \"evaluate\" or \"audit\"") };
    app.jobs.cancel(name, false).await;
    ok(app.jobs_status())
}

async fn personal(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> R {
    let _ = link_recommendations(&app.db.lock(), now_ms());
    let lane = q.get("lane").filter(|s| !s.is_empty()).and_then(|l| to_lane(Some(l)));
    let r = app.models.request(Request::Personal { lane }, 30_000).await;
    let games: Vec<Value> = app
        .db
        .lock()
        .own_games(20)
        .unwrap_or_default()
        .iter()
        .map(|g| json!({ "matchId": g.match_id, "created": g.created_at, "win": g.win == 1, "lane": to_lane(Some(&g.position)), "opponentId": g.opponent_champion_id }))
        .collect();
    answer(r.map(|report| json!({ "report": report, "games": games })))
}

async fn review(State(app): State<Arc<App>>, AxPath(match_id): AxPath<String>) -> R {
    let _ = link_recommendations(&app.db.lock(), now_ms());
    match app.models.request(Request::Review { match_id }, 30_000).await {
        Ok(v) if v.is_null() => err(StatusCode::NOT_FOUND, "Not one of your stored games"),
        Ok(v) => ok(v),
        Err(m) => err(StatusCode::SERVICE_UNAVAILABLE, m),
    }
}

fn is_local_origin(headers: &HeaderMap) -> bool {
    let Some(o) = headers.get("origin").and_then(|v| v.to_str().ok()) else { return true };
    match reqwest::Url::parse(o) {
        Ok(u) => matches!(u.host_str(), Some("localhost") | Some("127.0.0.1")) || u.scheme() == "tauri" || u.host_str() == Some("tauri.localhost"),
        Err(_) => false,
    }
}

struct LcuSend(Arc<crate::lcu::connector::LcuConnector>);
impl LcuApi for LcuSend {
    fn send<'a>(&'a self, method: &'a str, path: &'a str, body: Option<&'a Value>) -> SendFuture<'a> {
        Box::pin(self.0.send(method, path, body))
    }
}

async fn lcu_import(State(app): State<Arc<App>>, headers: HeaderMap, body: Option<Json<Value>>) -> R {
    if !is_local_origin(&headers) {
        return err(StatusCode::FORBIDDEN, "Only the app itself can write to the League client.");
    }
    let lcu = app.lcu.state();
    let live_active = app.inner.lock().live_state.as_ref().map(|s| s["active"] == true).unwrap_or(false);
    if lcu["status"] != "connected" || lcu["phase"] != "ChampSelect" || lcu["champSelect"].is_null() || live_active {
        return err(StatusCode::CONFLICT, "Builds can only be saved to the League client during champ select.");
    }
    if lcu["champSelect"]["isIrelia"] != true {
        return err(StatusCode::CONFLICT, "Pick Irelia first: the rune page would become your current page.");
    }
    let b = body.map(|b| b.0).unwrap_or(json!({}));
    let mode = if b["mode"] == "draft" { "draft" } else { "baseline" };
    let (build, matchup) = {
        let i = app.inner.lock();
        (i.build.clone(), i.matchup.clone())
    };
    let Some(build) = build.filter(|bd| bd["key"] == b["key"] && bd["key"].as_str() == Some(matchup.key.as_str())) else {
        return err(StatusCode::CONFLICT, "The build just changed. Check it and press again.");
    };
    let r = &build["response"];
    let shown = if mode == "draft" && !r["draft"].is_null() { &r["draft"]["build"] } else { &r["baseline"] };
    let Ok(rec) = serde_json::from_value::<irelia::types::BuildRecommendation>(shown.clone()) else { return err(StatusCode::INTERNAL_SERVER_ERROR, "The build could not be read") };
    let owned = read_app_config().get("lcuImport").cloned().unwrap_or(json!({}));
    let o = ImportOptions {
        lane: matchup.lane,
        opponent_name: matchup.opponent_id.and_then(|id| app.data().champion(id).map(|c| c.name.clone())),
        rune_page_id: owned["runePageId"].as_i64(),
        item_set_uid: owned["itemSetUid"].as_str().map(String::from),
        best: r["facts"]["best"].as_object().map(|_| r["facts"]["best"].clone()),
    };
    let result = import_build(&LcuSend(app.lcu.clone()), &rec, &o).await;
    update_app_config(vec![("lcuImport", Some(result["owned"].clone()))]);
    ok(result)
}

async fn state(State(app): State<Arc<App>>) -> R {
    let (live, matchup, build) = {
        let i = app.inner.lock();
        (i.live_state.clone(), i.matchup.to_json(), i.build.clone())
    };
    ok(json!({
        "lcu": app.effective_lcu(), "live": live, "scraper": app.scraper.status.lock().clone(), "matchup": matchup, "build": build,
        "model": app.models.status(), "jobs": app.jobs_status(),
    }))
}

async fn scraper_start(State(app): State<Arc<App>>) -> R {
    app.scraper.start();
    ok(json!({ "ok": true }))
}
async fn scraper_stop(State(app): State<Arc<App>>) -> R {
    app.scraper.stop();
    ok(json!({ "ok": true }))
}

async fn db_stats(State(app): State<Arc<App>>) -> R {
    answer(app.models.request(Request::Stats, 30_000).await)
}

async fn db_audit(State(app): State<Arc<App>>) -> R {
    if extra(&app).map(|x| x.slim_running()).unwrap_or(false) {
        return err(StatusCode::CONFLICT, SLIM_RUNNING);
    }
    let size = app.db.lock().counts(&app.data().patch).map(|c| c.matches as f64).ok();
    match app.jobs.run(JobKind::Audit, size).await {
        Ok(v) => ok(v),
        Err(m) => err(StatusCode::INTERNAL_SERVER_ERROR, m),
    }
}

async fn db_backfill(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> R {
    let force = body.map(|b| b.0["force"] == true).unwrap_or(false);
    app.scraper.backfill(force);
    ok(json!({ "ok": true }))
}

async fn estimates(State(app): State<Arc<App>>) -> R {
    let data = app.data();
    let (counts, games, stale) = {
        let db = app.db.lock();
        let c = db.counts(&data.patch).map(|c| c.matches as f64).unwrap_or(0.0);
        let g = irelia::model::run_eval::eval_size(&db, &data);
        let keep = irelia::model::run_eval::model_patches(&db, &data, irelia::model::weights::MEMORY_PATCHES).unwrap_or_default();
        (c, g, db.stale_count(&keep) as f64)
    };
    let threads = crate::system::planned_threads(games);
    let mut e = serde_json::Map::new();
    e.insert("evaluate".into(), json!(irelia::progress::expected_ms("evaluate", games, Some(threads))));
    e.insert("evaluate-update".into(), json!(irelia::progress::expected_ms("evaluate-update", games, Some(threads))));
    e.insert("audit".into(), json!(irelia::progress::expected_ms("audit", counts, None)));
    e.insert("purge".into(), json!(irelia::progress::expected_ms("purge", stale, None)));
    if let Some(x) = extra(&app) {
        for (k, v) in x.estimates() {
            e.insert(k, v);
        }
    }
    ok(Value::Object(e))
}

async fn system(State(app): State<Arc<App>>) -> R {
    let games = irelia::model::run_eval::eval_size(&app.db.lock(), &app.data());
    ok(crate::system::system_info(games))
}

async fn simulate_draft(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> R {
    let b = body.map(|b| b.0).unwrap_or(json!({}));
    let data = app.data();
    let enemies: Vec<i64> = b["enemies"].as_array().map(|a| a.iter().map(irelia::js::to_number).filter(|x| x.fract() == 0.0 && data.champions.contains_key(&(*x as i64))).map(|x| x as i64).collect()).unwrap_or_default();
    let lane = if b.get("lane").is_none() { "TOP".to_string() } else { to_lane(Some(&js_string(&b["lane"]))).map(String::from).unwrap_or_else(|| js_string(&b["lane"]).to_uppercase()) };
    ok(app.simulate_draft(enemies, lane))
}

/// A League client event, handled exactly as one from the client's websocket: { uri, eventType, data }.
async fn client_event(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> R {
    let b = body.map(|b| b.0).unwrap_or(json!({}));
    let (uri, kind) = (b["uri"].as_str().unwrap_or("").to_string(), b["eventType"].as_str().unwrap_or("Update").to_string());
    let a = app.clone();
    // Off the request's task, as the client's events are: a handler that never returns must not take the server along.
    let done = tokio::task::spawn_blocking(move || a.lcu.inject_event(&uri, &kind, &b["data"]));
    match tokio::time::timeout(std::time::Duration::from_secs(10), done).await {
        Ok(_) => ok(json!({ "handled": true })),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "The client event was not handled within 10 s".to_string()),
    }
}

async fn simulate_live(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> R {
    let b = body.map(|b| b.0).unwrap_or(json!({}));
    app.stop_simulation();
    if b["stop"] == true || (b.get("stop").map(|s| !s.is_null() && s != &json!(false) && s != &json!(0) && s != &json!("")).unwrap_or(false)) {
        app.on_live_ended();
        return ok(json!({ "running": false }));
    }
    let lane = if b.get("lane").is_none() { Some("TOP") } else { to_lane(Some(&js_string(&b["lane"]))) };
    let Some(lane) = lane else { return err(StatusCode::BAD_REQUEST, format!("Unknown lane \"{}\": use TOP or MIDDLE", js_string(&b["lane"]))) };
    let start = irelia::js::to_number(&b["startAt"]);
    app.simulate_live(if start.is_finite() { start } else { 0.0 }, lane, b["positions"] != false);
    ok(json!({ "running": true }))
}

// ---------------------------------------------------------------- settings (routes/settings.ts)

fn mask(k: &str) -> Value {
    if k.is_empty() {
        return Value::Null;
    }
    let chars: Vec<char> = k.chars().collect();
    let head: String = chars.iter().take(10).collect();
    let tail: String = chars.iter().skip(chars.len().saturating_sub(4)).collect();
    json!(format!("{head}…{tail}"))
}

fn settings_state() -> Value {
    let app = read_app_config();
    let key = config().riot_api_key;
    let saved = app.get("riotApiKey").and_then(|v| v.as_str()).map(|s| !s.is_empty()).unwrap_or(false);
    json!({
        "riotApiKey": mask(&key),
        "keySource": if saved { "settings" } else if !key.is_empty() { "env" } else { "none" },
        "keySavedAt": if saved { app.get("riotApiKeySavedAt").cloned().unwrap_or(Value::Null) } else { Value::Null },
        "myPlatform": irelia::riot::routing::to_platform(app.get("myPlatform").and_then(|v| v.as_str())),
        "platforms": irelia::riot::routing::PLATFORMS.iter().map(|p| json!({ "id": p.0, "label": p.2 })).collect::<Vec<_>>(),
        "pauseFetchingInGame": app.get("pauseFetchingInGame") != Some(&json!(false)),
    })
}

fn is_key(k: &str) -> bool {
    let Some(rest) = k.strip_prefix("RGAPI-").or_else(|| k.strip_prefix("rgapi-")) else { return false };
    let parts: Vec<&str> = rest.split('-').collect();
    let lens = [8, 4, 4, 4, 12];
    parts.len() == 5 && parts.iter().zip(lens).all(|(p, n)| p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

async fn settings_get() -> R {
    ok(settings_state())
}

async fn settings_put(State(app): State<Arc<App>>, body: Option<Json<Value>>) -> R {
    let b = body.map(|b| b.0).unwrap_or(json!({}));
    if let Some(p) = b["pauseFetchingInGame"].as_bool() {
        update_app_config(vec![("pauseFetchingInGame", Some(json!(p)))]);
        app.scraper.set_pause_in_game(p);
        return ok(settings_state());
    }
    if let Some(m) = b.get("myPlatform") {
        let p = if m.is_null() { None } else { irelia::riot::routing::to_platform(Some(&js_string(m))) };
        if !m.is_null() && p.is_none() {
            return err(StatusCode::BAD_REQUEST, format!("Unknown server \"{}\"", js_string(m)));
        }
        update_app_config(vec![("myPlatform", p.map(Value::String))]);
        app.scraper.retry_me();
        return ok(settings_state());
    }
    let Some(key) = b["riotApiKey"].as_str() else { return err(StatusCode::BAD_REQUEST, "riotApiKey (string) is required") };
    let key = key.trim();
    if !key.is_empty() && !is_key(key) {
        return err(StatusCode::BAD_REQUEST, "That does not look like a Riot API key (RGAPI-xxxxxxxx-...).");
    }
    update_app_config(vec![
        ("riotApiKey", if key.is_empty() { None } else { Some(json!(key)) }),
        ("riotApiKeySavedAt", if key.is_empty() { None } else { Some(json!(now_ms())) }),
    ]);
    let effective = if key.is_empty() { std::env::var("RIOT_API_KEY").unwrap_or_default() } else { key.to_string() };
    app.scraper.set_api_key(&effective);
    app.scraper.retry_me();
    ok(settings_state())
}

// ---------------------------------------------------------------- GPU (routes/gpu.ts)

use irelia::gpu::log::{clear_gpu_log, gpu_log, gpu_log_file, read_gpu_log};

fn gpu_state() -> Value {
    json!({
        "useGpu": read_app_config().get("useGpu") == Some(&json!(true)),
        "testing": irelia::gpu::accelerate::gpu_testing(),
        "log": read_gpu_log(400),
        "logFile": gpu_log_file().to_string_lossy(),
    })
}

async fn gpu_get() -> R {
    ok(gpu_state())
}

async fn gpu_put(body: Option<Json<Value>>) -> R {
    let Some(on) = body.and_then(|b| b.0["useGpu"].as_bool()) else { return err(StatusCode::BAD_REQUEST, "useGpu (boolean) is required") };
    update_app_config(vec![("useGpu", if on { Some(json!(true)) } else { None })]);
    gpu_log(if on { "Use GPU on: the next evaluation uses the GPU." } else { "Use GPU off: Evaluate builds runs on the processor." });
    ok(gpu_state())
}

/// Test GPU: waits while Evaluate builds runs (they would share the card's memory) and in champ select or a game (the
/// card is the game's).
async fn gpu_test(State(app): State<Arc<App>>) -> R {
    if app.jobs.running_job("evaluate").is_some() {
        return err(StatusCode::CONFLICT, "Evaluate builds is running: test the GPU once it ends.");
    }
    if app.jobs.busy() {
        return err(StatusCode::CONFLICT, "Champ select or a game is on: test the GPU after it.");
    }
    let passed = tokio::task::spawn_blocking(irelia::gpu::accelerate::test_gpu).await.unwrap_or(false);
    let mut s = gpu_state();
    s["ok"] = json!(passed);
    ok(s)
}

async fn gpu_clear() -> R {
    clear_gpu_log();
    ok(gpu_state())
}

// ---------------------------------------------------------------- /ws

async fn ws(State(app): State<Arc<App>>, upgrade: WebSocketUpgrade) -> R {
    upgrade.on_upgrade(move |socket| socket_loop(app, socket))
}

async fn socket_loop(app: Arc<App>, mut socket: WebSocket) {
    let mut rx = app.events.subscribe();
    let mut first: Vec<Value> = vec![json!({ "type": "lcu", "state": app.effective_lcu() })];
    if let Some(s) = app.scraper.status.lock().clone() {
        first.push(json!({ "type": "scraper", "status": s }));
    }
    if let Some(s) = app.scraper.rate.lock().clone() {
        first.push(json!({ "type": "ratelimit", "snapshot": s }));
    }
    let (live, matchup, build) = {
        let i = app.inner.lock();
        (i.live_state.clone(), i.matchup.to_json(), i.build.clone())
    };
    if let Some(l) = live {
        first.push(json!({ "type": "live", "state": l }));
    }
    first.push(json!({ "type": "matchup", "matchup": matchup }));
    let extra_first = extra(&app).map(|x| x.ws_initial()).unwrap_or_default();
    let (conversion, slim, backup): (Vec<Value>, Vec<Value>, Vec<Value>) = {
        let mut c = vec![];
        let mut s = vec![];
        let mut b = vec![];
        for e in extra_first {
            match e["type"].as_str() {
                Some("conversion") => c.push(e),
                Some("slim") => s.push(e),
                _ => b.push(e),
            }
        }
        (c, s, b)
    };
    first.extend(conversion);
    first.extend(slim);
    if let Some(b) = build {
        first.push(json!({ "type": "build", "build": b }));
    }
    first.push(json!({ "type": "model", "status": app.models.status() }));
    first.push(json!({ "type": "jobs", "status": app.jobs_status() }));
    first.extend(backup);
    for ev in first {
        if socket.send(Message::Text(irelia::js::stringify(&ev))).await.is_err() {
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
