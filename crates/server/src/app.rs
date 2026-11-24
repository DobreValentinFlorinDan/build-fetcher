//! The app's state and its flows: the League client → activity and champ select → recommendations; the Live Client →
//! the live context → in-game advice; status for the dashboard; the realtime fan-out to `/ws`.

use crate::activity::{os_idle_seconds, ActivityDetector, IDLE_POLL_S};
use crate::game_mode::GameMode;
use crate::handles::Handles;
use crate::lcu::champ_select::ChampSelect;
use crate::lcu::connector::{LcuConnector, LcuEvent};
use crate::lcu::importer::{import_build, ImportResult, LcuApi, Owned};
use crate::lcu::live_client::{PerformanceTracker, END_AFTER_FAILURES, LIVE_PORT};
use crate::lcu::live_context::{live_context, material};
use crate::static_data::StaticSlot;
use bf_core::build::{Activity, BuildRecommendation, DraftContext, FetchStatus, LiveAdvice, LiveContext, ModelStatus, Patch, Role};
use bf_core::config::{LIVE_IDLE_POLL_MS, LIVE_POLL_MS};
use bf_core::ddragon::StaticData;
use bf_core::util::now_ms;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// In-game advice is asked again at least this often, items or not (the game state moves on).
const LIVE_ASK_EVERY_MS: i64 = 30_000;
/// The dashboard's status refresh.
pub const STATUS_EVERY_S: u64 = 5;

/// The League client as the dashboard sees it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LcuView {
    /// "searching" | "connecting" | "connected" | "disconnected"
    pub status: String,
    pub phase: String,
}

/// Champ select: the draft and the build for it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftState {
    pub champ_select: ChampSelect,
    /// The build for the draft (the previous one stays shown while the next is worked out).
    pub recommendation: Option<BuildRecommendation>,
    pub pending: bool,
    pub error: Option<String>,
}

/// The game, for the overlay: champions and items, never names.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveState {
    pub active: bool,
    /// Driven by POST /api/dev/simulate-live.
    pub simulated: bool,
    pub context: Option<LiveContext>,
    pub advice: Option<LiveAdvice>,
    pub error: Option<String>,
    /// CS, vision and gold against the lane opponent (live_client::PerformanceTracker).
    pub performance: Value,
}

/// GET /api/status and the periodic `status` event.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusView {
    pub version: &'static str,
    pub activity: Activity,
    pub lcu: LcuView,
    pub fetch: FetchStatus,
    pub model: ModelStatus,
    pub first_run_complete: bool,
    pub key_set: bool,
    pub static_ready: bool,
}

#[derive(Default)]
struct LiveAsk {
    material: String,
    at_ms: i64,
    in_flight: bool,
}

pub struct App {
    pub h: Handles,
    pub data: StaticSlot,
    pub events: tokio::sync::broadcast::Sender<String>,
    pub activity: ActivityDetector,
    /// Below-normal priority during champ select and games (None in tests: it changes the test process).
    pub game_mode: Option<GameMode>,
    pub lcu: Mutex<Option<Arc<LcuConnector>>>,
    /// The client's API for the import (the connector, or a fake in tests).
    pub lcu_api: RwLock<Option<Arc<dyn LcuApi>>>,
    lcu_view: Mutex<LcuView>,
    pub draft: Mutex<Option<DraftState>>,
    draft_gen: AtomicU64,
    /// Champ select's role, for a game whose Live Client data have no positions.
    last_role: Mutex<Option<Role>>,
    pub live: Mutex<Option<LiveState>>,
    live_ask: Mutex<LiveAsk>,
    live_running: Mutex<Option<Arc<AtomicBool>>>,
    sim_stop: Mutex<Option<Arc<AtomicBool>>>,
    owned: Mutex<Owned>,
}

impl App {
    pub fn new(h: Handles, data: StaticSlot, game_mode: Option<GameMode>) -> Arc<App> {
        let (events, _) = tokio::sync::broadcast::channel::<String>(256);
        Arc::new(App {
            h,
            data,
            events,
            activity: ActivityDetector::default(),
            game_mode,
            lcu: Mutex::new(None),
            lcu_api: RwLock::new(None),
            lcu_view: Mutex::new(LcuView { status: "searching".into(), phase: "None".into() }),
            draft: Mutex::new(None),
            draft_gen: AtomicU64::new(0),
            last_role: Mutex::new(None),
            live: Mutex::new(None),
            live_ask: Mutex::new(LiveAsk::default()),
            live_running: Mutex::new(None),
            sim_stop: Mutex::new(None),
            owned: Mutex::new(Owned::default()),
        })
    }

