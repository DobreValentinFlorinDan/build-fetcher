//! The server end to end, with the fakes: every route's JSON against the contract, the key routes (the key never in
//! a body or a log line), the import through the route, champ select and the live game through the same paths a real
//! client's events take, and the activity reaching the store and the learner.

use crate::app::App;
use crate::fakes::{FakeKeys, FakeLearner, FakeStore, StubBuildSource};
use crate::handles::{AppSettings, Handles, KeyOffer};
use crate::lcu::connector::{LcuConnector, LcuEvent};
use crate::lcu::importer::LcuApi;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use bf_core::build::*;
use http_body_util::BodyExt;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const KEY: &str = "RGAPI-00000000-test";
const IRELIA_KEY: &str = "RGAPI-00000000-irelia";

struct T {
    app: Arc<App>,
    store: Arc<FakeStore>,
    learner: Arc<FakeLearner>,
    keys: Arc<FakeKeys>,
}

fn harness(offer: KeyOffer) -> T {
    let slot = crate::static_data::empty_slot();
    *slot.write() = Some(bf_core::testkit::load_test_static_data());
    let store = Arc::new(FakeStore::default());
    let learner = Arc::new(FakeLearner::default());
    let keys = Arc::new(FakeKeys::new(offer));
    *keys.irelia_key.lock() = Some(IRELIA_KEY.into());
    let h = Handles { builds: Arc::new(StubBuildSource { data: slot.clone() }), store: store.clone(), learner: learner.clone(), keys: keys.clone() };
    T { app: App::new(h, slot, None), store, learner, keys }
}

async fn call(app: &Arc<App>, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, String) {
    call_raw(app, method, uri, body.map(|b| b.to_string()), &[]).await
}

