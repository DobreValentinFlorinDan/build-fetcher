//! Port of `server/index.ts` (everything but the HTTP routes, which are in routes.rs): the realtime fan-out, the model
//! and scraper threads, the matchup and its build, the League client and the live game, the patch watch, and the
//! automatic evaluation.

use crate::jobs::{JobKind, Jobs};
use crate::lcu::connector::{parse_champ_select, LcuAccount, LcuConnector, LcuEvent};
use crate::lcu::live_client::{PerformanceTracker, END_AFTER_FAILURES, LIVE_PORT};
use crate::matchup::{LiveView, Matchup, MatchupSession};
use crate::model_worker::{EnvPatch, ModelClient, ModelEvent, Request};
use crate::scraper::{ScraperController, ScraperEvent};
use irelia::app_config::{read_app_config, update_app_config};
use irelia::auto_eval::{AutoEvalDeps, AutoEvaluation};
use irelia::config::{config, CHAMPION_ID, LIVE_IDLE_POLL_MS, LIVE_POLL_MS};
use irelia::db::{ChampionTotals, Db};
use irelia::ddragon::{load_patch_history, load_static_data, StaticData};
use irelia::engine::adaptive::{analyse_live_game, identify};
use irelia::engine::build_plan::{build_plan, plan_status};
use irelia::engine::exclusivity::{build_exclusivity_index, ExclusivityIndex};
use irelia::engine::item_catalog::{item_catalog, ItemCatalog};
use irelia::engine::lane_opponent::guess_lane_opponents;
use irelia::model::eval_base::{plan_evaluation, EvalPlan};
use irelia::model::live::LiveHint;
use irelia::model::profiles::{ChampionProfiles, PROFILE_DEFAULTS};
use irelia::model::run_eval::eval_size;
use irelia::model::service::{read_evaluation, EvalSummary};
use irelia::model::weights::HISTORY_PATCHES;
use irelia::patches::VersionRow;
use irelia::scope::{default_scope, load_scope};
use irelia::types::to_lane;
use irelia::util::now_ms;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const QUEUE_PHASES: [&str; 2] = ["Matchmaking", "ReadyCheck"];

pub struct Inner {
    pub live_view: Option<LiveView>,
    pub matchup: Matchup,
    pub build: Option<Value>,
    pub live_state: Option<Value>,
    pub live_next: Value,
    pub live_buy: Value,
    reranking: bool,
    champ_select_since: Option<i64>,
    game_since: Option<i64>,
    live_shown: Vec<i64>,
    hint_cache: Option<(Value, LiveHint)>,
    pub simulated_draft: Option<Value>,
    sim_stop: Option<Arc<AtomicBool>>,
    profiles: Option<Arc<ChampionProfiles>>,
    catalog: Option<(String, Arc<ItemCatalog>)>,
    faced: HashMap<&'static str, (i64, HashMap<i64, f64>)>,
    /// The client state last written to the log.
    client_logged: String,
    building: bool,
    build_again: bool,
    build_retry_ms: u64,
    build_timer: u64,
    applied_evaluation: String,
}

pub struct App {
    pub rt: tokio::runtime::Handle,
    pub data: RwLock<Arc<StaticData>>,
    pub excl: RwLock<Arc<ExclusivityIndex>>,
    pub db: Mutex<Db>,
    pub events: tokio::sync::broadcast::Sender<String>,
    pub models: Arc<ModelClient>,
    pub scraper: Arc<ScraperController>,
    pub jobs: Arc<Jobs>,
    pub lcu: Arc<LcuConnector>,
    pub session: Mutex<MatchupSession>,
    pub inner: Mutex<Inner>,
    pub auto_eval: Mutex<Option<Arc<AutoEvaluation>>>,
    live_running: Mutex<Option<Arc<AtomicBool>>>,
    pub game_mode: crate::game_mode::GameMode,
    pub extra: Mutex<Option<Arc<dyn crate::routes::Extra>>>,
}

/// Loads Data Dragon (cached on disk) with the fingerprints of the patches patch memory reaches.
pub async fn static_data(known: Option<&[String]>) -> anyhow::Result<StaticData> {
    let mut d = load_static_data(known).await?;
    let versions = d.versions.clone().unwrap_or_else(|| vec![d.version.clone()]);
    d.history = Some(load_patch_history(&versions, HISTORY_PATCHES).await);
    Ok(d)
}

fn build_sources() -> Vec<String> {
    load_scope().map(|s| s.build_sources).unwrap_or_else(|_| vec!["otp".into()])
}

/// The app behind a callback's weak reference. The lock is released before the app is used: a callback that kept it
/// through its body froze the app at the first champ-select event (the client's callback paused the fetcher, whose
/// status callback then waited for the same lock, on the same thread).
fn app_of(w: &Mutex<std::sync::Weak<App>>) -> Option<Arc<App>> {
    let app = w.lock().upgrade();
    app
}

impl App {
    pub fn broadcast(&self, ev: Value) {
        let _ = self.events.send(irelia::js::stringify(&ev));
    }

    pub fn data(&self) -> Arc<StaticData> {
        self.data.read().clone()
    }