    pub fn broadcast(&self, ev: Value) {
        let _ = self.events.send(ev.to_string());
    }

    pub fn static_data(&self) -> Option<Arc<StaticData>> {
        self.data.read().clone()
    }

    pub fn patch(&self) -> Option<Patch> {
        self.static_data().and_then(|d| Patch::parse(&d.patch))
    }

    pub fn champion_name(&self, id: i64) -> String {
        self.static_data().and_then(|d| d.champion(id).map(|c| c.name.clone())).unwrap_or_else(|| format!("Champion {id}"))
    }

    pub fn lcu_view(&self) -> LcuView {
        self.lcu_view.lock().clone()
    }

    pub fn status(&self) -> StatusView {
        let settings = self.h.store.settings();
        StatusView {
            version: env!("CARGO_PKG_VERSION"),
            activity: self.activity.current(),
            lcu: self.lcu_view(),
            fetch: self.h.store.fetch_status(),
            model: self.h.learner.status(),
            first_run_complete: settings.first_run_complete,
            key_set: !self.h.keys.key_masked().is_empty(),
            static_ready: self.data.read().is_some(),
        }
    }

    // ------------------------------------------------------------------ start (not in tests)

    /// The League client, the OS idle time and the periodic status.
    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        let lcu = LcuConnector::new(
            bf_core::config::config().league_paths,
            Arc::new(move |ev| me.on_lcu_event(ev)),
        );
        *lcu.patch.lock() = self.patch();
        *self.lcu.lock() = Some(lcu.clone());
        *self.lcu_api.write() = Some(lcu.clone() as Arc<dyn LcuApi>);
        lcu.start();
        self.live_start();