async fn call_raw(app: &Arc<App>, method: Method, uri: &str, body: Option<String>, headers: &[(&str, &str)]) -> (StatusCode, String) {
    let mut req = Request::builder().method(method).uri(uri).header("host", "127.0.0.1:5184");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let req = match body {
        Some(b) => req.header("content-type", "application/json").body(Body::from(b)).unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let res = crate::routes::router(app.clone(), None).oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn get(app: &Arc<App>, uri: &str) -> Value {
    let (s, b) = call(app, Method::GET, uri, None).await;
    assert_eq!(s, StatusCode::OK, "{uri}: {b}");
    serde_json::from_str(&b).unwrap()
}

fn keys_of(v: &Value) -> Vec<&str> {
    v.as_object().unwrap().keys().map(|k| k.as_str()).collect()
}

/// Log lines of every test in this process (the hook is global), for the "never in a log line" checks.
fn captured_logs() -> Arc<Mutex<Vec<String>>> {
    static LINES: std::sync::OnceLock<Arc<Mutex<Vec<String>>>> = std::sync::OnceLock::new();
    LINES
        .get_or_init(|| {
            let lines = Arc::new(Mutex::new(vec![]));
            let l2 = lines.clone();
            crate::logging::set_hook(Some(Arc::new(move |s: &str| l2.lock().push(s.to_string()))));
            lines
        })
        .clone()
}

// ---------------------------------------------------------------- contract shapes

#[tokio::test]
async fn status_routes_match_the_contract() {
    let t = harness(KeyOffer::Missing);
    let s = get(&t.app, "/api/status").await;
    assert_eq!(keys_of(&s), vec!["version", "activity", "lcu", "fetch", "model", "firstRunComplete", "keySet", "staticReady"]);
    assert_eq!(s["activity"], "idle");
    assert_eq!(s["lcu"], json!({ "status": "searching", "phase": "None" }));
    assert_eq!(s["version"], "1.0.0");
    // Each part deserializes into the contract's own type: same field names, same casing.
    let fetch: FetchStatus = serde_json::from_value(s["fetch"].clone()).unwrap();
    assert_eq!(fetch, t.store.fetch_status_for_test());
    let model: ModelStatus = serde_json::from_value(s["model"].clone()).unwrap();
    assert_eq!(model.checkpoints.len(), 3);
    assert_eq!(keys_of(&s["fetch"]["budgets"][0]), vec!["scope", "limits", "used", "ours", "limit", "windowS", "shared"]);
    assert_eq!(keys_of(&s["model"]["checkpoints"][0]["metrics"]), vec!["games", "logLoss", "brier", "calibration"]);

    let f: FetchStatus = serde_json::from_value(get(&t.app, "/api/fetch").await).unwrap();
    assert_eq!(f.budgets.iter().filter(|b| b.shared).count(), 1);
    let st = get(&t.app, "/api/storage").await;
    assert_eq!(keys_of(&st), vec!["budgetBytes", "usedBytes", "parts", "patches", "lastCleanupFreedBytes"]);
    let st: StorageStatus = serde_json::from_value(st).unwrap();
    assert_eq!(st.used_bytes, st.parts.iter().map(|p| p.1).sum::<u64>());
    let ages: Vec<PairVolume> = serde_json::from_value(get(&t.app, "/api/data-age").await).unwrap();
    assert!(!ages.is_empty());
    let m: ModelStatus = serde_json::from_value(get(&t.app, "/api/model").await).unwrap();
    assert_eq!(m.checkpoints.iter().filter(|c| c.active).count(), 1);
    assert_eq!(get(&t.app, "/api/health").await, json!({ "ok": true, "version": "1.0.0" }));
    assert_eq!(get(&t.app, "/api/draft").await, Value::Null);
    assert_eq!(get(&t.app, "/api/live").await, Value::Null);
}

impl FakeStore {
    fn fetch_status_for_test(&self) -> FetchStatus {
        crate::handles::StoreHandle::fetch_status(self)
    }
}

#[tokio::test]
async fn build_for_any_champion_matches_the_contract() {
    let t = harness(KeyOffer::Missing);
    let data = bf_core::testkit::load_test_static_data();
    for (champion, role) in [(103, Some("MIDDLE")), (12, None), (950, Some("jungle"))] {
        let uri = match role {
            Some(r) => format!("/api/build?champion={champion}&role={r}&vs=134&enemies=134,238"),
            None => format!("/api/build?champion={champion}"),
        };
        let v = get(&t.app, &uri).await;
        assert_eq!(keys_of(&v), vec!["champion", "role", "patch", "tier", "laneOpponent", "enemyProfile", "games", "sections", "model", "notes"]);
        assert_eq!(keys_of(&v["sections"][0]["options"][0]), vec!["option", "label", "games", "wins", "weightedGames", "winRate", "wpa", "predictedWin", "basis", "thin"]);
        let rec: BuildRecommendation = serde_json::from_value(v).unwrap();
        assert_eq!(rec.champion, champion);
        assert_eq!(rec.patch, Patch::parse(&data.patch));
        if role.is_some() {
            assert_eq!(rec.lane_opponent, Some(134));
        }
        assert!(rec.sections.iter().all(|s| s.options.len() >= 2), "several options in each section");
    }
    let (s, b) = call(&t.app, Method::GET, "/api/build?champion=99999", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    let (s, _) = call(&t.app, Method::GET, "/api/build?champion=103&role=CARRY", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let st = get(&t.app, "/api/static").await;
    assert_eq!(keys_of(&st), vec!["ready", "version", "patch", "champions", "items", "runes", "summoners"]);
    assert_eq!(st["champions"].as_array().unwrap().len(), 173);
    assert_eq!(st["items"]["3078"]["name"], "Trinity Force");
    assert_eq!(st["runes"]["8010"]["name"], "Conqueror");
}

#[tokio::test]
async fn settings_round_trip_and_validation() {
    let t = harness(KeyOffer::Missing);
    let v = get(&t.app, "/api/settings").await;
    assert_eq!(keys_of(&v), vec!["settings", "key"]);
    let s: AppSettings = serde_json::from_value(v["settings"].clone()).unwrap();
    assert_eq!(s, AppSettings::default());
    assert_eq!(keys_of(&v["key"]), vec!["masked", "set", "error", "ireliaOffer", "portal"]);

    let patch = json!({ "regions": ["EUW1", "EUW1", "KR"], "rankFloor": "diamond", "diskBudgetBytes": 4_294_967_296u64, "tierA": [103, 103, 39], "gpu": false, "learning": false });
    let (st, b) = call(&t.app, Method::PATCH, "/api/settings", Some(patch)).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let s: AppSettings = serde_json::from_value(serde_json::from_str::<Value>(&b).unwrap()["settings"].clone()).unwrap();
    assert_eq!(s.regions, vec![Platform::Euw1, Platform::Kr], "duplicates dropped");
    assert_eq!(s.tier_a, vec![103, 39]);
    assert_eq!((s.rank_floor, s.disk_budget_bytes, s.gpu, s.learning), (RankBand::Diamond, 4_294_967_296, false, false));

    for bad in [
        json!({ "regions": [] }),
        json!({ "regions": ["NA1"] }),
        json!({ "rankFloor": "mine" }),
        json!({ "diskBudgetBytes": 1000 }),
        json!({ "tierA": [99999] }),
        json!({ "somethingElse": true }),
    ] {
        let (st, b) = call(&t.app, Method::PATCH, "/api/settings", Some(bad.clone())).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{bad}: {b}");
        assert!(serde_json::from_str::<Value>(&b).unwrap()["error"].is_string());
    }
    assert_eq!(t.store.settings.lock().regions, vec![Platform::Euw1, Platform::Kr], "a refused patch changes nothing");

    let (st, b) = call(&t.app, Method::PATCH, "/api/settings", Some(json!({ "firstRunComplete": true }))).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert!(get(&t.app, "/api/status").await["firstRunComplete"].as_bool().unwrap());
}

#[tokio::test]
async fn model_rollback() {
    let t = harness(KeyOffer::Missing);
    let (s, b) = call(&t.app, Method::POST, "/api/model/rollback", Some(json!({ "version": "v2" }))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let m: ModelStatus = serde_json::from_str(&b).unwrap();
    assert_eq!(m.checkpoints.iter().find(|c| c.active).unwrap().version, "v2");
    assert!(t.learner.log.lock()[0].contains("v2"));
    let (s, _) = call(&t.app, Method::POST, "/api/model/rollback", Some(json!({ "version": "v99" }))).await;
    assert_eq!(s, StatusCode::CONFLICT);
}

// ---------------------------------------------------------------- the key

#[tokio::test]
async fn key_offer_states() {
    for (offer, expect) in [
        (KeyOffer::Available, json!({ "state": "available" })),
        (KeyOffer::Missing, json!({ "state": "missing" })),
        (KeyOffer::Empty, json!({ "state": "empty" })),
        (KeyOffer::Unreadable { reason: "not valid JSON".into() }, json!({ "state": "unreadable", "reason": "not valid JSON" })),
    ] {
        let t = harness(offer.clone());
        let v = get(&t.app, "/api/settings").await;
        assert_eq!(v["key"]["ireliaOffer"], expect);
        assert_eq!(v["key"]["set"], false);
        assert_eq!(v["key"]["masked"], "");
        assert_eq!(v["key"]["portal"], "https://developer.riotgames.com");
        // Importing works only when offered; otherwise a reason, and no key is set.
        let (s, b) = call(&t.app, Method::POST, "/api/key/import-irelia", None).await;
        if offer == KeyOffer::Available {
            assert_eq!(s, StatusCode::OK, "{b}");
            let k: Value = serde_json::from_str(&b).unwrap();
            assert_eq!(k["set"], true);
            assert_eq!(k["masked"], bf_core::config::mask_key(IRELIA_KEY));
            assert_eq!(t.keys.key.lock().as_deref(), Some(IRELIA_KEY));
        } else {
            assert_eq!(s, StatusCode::CONFLICT, "{b}");
            assert!(t.keys.key.lock().is_none());
        }
        assert!(!b.contains(IRELIA_KEY) && !b.contains("00000000-irelia"), "{b}");
    }
}

#[tokio::test]
async fn the_key_never_appears_in_a_response_or_a_log_line() {
    let logs = captured_logs();
    let t = harness(KeyOffer::Available);
    let mut bodies = vec![];
    // Pasted: stored as given (trimmed), answered masked.
    let (s, b) = call(&t.app, Method::POST, "/api/key", Some(json!({ "key": format!("  {KEY}  ") }))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(t.keys.key.lock().as_deref(), Some(KEY));
    assert_eq!(serde_json::from_str::<Value>(&b).unwrap()["masked"], "RGAPI-…test");
    bodies.push(b);
    // Malformed pastes: refused without repeating them.
    for bad in [json!({ "key": "RGAPI-00000000 test" }), json!({ "key": "00000000-test-RGAPI" }), json!({ "nokey": KEY })] {
        let (s, b) = call(&t.app, Method::POST, "/api/key", Some(bad)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        bodies.push(b);
    }
    let (s, b) = call_raw(&t.app, Method::POST, "/api/key", Some(format!("{{\"key\": \"{KEY}\"")), &[]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "broken JSON");
    bodies.push(b);
    // Riot refused it: the error is shown, scrubbed even if the store put the key in it.
    *t.keys.error.lock() = Some(format!("403 Forbidden for {KEY}"));
    let v = get(&t.app, "/api/settings").await;
    assert_eq!(v["key"]["error"], "403 Forbidden for RGAPI-…test");
    bodies.push(v.to_string());
    // An error from the store carrying the key is scrubbed too.
    *t.keys.offer.lock() = KeyOffer::Unreadable { reason: format!("bad file near {KEY}") };
    let (_, b) = call(&t.app, Method::POST, "/api/key/import-irelia", None).await;
    bodies.push(b);
    bodies.push(get(&t.app, "/api/settings").await.to_string());
    // Every other route.
    for uri in ["/api/status", "/api/fetch", "/api/storage", "/api/model", "/api/data-age", "/api/build?champion=103"] {
        bodies.push(get(&t.app, uri).await.to_string());
    }
    for b in &bodies {
        assert!(!b.contains(KEY) && !b.contains("00000000-test"), "the key leaked: {b}");
    }
    for l in logs.lock().iter() {
        assert!(!l.contains("00000000-"), "the key leaked into the log: {l}");
    }
    assert!(logs.lock().iter().any(|l| l.contains("[key] a new key was set")));
}

#[tokio::test]
async fn other_sites_cannot_use_the_server() {
    let t = harness(KeyOffer::Missing);
    let (s, _) = call_raw(&t.app, Method::POST, "/api/key", Some(json!({ "key": KEY }).to_string()), &[("origin", "https://evil.example")]).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(t.keys.key.lock().is_none());
    let mut req = Request::builder().uri("/api/status").header("host", "evil.example:5184").body(Body::empty()).unwrap();
    req.headers_mut().insert("host", "evil.example:5184".parse().unwrap());
    let res = crate::routes::router(t.app.clone(), None).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "DNS rebinding");
    let (s, _) = call_raw(&t.app, Method::GET, "/api/status", None, &[("origin", "http://localhost:5173")]).await;
    assert_eq!(s, StatusCode::OK, "the Vite dev server");
}

// ---------------------------------------------------------------- champ select, import, live

fn collect_events(app: &Arc<App>) -> Arc<Mutex<Vec<String>>> {
    let mut rx = app.events.subscribe();
    let out = Arc::new(Mutex::new(vec![]));
    let o2 = out.clone();
    tokio::spawn(async move {
        while let Ok(e) = rx.recv().await {
            o2.lock().push(e);
        }
    });
    out
}

async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn champ_select_from_client_events_shows_a_build_and_never_a_name() {
    let t = harness(KeyOffer::Missing);
    let events = collect_events(&t.app);
    let me = t.app.clone();
    let lcu = LcuConnector::new(vec![], Arc::new(move |ev| me.on_lcu_event(ev)));
    *lcu.patch.lock() = t.app.patch();
    *t.app.lcu.lock() = Some(lcu.clone());
    // The same path as the client's websocket events.
    lcu.inject_event("/lol-gameflow/v1/gameflow-phase", "Update", &json!("ChampSelect"));
    lcu.inject_event("/lol-champ-select/v1/session", "Update", &crate::lcu::champ_select::tests::ranked_session());
    settle().await;
    let d = get(&t.app, "/api/draft").await;
    assert_eq!(d["champSelect"]["draft"]["champion"], 103);
    assert_eq!(d["champSelect"]["draft"]["role"], "MIDDLE");
    assert_eq!(d["pending"], false);
    let rec: BuildRecommendation = serde_json::from_value(d["recommendation"].clone()).unwrap();
    assert_eq!((rec.champion, rec.role), (103, Role::Middle));
    assert_eq!(*t.store.activity.lock(), vec![Activity::ChampSelect], "the scheduler pauses");
    assert_eq!(*t.learner.activity.lock(), vec![Activity::ChampSelect]);

    // A new pick updates the draft and the build.
    let mut s = crate::lcu::champ_select::tests::ranked_session();
    s["theirTeam"][1]["championId"] = json!(134);
    lcu.inject_event("/lol-champ-select/v1/session", "Update", &s);
    settle().await;
    let d = get(&t.app, "/api/draft").await;
    assert_eq!(d["champSelect"]["draft"]["enemies"], json!([[122, null], [134, null]]));

    let all = events.lock().join("\n") + &d.to_string();
    assert!(all.contains("\"type\":\"draft\"") && all.contains("\"recommendation\":{"));
    for id in crate::lcu::champ_select::tests::identifying_strings() {
        assert!(!all.contains(&id), "{id:?} leaked to the dashboard");
    }
    // Champ select ends: the draft goes.
    lcu.inject_event("/lol-gameflow/v1/gameflow-phase", "Update", &json!("Lobby"));
    settle().await;
    assert_eq!(get(&t.app, "/api/draft").await, Value::Null);
    assert_eq!(t.store.activity.lock().last(), Some(&Activity::Idle));
    // The user's own account goes to the store; the end of a game too.
    t.app.on_lcu_event(LcuEvent::GameEnded);
    assert_eq!(*t.store.games_ended.lock(), 1);
}

#[tokio::test]
async fn import_through_the_route_for_any_champion() {
    let t = harness(KeyOffer::Missing);
    let (s, b) = call(&t.app, Method::POST, "/api/import", Some(json!({ "champion": 222 }))).await;
    assert_eq!(s, StatusCode::CONFLICT, "no client: {b}");
    assert!(b.contains("not connected"));
    let fake = Arc::new(crate::lcu::importer::tests::fake(vec![], 5, vec![]));
    *t.app.lcu_api.write() = Some(fake.clone() as Arc<dyn LcuApi>);
    let (s, b) = call(&t.app, Method::POST, "/api/import", Some(json!({ "champion": 222, "role": "BOTTOM" }))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let v: Value = serde_json::from_str(&b).unwrap();
    assert_eq!(keys_of(&v), vec!["runePage", "itemSet", "title"]);
    assert_eq!(v["title"], "Build Fetcher · Jinx Bot");
    assert_eq!((v["runePage"]["ok"].clone(), v["itemSet"]["ok"].clone()), (json!(true), json!(true)));
    assert_eq!(fake.sets.lock()["itemSets"][0]["associatedChampions"], json!([222]));
    // Again, for another champion: the app's own page and set are replaced, nothing else.
    let (_, b) = call(&t.app, Method::POST, "/api/import", Some(json!({ "champion": 266 }))).await;
    let v: Value = serde_json::from_str(&b).unwrap();
    assert_eq!((v["runePage"]["replaced"].clone(), v["itemSet"]["replaced"].clone()), (json!(true), json!(true)));
    assert_eq!(fake.pages.lock().len(), 1);
    assert_eq!(fake.sets.lock()["itemSets"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn live_game_gives_several_options_and_no_names() {
    let t = harness(KeyOffer::Missing);
    let events = collect_events(&t.app);
    let raw = crate::lcu::live_context::tests::snapshot();
    t.app.on_live_data(&raw, json!(null), false);
    settle().await;
    let l = get(&t.app, "/api/live").await;
    assert_eq!(keys_of(&l), vec!["active", "simulated", "context", "advice", "error", "performance"]);
    let ctx: LiveContext = serde_json::from_value(l["context"].clone()).unwrap();
    assert_eq!(ctx.draft.champion, 103);
    let advice: LiveAdvice = serde_json::from_value(l["advice"].clone()).unwrap();
    assert!(advice.next_buys.len() >= 2, "several options, never one instruction");
    assert!(advice.next_buys.iter().all(|o| !ctx.my_items.contains(&o.option.item_ids()[0])), "nothing already owned");
    assert_eq!(*t.store.activity.lock(), vec![Activity::InGame]);
    let all = events.lock().join("\n");
    for name in ["MeTheUser", "AllyTopName", "EnemyMidName", "#EUW"] {
        assert!(!all.contains(name), "{name} leaked");
    }
    t.app.on_live_ended();
    assert_eq!(get(&t.app, "/api/live").await, Value::Null);
    assert_eq!(t.store.activity.lock().last(), Some(&Activity::Idle));
}

#[tokio::test]
async fn simulated_game_drives_the_overlay() {
    let t = harness(KeyOffer::Missing);
    let (s, _) = call(&t.app, Method::POST, "/api/dev/simulate-live", Some(json!({ "lane": "MIDDLE", "champion": "Orianna" }))).await;
    assert_eq!(s, StatusCode::OK);
    settle().await;
    let l = get(&t.app, "/api/live").await;
    assert_eq!(l["simulated"], true);
    assert_eq!(l["context"]["draft"]["champion"], 61);
    assert_eq!(l["context"]["dragons"], json!([1, 0]));
    assert!(l["performance"]["me"]["cs"].is_number());
    let (_, b) = call(&t.app, Method::POST, "/api/dev/stop-simulation", None).await;
    assert!(b.contains("\"stopped\":true"));
    assert_eq!(get(&t.app, "/api/live").await, Value::Null);
}