    pub async fn start(db: Db, data: StaticData) -> Arc<App> {
        let data = Arc::new(data);
        let (events, _) = tokio::sync::broadcast::channel::<String>(1024);
        let weak: Arc<Mutex<std::sync::Weak<App>>> = Arc::new(Mutex::new(std::sync::Weak::new()));
        let w = weak.clone();
        let models = ModelClient::new(data.clone(), Arc::new(move |ev| {
            if let Some(app) = app_of(&w) {
                app.on_model_event(ev);
            }
        }));
        let w = weak.clone();
        let cfg = config();
        let scraper = ScraperController::new(&data.patch, &data.previous_patch, &cfg.riot_api_key, Arc::new(move |ev| {
            if let Some(app) = app_of(&w) {
                match ev {
                    ScraperEvent::Status(s) => {
                        if let Some(p) = s["me"]["platform"].as_str() {
                            app.models.set_env(EnvPatch { own_platform: Some(Some(p.to_string())), ..Default::default() });
                        }
                        let s = app.with_backfill_progress(s);
                        app.broadcast(json!({ "type": "scraper", "status": s }));
                    }
                    ScraperEvent::RateLimit(s) => app.broadcast(json!({ "type": "ratelimit", "snapshot": s })),
                }
            }
        }));
        scraper.set_in_game(false, Some(read_app_config().get("pauseFetchingInGame") != Some(&json!(false))));
        let w = weak.clone();
        let w2 = weak.clone();
        let jobs = Jobs::new(
            data.clone(),
            Arc::new(move || {
                if let Some(app) = app_of(&w) {
                    app.models.set_env(EnvPatch { evaluating: Some(app.jobs.running_job("evaluate").is_some()), ..Default::default() });
                    app.broadcast(json!({ "type": "jobs", "status": app.jobs_status() }));
                }
            }),
            Arc::new(|games| Some(crate::system::planned_threads(games))),
            Arc::new(move |progress| {
                let app = app_of(&w2).ok_or_else(|| anyhow::anyhow!("the app is closing"))?;
                let stop = { let a = app.clone(); move || a.jobs.audit_stopped() };
                crate::audit::run_audit_job(&config().data_dir, progress, &stop)
            }),
        );
        let w = weak.clone();
        let guess: crate::lcu::connector::GuessOpponents = Arc::new(move |enemies, lanes| {
            match app_of(&w) {
                Some(app) => app.guess_opponents(enemies, lanes),
                None => lanes.iter().map(|l| (*l, None)).collect(),
            }
        });
        let w = weak.clone();
        let lcu = LcuConnector::new(cfg.league_paths.clone(), CHAMPION_ID, guess.clone(), Arc::new(move |ev| {
            if let Some(app) = app_of(&w) {
                app.on_lcu_event(ev);
            }
        }));
        let g2 = guess.clone();
        let session = MatchupSession::new(
            Box::new(|| read_app_config().get("lane").and_then(|v| v.as_str()).and_then(|l| to_lane(Some(l)))),
            Box::new(|lane| {
                update_app_config(vec![("lane", Some(json!(lane)))]);
            }),
            Box::new(move |e, l| g2(e, l)),
        );
        let mut session = session;
        let matchup = session.resolve(Some(&lcu.state()), None);
        let app = Arc::new(App {
            rt: tokio::runtime::Handle::current(),
            excl: RwLock::new(Arc::new(build_exclusivity_index(&data))),
            data: RwLock::new(data),
            db: Mutex::new(db),
            events,
            models,
            scraper,
            jobs,
            lcu,
            session: Mutex::new(session),
            inner: Mutex::new(Inner {
                live_view: None,
                matchup,
                build: None,
                live_state: None,
                live_next: Value::Null,
                live_buy: Value::Null,
                reranking: false,
                champ_select_since: None,
                game_since: None,
                live_shown: vec![],
                hint_cache: None,
                simulated_draft: None,
                sim_stop: None,
                profiles: None,
                catalog: None,
                faced: HashMap::new(),
                client_logged: String::new(),
                building: false,
                build_again: false,
                build_retry_ms: 5000,
                build_timer: 0,
                applied_evaluation: String::new(),
            }),
            auto_eval: Mutex::new(None),
            live_running: Mutex::new(None),
            game_mode: crate::game_mode::GameMode::default(),
            extra: Mutex::new(None),
        });
        *weak.lock() = Arc::downgrade(&app);
        app.lcu.start();
        app.request_build(0);
        let auto = AutoEvaluation::new(Arc::new(AutoDeps(Arc::downgrade(&app))));
        *app.auto_eval.lock() = Some(auto.clone());
        auto.check();
        // Every minute: an evaluation another computer's backup left, then the automatic evaluation.
        let a = app.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                a.adopt_evaluation();
                let auto = a.auto_eval.lock().clone();
                if let Some(auto) = auto {
                    let _ = tokio::task::spawn_blocking(move || auto.check()).await;
                }
            }
        });
        // Patch watch: every hour.
        let a = app.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                match static_data(None).await {
                    Ok(next) if next.version != a.data().version => a.set_patch(next),
                    Ok(_) => {}
                    Err(e) => eprintln!("[ddragon] refresh failed {e}"),
                }
            }
        });
        app
    }

    pub fn set_patch(&self, next: StaticData) {
        let next = Arc::new(next);
        *self.excl.write() = Arc::new(build_exclusivity_index(&next));
        *self.data.write() = next.clone();
        self.scraper.set_patch(&next.patch, &next.previous_patch);
        self.models.set_patch(next.clone());
        self.jobs.set_data(next.clone());
        self.inner.lock().catalog = None;
        self.broadcast(json!({ "type": "patch", "patch": next.patch, "version": next.version }));
        eprintln!("[ddragon] new patch {} - older games are now excluded from recommendations", next.patch);
    }

    // ---------------------------------------------------------------- status of the parts

    pub fn jobs_status(&self) -> Value {
        let mut s = self.jobs.status();
        if let Some(auto) = self.auto_eval.lock().clone() {
            s["auto"] = auto.status().to_json();
            if let Some(p) = auto.plan() {
                s["plan"] = serde_json::to_value(p).unwrap();
            }
        }
        s
    }

    fn with_backfill_progress(&self, mut status: Value) -> Value {
        let b = status["backfill"].clone();
        if !b.is_object() {
            return status;
        }
        static RUN: std::sync::LazyLock<Mutex<Option<(i64, Option<irelia::progress::Progress>, bool)>>> = std::sync::LazyLock::new(|| Mutex::new(None));
        let started = b["startedAt"].as_i64().unwrap_or(0);
        let total = b["total"].as_f64().unwrap_or(0.0);
        let mut run = RUN.lock();
        if run.as_ref().map(|r| r.0 != started).unwrap_or(true) {
            *run = Some((started, None, false));
        }
        let r = run.as_mut().unwrap();
        if b["finishedAt"].is_null() {
            if r.1.is_none() && total > 0.0 {
                r.1 = Some(irelia::progress::Progress::starting_at(irelia::progress::expected_ms("backfill", total, None), started));
            }
            let tracker = r.1.clone().unwrap_or_else(|| irelia::progress::Progress::starting_at(None, started));
            let fraction = if total > 0.0 { Some(b["done"].as_f64().unwrap_or(0.0) / total) } else { None };
            status["backfill"]["progress"] = tracker.info(fraction);
            return status;
        }
        if !r.2 && !b["phase"].as_str().unwrap_or("").starts_with("Failed") && total > 0.0 {
            let fin = b["finishedAt"].as_i64().unwrap_or(0);
            irelia::progress::record_timing("backfill", irelia::progress::Timing { size: total, ms: (fin - started) as f64, at: fin as f64, ..Default::default() });
            r.2 = true;
        }
        status
    }

    fn on_model_event(self: &Arc<Self>, ev: ModelEvent) {
        match ev {
            ModelEvent::Status(status, rows) => {
                if let Some(rows) = rows {
                    let p = Arc::new(ChampionProfiles::new(&rows.0, &rows.1, self.data(), PROFILE_DEFAULTS));
                    self.inner.lock().profiles = Some(p);
                }
                self.broadcast(json!({ "type": "model", "status": status }));
                let state = status["state"].as_str().unwrap_or("");
                let no_build = self.inner.lock().build.is_none();
                if state == "ready" || state == "failed" || no_build {
                    self.request_build(60);
                }
            }
            ModelEvent::Data => {
                if self.inner.lock().matchup.phase != "game" {
                    self.request_build(60);
                }
            }
        }
    }

    // ---------------------------------------------------------------- lane opponents

    fn faced_in(&self, lane: &'static str) -> HashMap<i64, f64> {
        let stale = match self.inner.lock().faced.get(lane) {
            Some((at, f)) if now_ms() - at < 60_000 => return f.clone(),
            Some((_, f)) => Some(f.clone()),
            None => None,
        };
        let data = self.data();
        // Champ select must never wait behind a long database job (an evaluation's plan, an import, archiving): after
        // a short wait the last counts are used (none yet: no guess this time), and the next event tries again.
        let Some(db) = self.db.try_lock_for(Duration::from_millis(250)) else {
            eprintln!("[client] the database is busy: lane opponents guessed from the last counts");
            return stale.unwrap_or_default();
        };
        let faced: HashMap<i64, f64> = db
            .opponent_counts(&[data.patch.clone(), data.previous_patch.clone()], Some(lane), &build_sources())
            .unwrap_or_default()
            .into_iter()
            .map(|(id, n)| (id, n as f64))
            .collect();
        drop(db);
        self.inner.lock().faced.insert(lane, (now_ms(), faced.clone()));
        faced
    }

    pub fn guess_opponents(&self, enemies: &[i64], lanes: &[&'static str]) -> Vec<(&'static str, Option<i64>)> {
        let faced: HashMap<&'static str, HashMap<i64, f64>> = lanes.iter().map(|l| (*l, self.faced_in(l))).collect();
        let data = self.data();
        let champs = |id: i64| data.champion(id);
        guess_lane_opponents(enemies, lanes, &faced, &champs)
    }

    // ---------------------------------------------------------------- League client

    pub fn effective_lcu(&self) -> Value {
        let state = self.lcu.state();
        let sim = self.inner.lock().simulated_draft.clone();
        match sim {
            Some(d) if state["champSelect"].is_null() => {
                let mut s = state.clone();
                s["phase"] = json!("ChampSelect (simulated)");
                s["champSelect"] = d;
                s
            }
            _ => state,
        }
    }

    fn on_lcu_event(self: &Arc<Self>, ev: LcuEvent) {
        match ev {
            LcuEvent::Account(a) => self.set_account(a),
            LcuEvent::GameEnded => self.scraper.game_ended(),
            LcuEvent::State(state) => {
                {
                    let mut i = self.inner.lock();
                    i.champ_select_since = if state["phase"] == "ChampSelect" { i.champ_select_since.or(Some(now_ms())) } else { None };
                    // The log follows the client: its connection, each phase, and whether champ select is on Irelia.
                    let cs = &state["champSelect"];
                    let seen = format!(
                        "{} | {} | {}",
                        state["status"].as_str().unwrap_or("?"),
                        state["phase"].as_str().unwrap_or("?"),
                        if cs.is_null() { "no draft".to_string() } else { format!("draft: Irelia {}, position {}, {} enemies shown", cs["isIrelia"], cs["myPosition"], cs["enemies"].as_array().map(|e| e.iter().filter(|x| x["championId"].as_i64().unwrap_or(0) != 0).count()).unwrap_or(0)) }
                    );
                    if i.client_logged != seen {
                        eprintln!("[client] {seen}");
                        i.client_logged = seen;
                    }
                }
                self.broadcast(json!({ "type": "lcu", "state": self.effective_lcu() }));
                self.refresh_matchup();
                let phase = state["phase"].as_str().unwrap_or("");
                if phase == "InProgress" || phase == "Reconnect" || state["status"] != "connected" {
                    self.live_start();
                } else {
                    self.live_stop();
                }
            }
        }
    }

    fn set_account(&self, a: LcuAccount) {
        self.scraper.set_me_account(irelia::scraper::account::ClientAccount { game_name: a.game_name, tag_line: a.tag_line, platform: a.platform });
    }

    // ---------------------------------------------------------------- the matchup and its build

    pub fn refresh_matchup(self: &Arc<Self>) {
        let lcu = self.effective_lcu();
        let live_view = self.inner.lock().live_view.clone();
        let next = self.session.lock().resolve(Some(&lcu), live_view.as_ref());
        let busy = next.phase != "idle";
        let phase = self.lcu.state()["phase"].as_str().unwrap_or("").to_string();
        self.models.set_env(EnvPatch { busy: Some(busy), queued: Some(QUEUE_PHASES.contains(&phase.as_str())), ..Default::default() });
        self.jobs.set_busy(busy);
        self.scraper.set_in_game(busy, None);
        self.game_mode.set(busy);
        if let Some(x) = self.extra.lock().clone() {
            x.set_busy(busy);
        }
        let (rebuild, phase_changed) = {
            let mut i = self.inner.lock();
            if next == i.matchup {
                return;
            }
            let r = next.key != i.matchup.key;
            let p = next.phase != i.matchup.phase;
            i.matchup = next.clone();
            (r, p)
        };
        eprintln!(
            "[matchup] {}: {}, opponent {} ({}), {} enemies, {} allies",
            next.phase,
            next.lane,
            next.opponent_id.map(|o| o.to_string()).unwrap_or_else(|| "unknown".into()),
            next.opponent_from.unwrap_or("-"),
            next.enemies.len(),
            next.allies.len()
        );
        self.broadcast(json!({ "type": "matchup", "matchup": next.to_json() }));
        if rebuild {
            self.request_build(60);
        }
        if phase_changed {
            if let Some(a) = self.auto_eval.lock().clone() {
                self.rt.spawn_blocking(move || a.check());
            }
        }
    }

    pub fn request_build(self: &Arc<Self>, delay_ms: u64) {
        let ticket = {
            let mut i = self.inner.lock();
            i.build_timer += 1;
            i.build_timer
        };
        let me = self.clone();
        self.rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            if me.inner.lock().build_timer != ticket {
                return;
            }
            me.run_build().await;
        });
    }

    async fn run_build(self: &Arc<Self>) {
        {
            let mut i = self.inner.lock();
            if i.building {
                i.build_again = true;
                return;
            }
            i.building = true;
        }
        let m = self.inner.lock().matchup.clone();
        let r = self
            .models
            .request(Request::Draft { lane: m.lane, opponent: m.opponent_id, enemies: m.enemies.clone(), allies: m.allies.clone(), regions: m.regions.clone() }, 120_000)
            .await;
        match r {
            Ok(response) => {
                let build = json!({ "key": m.key, "response": response, "at": now_ms() });
                {
                    let mut i = self.inner.lock();
                    i.build = Some(build.clone());
                    i.build_retry_ms = 5000;
                }
                eprintln!("[build] {} for {}", build["response"]["mode"].as_str().unwrap_or("?"), m.key);
                self.broadcast(json!({ "type": "build", "build": build }));
                self.log_draft(&m, &build["response"]);
            }
            Err(e) => {
                eprintln!("[build] {e}");
                let wait = {
                    let mut i = self.inner.lock();
                    let w = i.build_retry_ms;
                    i.build_retry_ms = (w * 2).min(60_000);
                    w
                };
                let me = self.clone();
                self.rt.spawn(async move {
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                    me.request_build(60);
                });
            }
        }
        let again = {
            let mut i = self.inner.lock();
            i.building = false;
            std::mem::take(&mut i.build_again)
        };
        if again {
            Box::pin(self.run_build()).await;
        }
    }

    fn log_draft(&self, m: &Matchup, response: &Value) {
        let since = self.inner.lock().champ_select_since;
        let lcu = self.lcu.state();
        let Some(since) = since else { return };
        if lcu["phase"] != "ChampSelect" || lcu["champSelect"]["isIrelia"] != true {
            return;
        }
        let shown = if response["mode"] == "draft" && !response["draft"].is_null() { &response["draft"]["build"] } else { &response["baseline"] };
        let _ = self.db.lock().log_recommendation(&format!("draft:{since}"), "draft", Some(m.lane), m.opponent_id, response["mode"].as_str().unwrap_or("baseline"), &shown_build(shown));
    }

    // ---------------------------------------------------------------- the live game

    fn champion_id(&self, p: &Value) -> Option<i64> {
        let data = self.data();
        p["championName"].as_str().and_then(|n| data.champion_by_key(n)).map(|c| c.id)
    }

    /// The live game in the matchup's terms, and who you face.
    pub fn live_hint(self: &Arc<Self>, raw: &Value) -> LiveHint {
        if let Some((r, h)) = &self.inner.lock().hint_cache {
            if r == raw {
                return h.clone();
            }
        }
        let (me_i, opp_i) = identify(raw, None);
        let players: Vec<Value> = raw["allPlayers"].as_array().cloned().unwrap_or_default();
        let me = me_i.map(|i| &players[i]);
        let ids = |ps: Vec<&Value>| ps.into_iter().filter_map(|p| self.champion_id(p)).collect::<Vec<i64>>();
        let view = LiveView {
            lane: to_lane(me.and_then(|m| m["position"].as_str())),
            position_opponent: opp_i.and_then(|i| self.champion_id(&players[i])),
            enemies: match me {
                Some(m) => ids(players.iter().filter(|p| p["team"] != m["team"]).collect()),
                None => vec![],
            },
            allies: match me_i {
                Some(mi) => ids(players.iter().enumerate().filter(|(i, p)| p["team"] == players[mi]["team"] && *i != mi).map(|(_, p)| p).collect()),
                None => vec![],
            },
        };
        self.inner.lock().live_view = Some(view.clone());
        let lcu = self.effective_lcu();
        let m = self.session.lock().resolve(Some(&lcu), Some(&view));
        let enemy = match (me, m.opponent_id) {
            (Some(me), Some(oid)) => players.iter().find(|p| p["team"] != me["team"] && self.champion_id(p) == Some(oid)),
            _ => None,
        };
        let hint = LiveHint { opponent_name: enemy.map(|e| Some(e["championName"].as_str().unwrap_or("").to_string())), lane: Some(m.lane) };
        self.inner.lock().hint_cache = Some((raw.clone(), hint.clone()));
        hint
    }

    fn plan_items(&self) -> Arc<ItemCatalog> {
        let data = self.data();
        let mut i = self.inner.lock();
        if let Some((v, c)) = &i.catalog {
            if *v == data.version {
                return c.clone();
            }
        }
        let c = Arc::new(item_catalog(&data, &self.excl.read()));
        i.catalog = Some((data.version.clone(), c.clone()));
        c
    }

    pub fn plan_items_json(&self) -> Value {
        irelia::engine::item_catalog::catalog_json(&self.plan_items())
    }

    fn plan_target(&self, raw: &Value, hint: &LiveHint) -> Option<(i64, bool)> {
        let build = self.inner.lock().build.clone()?;
        let r = &build["response"];
        let shown = if r["mode"] == "draft" && !r["draft"].is_null() { &r["draft"]["build"] } else { &r["baseline"] };
        let rec: irelia::types::BuildRecommendation = serde_json::from_value(shown.clone()).ok()?;
        let (me, _) = identify(raw, hint.opponent_name.as_ref().map(|o| o.as_deref()));
        let owned: Vec<i64> = me
            .and_then(|i| raw["allPlayers"][i]["items"].as_array().cloned())
            .map(|a| a.iter().filter_map(|x| x["itemID"].as_i64()).filter(|id| *id > 0).collect())
            .unwrap_or_default();
        let items = self.plan_items();
        let plan = build_plan(&rec, &items);
        let (entries, next) = plan_status(&plan, &owned, &items);
        next.map(|n| (entries[n].entry.id, entries[n].entry.slot == "Boots"))
    }

    pub fn on_live_data(self: &Arc<Self>, raw: Value, performance: Value, simulated: bool) {
        {
            let mut i = self.inner.lock();
            if i.game_since.is_none() {
                i.game_since = Some(now_ms() - (raw["gameData"]["gameTime"].as_f64().unwrap_or(0.0) * 1000.0) as i64);
            }
        }
        let hint = self.live_hint(&raw);
        self.refresh_matchup();
        let profiles = self.inner.lock().profiles.clone();
        let data = self.data();
        let excl = self.excl.read().clone();
        let mut analysis = analyse_live_game(&raw, &data, &excl, profiles.as_deref(), hint.opponent_name.as_ref().map(|o| o.as_deref()));
        let (m_opp, m_from) = {
            let i = self.inner.lock();
            (i.matchup.opponent_id, i.matchup.opponent_from)
        };
        let from = match (m_opp, m_from) {
            (Some(_), Some(f)) => json!(match f {
                "game" => "position",
                "draft" => "draft",
                "manual" => "manual",
                _ => "guess",
            }),
            _ => {
                if analysis["laneOpponent"].is_null() {
                    Value::Null
                } else {
                    json!("position")
                }
            }
        };
        // laneOpponentFrom goes after laneOpponent (the TS spread order).
        let mut state = serde_json::Map::new();
        for (k, v) in analysis.as_object_mut().unwrap().iter() {
            state.insert(k.clone(), v.clone());
            if k == "laneOpponent" {
                state.insert("laneOpponentFrom".into(), from.clone());
            }
        }
        let (next, buy) = {
            let i = self.inner.lock();
            (i.live_next.clone(), i.live_buy.clone())
        };
        // TS order: ...analysis, laneOpponentFrom, performance, next, buy (laneOpponentFrom is a new key: after summary).
        let mut ordered = serde_json::Map::new();
        for (k, v) in state.iter() {
            if k != "laneOpponentFrom" {
                ordered.insert(k.clone(), v.clone());
            }
        }
        ordered.insert("laneOpponentFrom".into(), from);
        ordered.insert("performance".into(), performance);
        ordered.insert("next".into(), next);
        ordered.insert("buy".into(), buy);
        let state = Value::Object(ordered);
        self.inner.lock().live_state = Some(state.clone());
        self.broadcast(json!({ "type": "live", "state": state }));
        {
            let mut i = self.inner.lock();
            if i.reranking {
                return;
            }
            i.reranking = true;
        }
        let lite = json!({ "activePlayer": raw["activePlayer"], "allPlayers": raw["allPlayers"], "gameData": raw["gameData"] });
        let target = self.plan_target(&raw, &hint);
        let me = self.clone();
        self.rt.spawn(async move {
            let r = me.models.request(Request::Live { raw: lite, hint, plan_target: target }, 10_000).await;
            match r {
                Ok(v) => {
                    let (next, buy) = (v["next"].clone(), v["buy"].clone());
                    let send = {
                        let mut i = me.inner.lock();
                        let active = i.live_state.as_ref().map(|s| s["active"] == true).unwrap_or(false);
                        if !active || (next == i.live_next && buy == i.live_buy) {
                            None
                        } else {
                            i.live_next = next.clone();
                            i.live_buy = buy.clone();
                            if let Some(s) = i.live_state.as_mut() {
                                s["next"] = next.clone();
                                s["buy"] = buy.clone();
                            }
                            i.live_state.clone()
                        }
                    };
                    if let Some(s) = send {
                        me.broadcast(json!({ "type": "live", "state": s }));
                        if let (Some(id), false) = (next["itemId"].as_i64(), simulated) {
                            me.log_live_next(id);
                        }
                    }
                }
                Err(e) => eprintln!("[model] live re-rank failed: {e}"),
            }
            me.inner.lock().reranking = false;
        });
    }

    fn log_live_next(&self, item: i64) {
        let (session, shown) = {
            let mut i = self.inner.lock();
            if i.live_shown.last() == Some(&item) {
                return;
            }
            i.live_shown.push(item);
            (i.game_since, i.live_shown.clone())
        };
        let shown = json!({ "keystone": null, "core": [], "boots": null, "items": [], "next": shown });
        let _ = self.db.lock().log_recommendation(&format!("live:{}", session.map(|s| s.to_string()).unwrap_or_else(|| "null".into())), "live", None, None, "draft", &shown);
    }

    pub fn reset_live(&self) {
        {
            let mut i = self.inner.lock();
            i.live_state = None;
            i.live_view = None;
            i.hint_cache = None;
            i.live_next = Value::Null;
            i.live_buy = Value::Null;
            i.game_since = None;
            i.live_shown.clear();
        }
        self.models.live_reset();
    }

    pub fn on_live_ended(self: &Arc<Self>) {
        self.reset_live();
        self.broadcast(json!({ "type": "live", "state": {
            "active": false, "gameTime": 0, "myGold": 0, "players": [], "laneOpponent": null,
            "threat": { "apShare": 0, "tanks": 0, "healers": 0 }, "suggestions": [], "performance": null,
        } }));
        self.refresh_matchup();
    }

    /// Polls the Live Client Data API while a game may be on: 'data' every second, ended after three failures.
    pub fn live_start(self: &Arc<Self>) {
        let mut running = self.live_running.lock();
        if running.is_some() {
            return;
        }
        let flag = Arc::new(AtomicBool::new(true));
        *running = Some(flag.clone());
        drop(running);
        let me = self.clone();
        self.rt.spawn(async move {
            let mut tracker = PerformanceTracker::default();
            let mut active = false;
            let mut failures = 0u32;
            while flag.load(Ordering::SeqCst) {
                let r = crate::lcu::https::loopback_json(LIVE_PORT, "/liveclientdata/allgamedata", None, 2500).await;
                if !flag.load(Ordering::SeqCst) {
                    break;
                }
                match r {
                    Ok(raw) if raw["allPlayers"].as_array().map(|a| !a.is_empty()).unwrap_or(false) => {
                        if !active {
                            let (me_i, opp_i) = identify(&raw, None);
                            let who = |i: Option<usize>| i.map(|i| format!("{} ({})", raw["allPlayers"][i]["championName"].as_str().unwrap_or("?"), raw["allPlayers"][i]["position"].as_str().filter(|p| !p.is_empty()).unwrap_or("no position"))).unwrap_or_else(|| "not found".into());
                            eprintln!("[live] game data at {:.0} s: {} players, you: {}, same position on the other team: {}", raw["gameData"]["gameTime"].as_f64().unwrap_or(0.0), raw["allPlayers"].as_array().map(|a| a.len()).unwrap_or(0), who(me_i), who(opp_i));
                        }
                        active = true;
                        failures = 0;
                        // A real game ends a simulation.
                        let sim = me.inner.lock().sim_stop.take();
                        if let Some(s) = sim {
                            s.store(false, Ordering::SeqCst);
                            me.reset_live();
                        }
                        let hint = me.live_hint(&raw);
                        let data = me.data();
                        let perf = tracker.update(&raw, &|id| data.item(id).map(|i| i.gold_total).unwrap_or(0.0), hint.opponent_name.as_ref().map(|o| o.as_deref()));
                        me.on_live_data(raw, perf.unwrap_or(Value::Null), false);
                    }
                    other => {
                        if failures == 0 && me.lcu.state()["phase"] == "InProgress" {
                            // Once per run of failures while the client says a game is on.
                            match &other {
                                Err(e) => eprintln!("[live] no game data yet: {e}"),
                                Ok(_) => eprintln!("[live] no game data yet: the game answered without players"),
                            }
                        }
                        failures += 1;
                        if failures >= END_AFTER_FAILURES {
                            failures = 0;
                            if active {
                                eprintln!("[live] game data ended");
                                active = false;
                                tracker.reset();
                                if me.inner.lock().sim_stop.is_none() {
                                    me.on_live_ended();
                                }
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(if active { LIVE_POLL_MS } else { LIVE_IDLE_POLL_MS })).await;
            }
            if active && me.inner.lock().sim_stop.is_none() {
                me.on_live_ended();
            }
        });
    }

    pub fn live_stop(&self) {
        if let Some(f) = self.live_running.lock().take() {
            f.store(false, Ordering::SeqCst);
        }
    }

    /// POST /api/dev/simulate-live: drives the live tracker, the live panel and the overlay without League.
    pub fn simulate_live(self: &Arc<Self>, start_at: f64, lane: &'static str, positions: bool) {
        self.reset_live();
        let flag = Arc::new(AtomicBool::new(true));
        self.inner.lock().sim_stop = Some(flag.clone());
        let me = self.clone();
        self.rt.spawn(async move {
            let mut game = crate::lcu::simulate_live::SimulatedGame::new(start_at, lane, positions);
            let mut tracker = PerformanceTracker::default();
            while flag.load(Ordering::SeqCst) {
                let raw = game.tick();
                let hint = me.live_hint(&raw);
                let data = me.data();
                let perf = tracker.update(&raw, &|id| data.item(id).map(|i| i.gold_total).unwrap_or(0.0), hint.opponent_name.as_ref().map(|o| o.as_deref()));
                me.on_live_data(raw, perf.unwrap_or(Value::Null), true);
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        });
    }

    pub fn stop_simulation(&self) -> bool {
        match self.inner.lock().sim_stop.take() {
            Some(f) => {
                f.store(false, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    pub fn simulate_draft(self: &Arc<Self>, enemies: Vec<i64>, lane: String) -> Value {
        let sim = if enemies.is_empty() {
            None
        } else {
            let session = json!({
                "localPlayerCellId": 0,
                "myTeam": [{ "cellId": 0, "championId": CHAMPION_ID, "championPickIntent": 0, "assignedPosition": lane }],
                "theirTeam": enemies.iter().enumerate().map(|(i, id)| json!({ "cellId": 5 + i, "championId": id, "championPickIntent": 0, "assignedPosition": "" })).collect::<Vec<_>>(),
                "actions": [enemies.iter().enumerate().map(|(i, id)| json!({ "actorCellId": 5 + i, "championId": id, "completed": true, "type": "pick" })).collect::<Vec<_>>()],
            });
            let me = self.clone();
            Some(parse_champ_select(&session, CHAMPION_ID, &move |e, l| me.guess_opponents(e, l)))
        };
        self.inner.lock().simulated_draft = sim;
        let state = self.effective_lcu();
        self.broadcast(json!({ "type": "lcu", "state": state }));
        self.refresh_matchup();
        state
    }

    // ---------------------------------------------------------------- evaluations

    /// A finished evaluation: the model takes it at once; once per evaluation.
    pub fn after_evaluation(self: &Arc<Self>, result: &Value) {
        let file = result["file"].as_str().unwrap_or("").to_string();
        {
            let mut i = self.inner.lock();
            if file == i.applied_evaluation {
                return;
            }
            i.applied_evaluation = file;
        }
        self.adopt_evaluation();
        self.models.evaluated();
        if self.inner.lock().matchup.phase == "idle" {
            self.models.rebuild();
        }
        self.request_build(60);
        if let Some(a) = self.auto_eval.lock().clone() {
            self.rt.spawn_blocking(move || a.check());
        }
    }

    /// The other computer's evaluation an import left: taken now when valid here.
    pub fn adopt_evaluation(self: &Arc<Self>) -> Option<String> {
        if !irelia::model::eval_travel::incoming_evaluation() {
            return None;
        }
        if self.jobs.running_job("evaluate").is_some() {
            return Some("its evaluation is checked once the evaluation running now ends".into());
        }
        let r = irelia::model::eval_travel::adopt_incoming_evaluation(&self.db.lock(), &self.data())?;
        eprintln!("[backup] {}", r.1);
        if r.0 {
            self.models.evaluated();
            if self.inner.lock().matchup.phase == "idle" {
                self.models.rebuild();
            }
            self.request_build(60);
            if let Some(a) = self.auto_eval.lock().clone() {
                self.rt.spawn_blocking(move || a.check());
            }
        }
        Some(r.1)
    }

    pub async fn evaluate(self: &Arc<Self>, kind: JobKind) -> Result<Value, String> {
        let size = { eval_size(&self.db.lock(), &self.data()) };
        let r = self.jobs.run(kind, Some(size)).await?;
        self.after_evaluation(&r);
        // What the next one would run is worked out again before the answer goes (0.22.0 did it in line).
        let auto = self.auto_eval.lock().clone();
        if let Some(a) = auto {
            let _ = tokio::task::spawn_blocking(move || a.check()).await;
        }
        Ok(r)
    }
}

/// What a build looks like in rec_log.
pub fn shown_build(b: &Value) -> Value {
    let first = |k: &str| b[k][0]["value"].clone();
    let items: Vec<i64> = ["slot4", "slot5", "slot6"].iter().filter_map(|s| b["situational"][s][0]["value"].as_i64()).collect();
    json!({
        "keystone": b["runes"]["value"]["perks"][0].clone(),
        "core": if b["coreBuild"][0]["value"].is_array() { b["coreBuild"][0]["value"].clone() } else { json!([]) },
        "boots": first("boots"),
        "items": items,
    })
}

struct AutoDeps(std::sync::Weak<App>);

impl AutoDeps {
    fn app(&self) -> Option<Arc<App>> {
        self.0.upgrade()
    }
}

impl AutoEvalDeps for AutoDeps {
    fn following(&self) -> bool {
        read_app_config().get("patchMemory").is_none()
    }
    fn quiet(&self) -> bool {
        self.app().map(|a| a.inner.lock().matchup.phase == "idle").unwrap_or(false)
    }
    fn busy(&self) -> bool {
        self.app().and_then(|a| a.extra.lock().clone()).map(|x| x.blocked_for_evaluation()).unwrap_or(false)
    }
    fn summary(&self) -> Option<EvalSummary> {
        read_evaluation()
    }
    fn game_mark(&self) -> (i64, i64) {
        self.app().map(|a| a.db.lock().game_mark()).unwrap_or((0, 0))
    }
    fn games_after_row(&self, row: i64) -> i64 {
        self.app().map(|a| a.db.lock().games_after_row(row)).unwrap_or(0)
    }
    fn game_versions(&self) -> Vec<VersionRow> {
        self.app().map(|a| a.db.lock().game_versions().unwrap_or_default()).unwrap_or_default()
    }
    fn plan(&self) -> anyhow::Result<EvalPlan> {
        let a = self.app().ok_or_else(|| anyhow::anyhow!("closing"))?;
        let data = a.data();
        let db = a.db.lock();
        plan_evaluation(&db, &data, now_ms() as f64, None, None, None)
    }
    fn running(&self) -> Option<Option<String>> {
        let a = self.app()?;
        a.jobs.running_job("evaluate").map(|i| i["auto"].as_str().map(String::from))
    }
    fn start(&self, why: String, path: &'static str, done: Box<dyn FnOnce(Result<(), String>) + Send>) {
        let Some(a) = self.app() else { return done(Err("closing".into())) };
        a.rt.clone().spawn(async move {
            let r = a.evaluate(JobKind::Evaluate { quick: false, auto: Some(why), path: "auto", planned: path }).await;
            let _ = tokio::task::spawn_blocking(move || done(r.map(|_| ()))).await;
        });
    }
    fn pause(&self) {
        if let Some(a) = self.app() {
            a.rt.clone().spawn(async move { a.jobs.cancel("evaluate", true).await });
        }
    }
    fn changed(&self, _status: &Value) {
        if let Some(a) = self.app() {
            a.broadcast(json!({ "type": "jobs", "status": a.jobs_status() }));
        }
    }
    fn log(&self, line: &str) {
        eprintln!("[auto-eval] {line}");
    }
}

#[allow(dead_code)]
fn _unused(_: ChampionTotals, _: &dyn Fn() -> Value) {
    let _ = default_scope();
}