        let me = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(IDLE_POLL_S));
            loop {
                tick.tick().await;
                if let Some(a) = me.activity.set_idle_seconds(os_idle_seconds()) {
                    me.activity_changed(a);
                }
            }
        });
        let me = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(STATUS_EVERY_S));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if me.events.receiver_count() > 0 {
                    let me2 = me.clone();
                    // The handles may touch the disk (storage sizes, checkpoints): off the async threads.
                    if let Ok(s) = tokio::task::spawn_blocking(move || me2.status()).await {
                        me.broadcast(json!({ "type": "status", "status": s }));
                    }
                }
            }
        });
    }

    /// Data Dragon arrived: the patch goes into drafts.
    pub fn static_ready(&self) {
        if let Some(l) = self.lcu.lock().as_ref() {
            *l.patch.lock() = self.patch();
        }
        self.broadcast(json!({ "type": "static", "ready": true }));
    }

    // ------------------------------------------------------------------ activity

    /// One place every activity change goes through: the dashboard, the store's scheduler, the learner, game mode.
    pub fn activity_changed(&self, a: Activity) {
        crate::bf_log!("[activity] {a:?}");
        self.broadcast(json!({ "type": "activity", "activity": a }));
        self.h.store.set_activity(a);
        self.h.learner.set_activity(a);
        if let Some(g) = &self.game_mode {
            g.set(matches!(a, Activity::ChampSelect | Activity::InGame));
        }
    }

    // ------------------------------------------------------------------ the League client

    pub fn on_lcu_event(self: &Arc<Self>, ev: LcuEvent) {
        match ev {
            LcuEvent::Account(a) => self.h.store.set_account(&a),
            LcuEvent::GameEnded => self.h.store.game_ended(),
            LcuEvent::State(state) => {
                let view = LcuView {
                    status: state["status"].as_str().unwrap_or("searching").into(),
                    phase: state["phase"].as_str().unwrap_or("None").into(),
                };
                let changed = {
                    let mut v = self.lcu_view.lock();
                    std::mem::replace(&mut *v, view.clone()) != view
                };
                if changed {
                    crate::bf_log!("[client] {} | {}", view.status, view.phase);
                    self.broadcast(json!({ "type": "lcu", "state": view }));
                }
                if let Some(a) = self.activity.set_phase(&view.phase) {
                    self.activity_changed(a);
                }
                let cs: Option<ChampSelect> = serde_json::from_value(state["champSelect"].clone()).ok();
                self.on_champ_select(cs);
            }
        }
    }

    // ------------------------------------------------------------------ champ select

    /// A new draft (or none): shown at once, its build worked out in the background and shown when ready.
    pub fn on_champ_select(self: &Arc<Self>, cs: Option<ChampSelect>) {
        let Some(cs) = cs else {
            if self.draft.lock().take().is_some() {
                self.draft_gen.fetch_add(1, Ordering::SeqCst);
                self.broadcast(json!({ "type": "draft", "draft": null }));
            }
            return;
        };
        let state = {
            let mut d = self.draft.lock();
            if d.as_ref().is_some_and(|d| d.champ_select == cs) {
                return;
            }
            let previous = d.as_ref().and_then(|d| d.recommendation.clone()).filter(|r| r.champion == cs.draft.champion);
            let next = DraftState { champ_select: cs.clone(), recommendation: previous, pending: cs.draft.champion != 0, error: None };
            *d = Some(next.clone());
            next
        };
        if cs.draft.role.is_some() {
            *self.last_role.lock() = cs.draft.role;
        }
        self.broadcast(json!({ "type": "draft", "draft": state }));
        if cs.draft.champion == 0 {
            return;
        }
        let gen = self.draft_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let me = self.clone();
        let ctx = cs.draft.clone();
        tokio::spawn(async move {
            let result = me.recommend(ctx).await;
            if me.draft_gen.load(Ordering::SeqCst) != gen {
                return; // a newer draft took over
            }
            let state = {
                let mut d = me.draft.lock();
                let Some(s) = d.as_mut() else { return };
                s.pending = false;
                match result {
                    Ok(r) => {
                        if s.champ_select.draft.role.is_none() {
                            *me.last_role.lock() = Some(r.role);
                        }
                        s.recommendation = Some(r);
                        s.error = None;
                    }
                    Err(e) => s.error = Some(e.to_string()),
                }
                s.clone()
            };
            me.broadcast(json!({ "type": "draft", "draft": state }));
        });
    }

    /// BuildSource::recommend off the async threads (the learner may compute).
    pub async fn recommend(&self, ctx: DraftContext) -> anyhow::Result<BuildRecommendation> {
        let builds = self.h.builds.clone();
        tokio::task::spawn_blocking(move || builds.recommend(&ctx)).await?
    }

    /// The build for a champion (and role) imported into the client: the draft's own context when the champion is the
    /// one in champ select, so the import matches what is shown.
    pub async fn import(&self, champion: i64, role: Option<Role>) -> anyhow::Result<ImportResult> {
        let api = self.lcu_api.read().clone().ok_or_else(|| anyhow::anyhow!("The League client is not connected."))?;
        let ctx = self
            .draft
            .lock()
            .as_ref()
            .map(|d| d.champ_select.draft.clone())
            .filter(|d| d.champion == champion)
            .map(|mut d| {
                d.role = role.or(d.role);
                d
            })
            .unwrap_or(DraftContext { champion, role, patch: self.patch(), ..Default::default() });
        let rec = self.recommend(ctx).await?;
        let owned = self.owned.lock().clone();
        let r = import_build(&*api, &rec, &self.champion_name(champion), &owned).await;
        *self.owned.lock() = r.owned.clone();
        crate::bf_log!("[import] {}: rune page {}, item set {}", r.title, ok_str(r.rune_page.ok), ok_str(r.item_set.ok));
        Ok(r)
    }

    // ------------------------------------------------------------------ the live game

    /// A Live Client snapshot: the context at once, advice when something that matters changed (or every 30 s).
    pub fn on_live_data(self: &Arc<Self>, raw: &Value, performance: Value, simulated: bool) {
        if let Some(a) = self.activity.set_live_game(true) {
            self.activity_changed(a);
        }
        let ctx = self.static_data().and_then(|d| live_context(raw, &d, *self.last_role.lock()));
        let state = {
            let mut l = self.live.lock();
            let advice = l.as_ref().and_then(|s| s.advice.clone());
            let s = LiveState { active: true, simulated, context: ctx.clone(), advice, error: l.as_ref().and_then(|s| s.error.clone()), performance };
            *l = Some(s.clone());
            s
        };
        self.broadcast(json!({ "type": "live", "state": state }));
        let Some(ctx) = ctx else { return };
        let ask = {
            let mut a = self.live_ask.lock();
            let m = material(&ctx);
            let due = a.material != m || now_ms() - a.at_ms >= LIVE_ASK_EVERY_MS;
            if a.in_flight || !due {
                false
            } else {
                *a = LiveAsk { material: m, at_ms: now_ms(), in_flight: true };
                true
            }
        };
        if !ask {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let builds = me.h.builds.clone();
            let r = tokio::task::spawn_blocking(move || builds.live(&ctx)).await.map_err(anyhow::Error::from).and_then(|r| r);
            me.live_ask.lock().in_flight = false;
            let state = {
                let mut l = me.live.lock();
                let Some(s) = l.as_mut().filter(|s| s.active) else { return };
                match r {
                    Ok(a) => {
                        s.advice = Some(a);
                        s.error = None;
                    }
                    Err(e) => s.error = Some(e.to_string()),
                }
                s.clone()
            };
            me.broadcast(json!({ "type": "live", "state": state }));
        });
    }

    pub fn on_live_ended(&self) {
        *self.live.lock() = None;
        *self.live_ask.lock() = LiveAsk::default();
        self.broadcast(json!({ "type": "live", "state": { "active": false } }));
        if let Some(a) = self.activity.set_live_game(false) {
            self.activity_changed(a);
        }
    }

    /// Polls the Live Client Data API: every second during a game, every 3 s otherwise (it answers only in a game);
    /// a game ends after three failed polls in a row.
    pub fn live_start(self: &Arc<Self>) {
        let mut running = self.live_running.lock();
        if running.is_some() {
            return;
        }
        let flag = Arc::new(AtomicBool::new(true));
        *running = Some(flag.clone());
        drop(running);
        let me = self.clone();
        tokio::spawn(async move {
            let mut tracker = PerformanceTracker::default();
            let mut active = false;
            let mut failures = 0u32;
            while flag.load(Ordering::SeqCst) {
                let r = crate::lcu::https::loopback_json(LIVE_PORT, "/liveclientdata/allgamedata", None, 2500).await;
                match r {
                    Ok(raw) if raw["allPlayers"].as_array().is_some_and(|a| !a.is_empty()) => {
                        if !active {
                            crate::bf_log!("[live] game data at {:.0} s", raw["gameData"]["gameTime"].as_f64().unwrap_or(0.0));
                        }
                        active = true;
                        failures = 0;
                        // A real game ends a simulation.
                        if let Some(s) = me.sim_stop.lock().take() {
                            s.store(false, Ordering::SeqCst);
                        }
                        let perf = me.performance(&mut tracker, &raw);
                        me.on_live_data(&raw, perf, false);
                    }
                    _ => {
                        failures += 1;
                        if failures >= END_AFTER_FAILURES && active {
                            crate::bf_log!("[live] game data ended");
                            active = false;
                            tracker.reset();
                            if me.sim_stop.lock().is_none() {
                                me.on_live_ended();
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(if active { LIVE_POLL_MS } else { LIVE_IDLE_POLL_MS })).await;
            }
        });
    }

    fn performance(&self, tracker: &mut PerformanceTracker, raw: &Value) -> Value {
        let data = self.static_data();
        let price = |id: i64| data.as_ref().and_then(|d| d.item(id).map(|i| i.gold_total)).unwrap_or(0.0);
        tracker.update(raw, &price, None).unwrap_or(Value::Null)
    }

    /// POST /api/dev/simulate-live: drives the live context, the advice and the overlay without League.
    pub fn simulate_live(self: &Arc<Self>, start_at: f64, lane: &'static str, champion: String) {
        self.stop_simulation();
        let flag = Arc::new(AtomicBool::new(true));
        *self.sim_stop.lock() = Some(flag.clone());
        let me = self.clone();
        tokio::spawn(async move {
            let mut game = crate::lcu::simulate_live::SimulatedGame::new(start_at, lane, &champion, true);
            let mut tracker = PerformanceTracker::default();
            while flag.load(Ordering::SeqCst) {
                let raw = game.tick();
                let perf = me.performance(&mut tracker, &raw);
                me.on_live_data(&raw, perf, true);
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        });
    }

    pub fn stop_simulation(&self) -> bool {
        match self.sim_stop.lock().take() {
            Some(f) => {
                f.store(false, Ordering::SeqCst);
                self.on_live_ended();
                true
            }
            None => false,
        }
    }
}

fn ok_str(ok: bool) -> &'static str {
    if ok {
        "imported"
    } else {
        "not imported"
    }
}
