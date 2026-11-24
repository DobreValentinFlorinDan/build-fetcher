//! Port of `server/scraper/worker.ts` + `server/scraper/controller.ts`: all Riot API traffic. One lane (a tokio task)
//! per Riot host asks sourcing for its next job; the backfill rebuilds captures from the disk cache. Start/Stop is
//! persisted; champ select and games pause fetching (Settings → Pause fetching during games).

use irelia::app_config::read_app_config;
use irelia::capture::CAPTURE_VERSION;
use irelia::config::{config, CHAMPION_ID, RATE_SNAPSHOT_MS};
use irelia::db::{Db, OpenOptions};
use irelia::patches::Fallback;
use irelia::riot::client::{RiotAuthError, RiotClient};
use irelia::riot::disk_cache::DiskCache;
use irelia::riot::rate_limiter::{sleep, Signal};
use irelia::riot::routing::{is_platform, normalize_platform};
use irelia::scope::{default_scope, load_scope};
use irelia::scraper::account::{is_client_puuid, resolve_account, ClientAccount, Resolved};
use irelia::scraper::backfill::{run_backfill, BackfillEnv, BackfillProgress};
use irelia::scraper::sourcing::{lane_host, LaneKey, Sourcing, SourcingEnv};
use irelia::util::now_ms;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const IDLE_SLEEP_MS: i64 = 60_000;
const PAUSED: &str = "Paused during champ select and the game";

pub enum ScraperEvent {
    Status(Value),
    RateLimit(Value),
}

struct Lane {
    key: LaneKey,
    host: String,
    task: String,
    busy: bool,
    idle_until: Option<i64>,
    last_error: Option<String>,
    stop: Signal,
    wake: Arc<tokio::sync::Notify>,
}

/// One data folder's worth of scraper: its own connection, cache, client and sourcing.
struct Core {
    db: Arc<Mutex<Db>>,
    cache: Arc<Mutex<DiskCache>>,
    riot: Arc<RiotClient>,
    sourcing: Arc<Sourcing<RiotClient>>,
}

struct State {
    ddragon: Fallback,
    running: bool,
    enabled: bool,
    game_paused: bool,
    loop_abort: Signal,
    phase: String,
    last_error: Option<String>,
    last_event: Option<String>,
    backfill: Option<BackfillProgress>,
    backfill_abort: Option<Signal>,
    paused_backfill: Option<bool>,
    backfill_force: bool,
    lanes: indexmap::IndexMap<String, Lane>,
    me: Option<(String, String)>,
    client_account: Option<ClientAccount>,
    own_games_error: Option<String>,
    counts: Option<(i64, String, Value)>,
    last_snapshot_key: String,
    size_indexed: bool,
}

pub struct ScraperController {
    core: Mutex<Option<Arc<Core>>>,
    state: Mutex<State>,
    emit: Arc<dyn Fn(ScraperEvent) + Send + Sync>,
    pub status: Mutex<Option<Value>>,
    pub rate: Mutex<Option<Value>>,
    in_game: Mutex<(bool, bool)>,
    api_key: Mutex<String>,
}

fn lane_id(k: &LaneKey) -> String {
    match k {
        LaneKey::Platform(p) => format!("p:{p}"),
        LaneKey::Regional { region, .. } => format!("r:{region}"),
    }
}

fn err_text(e: &anyhow::Error) -> String {
    e.to_string()
}

impl ScraperController {
    pub fn new(patch: &str, previous_patch: &str, api_key: &str, emit: Arc<dyn Fn(ScraperEvent) + Send + Sync>) -> Arc<ScraperController> {
        let c = Arc::new(ScraperController {
            core: Mutex::new(None),
            state: Mutex::new(State {
                ddragon: Fallback { patch: patch.into(), previous_patch: previous_patch.into() },
                running: false,
                enabled: false,
                game_paused: false,
                loop_abort: Signal::new(),
                phase: "idle".into(),
                last_error: None,
                last_event: None,
                backfill: None,
                backfill_abort: None,
                paused_backfill: None,
                backfill_force: false,
                lanes: indexmap::IndexMap::new(),
                me: None,
                client_account: None,
                own_games_error: None,
                counts: None,
                last_snapshot_key: String::new(),
                size_indexed: false,
            }),
            emit,
            status: Mutex::new(None),
            rate: Mutex::new(None),
            in_game: Mutex::new((false, true)),
            api_key: Mutex::new(api_key.to_string()),
        });
        c.respawn();
        let me = c.clone();
        tokio::spawn(async move {
            let mut tick = 0u64;
            loop {
                tokio::time::sleep(Duration::from_millis(RATE_SNAPSHOT_MS)).await;
                tick += 1;
                if me.core.lock().is_none() {
                    continue;
                }
                me.post_rate_snapshot();
                if tick % (2000 / RATE_SNAPSHOT_MS) == 0 {
                    let (running, bf) = {
                        let st = me.state.lock();
                        (st.running, st.backfill_abort.is_some())
                    };
                    if running || bf {
                        me.report();
                    }
                }
            }
        });
        c
    }

    fn core(&self) -> Option<Arc<Core>> {
        self.core.lock().clone()
    }

    /// Starts the scraper on the current data folder; it resumes the background loop if it was enabled.
    pub fn respawn(self: &Arc<Self>) {
        if self.core.lock().is_some() {
            return;
        }
        let cfg = config();
        let db = match Db::open(&cfg.db_path, OpenOptions { migrate: false, busy_timeout_ms: 30_000 }) {
            Ok(db) => Arc::new(Mutex::new(db)),
            Err(e) => {
                eprintln!("[scraper] could not open the database: {e}");
                return;
            }
        };
        let cache = Arc::new(Mutex::new(DiskCache::new(&cfg.riot_cache_dir)));
        let key = { self.api_key.lock().clone() };
        let key = if key.is_empty() { cfg.riot_api_key.clone() } else { key };
        *self.api_key.lock() = key.clone();
        let riot = Arc::new(RiotClient::new(&key, cache.clone(), cfg.rate_limits.clone(), cfg.pacing));
        let weak = Arc::downgrade(self);
        let weak2 = weak.clone();
        let weak3 = weak.clone();
        let sourcing = Sourcing::new(SourcingEnv {
            db: db.clone(),
            riot: riot.clone(),
            scope: Arc::new(|| load_scope().unwrap_or_else(|_| default_scope())),
            fallback_patch: Arc::new(move || weak.upgrade().map(|c| c.state.lock().ddragon.clone()).unwrap_or(Fallback { patch: String::new(), previous_patch: String::new() })),
            me: Arc::new(move || weak2.upgrade().and_then(|c| c.state.lock().me.clone())),
            concurrency: cfg.pacing.concurrency,
            log: Arc::new(move |t| {
                if let Some(c) = weak3.upgrade() {
                    c.event(t);
                }
            }),
            now: None,
            recheck_pilots_ms: None,
            recheck_others_ms: None,
            reseed_ms: None,
            rescan_pilot_ms: None,
        });
        let weak4 = Arc::downgrade(self);
        riot.on_throttle(Some(Arc::new(move |_| {
            if let Some(c) = weak4.upgrade() {
                c.report();
            }
        })));
        let me = {
            let d = db.lock();
            let puuid = d.get_meta("me_puuid");
            let platform = d.get_meta("me_platform");
            match (puuid, platform) {
                (Some(p), Some(pl)) if !is_client_puuid(&p) && is_platform(&pl) => Some((p, pl)),
                _ => None,
            }
        };
        let enabled = db.lock().get_meta("scraper_enabled").as_deref() == Some("1");
        let pending = db.lock().count_pending_detail(CAPTURE_VERSION);
        *self.core.lock() = Some(Arc::new(Core { db, cache, riot, sourcing: sourcing.clone() }));
        let game_paused = self.game_paused();
        {
            let mut st = self.state.lock();
            st.me = me.clone();
            st.enabled = enabled;
            st.game_paused = game_paused;
            st.size_indexed = false;
        }
        self.report();
        if enabled && !game_paused {
            self.start_lanes();
        }
        if pending > 0 {
            self.start_backfill(false);
        }
        if me.is_some() {
            sourcing.request_own_games(&[0]);
            self.own_games_now();
        }
        if let Some(a) = self.state.lock().client_account.clone() {
            let s = self.clone();
            tokio::spawn(async move { s.set_me(Some(a)).await });
        }
        self.size_index_later();
    }

    /// Stops every Riot call, cache write and database write it owns (the data folder moves).
    pub async fn terminate(&self, what: &str) {
        {
            let mut st = self.state.lock();
            st.running = false;
            st.loop_abort.abort("paused");
            st.lanes.clear();
            if let Some(b) = st.backfill_abort.take() {
                b.abort("paused");
            }
        }
        // Give the lanes a moment to leave their transactions.
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Some(core) = self.core.lock().take() {
            core.cache.lock().close();
        }
        let s = {
            let mut s = self.status.lock();
            if let Some(v) = s.as_mut() {
                v["running"] = json!(false);
                v["phase"] = json!(format!("Paused while {what}"));
            }
            s.clone()
        };
        if let Some(s) = s {
            (self.emit)(ScraperEvent::Status(s));
        }
    }

    fn event(&self, msg: &str) {
        self.state.lock().last_event = Some(msg.to_string());
        self.report();
    }

    fn game_paused(&self) -> bool {
        let g = self.in_game.lock();
        g.0 && g.1
    }

    // ------------------------------------------------------------------------- lanes

    fn wake_all(&self) {
        for l in self.state.lock().lanes.values() {
            l.wake.notify_waiters();
        }
    }

    fn sync_lanes(self: &Arc<Self>) {
        let Some(core) = self.core() else { return };
        if !self.state.lock().running {
            return;
        }
        let wanted = core.sourcing.lanes();
        let ids: Vec<String> = wanted.iter().map(lane_id).collect();
        let mut start: Vec<String> = vec![];
        {
            let mut st = self.state.lock();
            let gone: Vec<String> = st.lanes.keys().filter(|k| !ids.contains(k)).cloned().collect();
            for k in gone {
                if let Some(l) = st.lanes.shift_remove(&k) {
                    l.stop.abort("stopped");
                }
            }
            for key in wanted {
                let id = lane_id(&key);
                if let Some(l) = st.lanes.get_mut(&id) {
                    l.key = key;
                    continue;
                }
                st.lanes.insert(id.clone(), Lane {
                    host: lane_host(&key),
                    key,
                    task: "Starting".into(),
                    busy: false,
                    idle_until: None,
                    last_error: None,
                    stop: Signal::new(),
                    wake: Arc::new(tokio::sync::Notify::new()),
                });
                start.push(id);
            }
        }
        for id in start {
            let me = self.clone();
            tokio::spawn(async move { me.run_lane(id).await });
        }
    }

    async fn run_lane(self: Arc<Self>, id: String) {
        let Some(core) = self.core() else { return };
        let (signal, wake) = {
            let st = self.state.lock();
            let Some(l) = st.lanes.get(&id) else { return };
            (Signal::any(&[&l.stop, &st.loop_abort]), l.wake.clone())
        };
        loop {
            if !self.state.lock().running || signal.aborted() {
                break;
            }
            let key = match self.state.lock().lanes.get(&id) {
                Some(l) => l.key.clone(),
                None => break,
            };
            let job = core.sourcing.next_job(&key);
            let job = match job {
                Ok(j) => j,
                Err(e) => {
                    self.lane_error(&id, &e, &signal).await;
                    continue;
                }
            };
            let Some(job) = job else {
                {
                    let mut st = self.state.lock();
                    let le = st.lanes.get(&id).and_then(|l| l.last_error.clone());
                    if le.is_some() && st.last_error == le {
                        st.last_error = None;
                    }
                    if let Some(l) = st.lanes.get_mut(&id) {
                        l.task = "Up to date".into();
                        l.busy = false;
                        l.idle_until = Some(now_ms() + IDLE_SLEEP_MS);
                        l.last_error = None;
                    }
                }
                self.report();
                let notified = wake.notified();
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(IDLE_SLEEP_MS as u64)) => {}
                    _ = notified => {}
                    _ = signal.cancelled() => {}
                }
                if let Some(l) = self.state.lock().lanes.get_mut(&id) {
                    l.idle_until = None;
                }
                self.sync_lanes();
                continue;
            };
            let failed = {
                let mut st = self.state.lock();
                let Some(l) = st.lanes.get_mut(&id) else { break };
                let failed = l.last_error.take();
                l.task = job.label.clone();
                l.busy = true;
                failed
            };
            self.report();
            match (job.run)(signal.clone()).await {
                Ok(()) => {
                    {
                        let mut st = self.state.lock();
                        if let Some(l) = st.lanes.get_mut(&id) {
                            l.busy = false;
                        }
                        if failed.is_some() && st.last_error == failed {
                            st.last_error = None;
                        }
                    }
                    self.report();
                    self.wake_all();
                    self.sync_lanes();
                }
                Err(e) => {
                    if let Some(l) = self.state.lock().lanes.get_mut(&id) {
                        l.busy = false;
                    }
                    if signal.aborted() {
                        break;
                    }
                    if e.downcast_ref::<RiotAuthError>().is_some() {
                        self.state.lock().last_error = Some(err_text(&e));
                        self.stop_lanes("Stopped: invalid API key");
                        if let Some(c) = self.core() {
                            let _ = c.db.lock().set_meta("scraper_enabled", "0");
                        }
                        self.state.lock().enabled = false;
                        break;
                    }
                    self.lane_error(&id, &e, &signal).await;
                }
            }
        }
    }

    async fn lane_error(&self, id: &str, e: &anyhow::Error, signal: &Signal) {
        let msg = err_text(e);
        {
            let mut st = self.state.lock();
            if let Some(l) = st.lanes.get_mut(id) {
                l.task = "Recovering from error".into();
                l.last_error = Some(msg.clone());
            }
            st.last_error = Some(msg);
        }
        self.report();
        let _ = sleep(5000.0, Some(signal)).await;
    }

    fn start_lanes(self: &Arc<Self>) {
        {
            let mut st = self.state.lock();
            if st.running {
                return;
            }
            st.running = true;
            st.last_error = None;
            st.loop_abort = Signal::new();
        }
        self.sync_lanes();
        self.report();
    }

    fn stop_lanes(&self, why: &str) {
        {
            let mut st = self.state.lock();
            st.running = false;
            st.loop_abort.abort("paused");
            st.lanes.clear();
            st.phase = why.into();
        }
        self.report();
    }

    fn lane_phase(st: &State) -> String {
        let busy: Vec<&Lane> = st.lanes.values().filter(|l| l.busy).collect();
        if !busy.is_empty() {
            return busy.iter().map(|l| l.task.clone()).collect::<Vec<_>>().join(" · ");
        }
        if !st.lanes.is_empty() && st.lanes.values().all(|l| l.idle_until.is_some()) {
            return "Up to date – no new work".into();
        }
        st.lanes.values().next().map(|l| l.task.clone()).unwrap_or_else(|| st.phase.clone())
    }

    // ------------------------------------------------------------------------- status

    fn rate_snapshot(&self, core: &Core) -> Value {
        let now = now_ms();
        let idle_until = {
            let st = self.state.lock();
            if st.running && !st.lanes.is_empty() && st.lanes.values().all(|l| l.idle_until.is_some()) {
                st.lanes.values().filter_map(|l| l.idle_until).min()
            } else {
                None
            }
        };
        let mut s = json!({ "at": now });
        if let Value::Object(m) = core.riot.rate_state(now as f64) {
            for (k, v) in m {
                s[k] = v;
            }
        }
        s["idleUntil"] = json!(idle_until);
        s
    }

    fn host_work(&self, core: &Core) -> Vec<Value> {
        let st = self.state.lock();
        st.lanes
            .values()
            .map(|l| {
                let (kind, platforms) = match &l.key {
                    LaneKey::Platform(p) => ("platform", vec![p.clone()]),
                    LaneKey::Regional { platforms, .. } => ("regional", platforms.clone()),
                };
                json!({
                    "host": l.host, "kind": kind, "platforms": platforms, "task": l.task, "busy": l.busy,
                    "queued": core.sourcing.queued(&l.key), "idleUntil": l.idle_until, "lastError": l.last_error,
                })
            })
            .collect()
    }

    fn counts(&self, core: &Core) -> (Value, String) {
        if let Some((at, patch, c)) = &self.state.lock().counts {
            if now_ms() - at <= 1000 {
                return (c.clone(), patch.clone());
            }
        }
        let first = load_scope().ok().and_then(|s| s.platforms.first().cloned()).unwrap_or_else(|| "KR".into());
        let patch = core.sourcing.region(&normalize_platform(&first).unwrap_or_else(|_| "KR".into())).patch;
        let db = core.db.lock();
        let c = db.counts(&patch).ok();
        let mut sources = serde_json::Map::new();
        for (k, n) in db.source_counts().unwrap_or_default() {
            sources.insert(k, json!(n));
        }
        drop(db);
        let v = json!({
            "players": c.as_ref().map(|c| c.players).unwrap_or(0), "otp": c.as_ref().map(|c| c.otp).unwrap_or(0),
            "mains": c.as_ref().map(|c| c.mains).unwrap_or(0), "matches": c.as_ref().map(|c| c.matches).unwrap_or(0),
            "games": c.as_ref().map(|c| c.games).unwrap_or(0), "current": c.as_ref().map(|c| c.current).unwrap_or(0), "sources": sources,
        });
        self.state.lock().counts = Some((now_ms(), patch.clone(), v.clone()));
        (v, patch)
    }

    pub fn report(&self) {
        let Some(core) = self.core() else { return };
        let (c, patch) = self.counts(&core);
        let rate = self.rate_snapshot(&core);
        let hosts = self.host_work(&core);
        let riot_id = core.db.lock().get_meta("me_riot_id");
        let st = self.state.lock();
        let paused_game = st.game_paused && st.enabled;
        let mut status = json!({
            "running": st.running,
            "phase": if st.running { Self::lane_phase(&st) } else if paused_game { PAUSED.to_string() } else { st.phase.clone() },
        });
        if paused_game {
            status["paused"] = json!("game");
        }
        let extra = json!({
            "requests": core.riot.requests.load(std::sync::atomic::Ordering::SeqCst),
            "rateWindow": core.riot.usage(),
            "rate": rate,
            "playersSeen": c["players"], "otpPlayers": c["otp"], "matchesScanned": c["matches"], "ireliaGames": c["games"],
            "currentPatchGames": c["current"], "currentPatch": patch, "lastError": st.last_error, "lastEvent": st.last_event,
            "mainPlayers": c["mains"], "sources": c["sources"], "hosts": hosts, "backfill": st.backfill.as_ref().map(|b| b.to_json()),
            "me": st.me.as_ref().map(|(_, platform)| {
                let mut m = json!({ "platform": platform, "games": c["sources"]["me"].as_i64().unwrap_or(0) });
                if let Some(r) = &riot_id {
                    m["riotId"] = json!(r);
                }
                m
            }),
        });
        drop(st);
        for (k, v) in extra.as_object().unwrap() {
            status[k] = v.clone();
        }
        *self.status.lock() = Some(status.clone());
        (self.emit)(ScraperEvent::Status(status));
    }

    fn post_rate_snapshot(&self) {
        let Some(core) = self.core() else { return };
        let s = self.rate_snapshot(&core);
        let hosts: Vec<Value> = s["hosts"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|h| {
                json!([
                    h["host"],
                    h["windows"].as_array().map(|w| w.iter().map(|x| format!("{}/{}/{}", x["used"], x["limit"], x["windowSec"])).collect::<Vec<_>>()),
                    irelia::js::round(h["paceMs"].as_f64().unwrap_or(0.0)),
                    h["nextAt"].as_f64().map(|n| irelia::js::round(n / 10.0)),
                    h["queued"], h["inFlight"], h["retryAfter"]["until"].as_f64().unwrap_or(0.0), h["retryAfter"]["scope"].as_str().unwrap_or(""),
                ])
            })
            .collect();
        let key = json!([hosts, s["busiest"], s["active"], s["idleUntil"]]).to_string();
        {
            let mut st = self.state.lock();
            if key == st.last_snapshot_key {
                return;
            }
            st.last_snapshot_key = key;
        }
        *self.rate.lock() = Some(s.clone());
        (self.emit)(ScraperEvent::RateLimit(s));
    }

    // ------------------------------------------------------------------------- backfill

    pub fn start_backfill(self: &Arc<Self>, force: bool) {
        let Some(core) = self.core() else { return };
        let signal = {
            let mut st = self.state.lock();
            if st.backfill_abort.is_some() {
                return;
            }
            if st.game_paused {
                st.paused_backfill = Some(st.paused_backfill.unwrap_or(false) || force);
                return;
            }
            let s = Signal::new();
            st.backfill_abort = Some(s.clone());
            st.backfill_force = force;
            s
        };
        let me = self.clone();
        tokio::task::spawn_blocking(move || {
            let sourcing = core.sourcing.clone();
            let region = move |p: &str| sourcing.region(p);
            let env = BackfillEnv {
                db: &core.db,
                cache: &core.cache,
                champion_id: CHAMPION_ID,
                harvest: load_scope().map(|s| s.harvest_irelias).unwrap_or(true),
                region: &region,
                me_puuid: me.state.lock().me.as_ref().map(|m| m.0.clone()),
            };
            let mut last = 0i64;
            let me2 = me.clone();
            let r = run_backfill(&env, &signal, &mut |p| {
                me2.state.lock().backfill = Some(p.clone());
                if now_ms() - last > 500 {
                    last = now_ms();
                    me2.report();
                }
            }, force, None);
            match r {
                Ok(b) => {
                    core.sourcing.invalidate();
                    me.state.lock().backfill = Some(b.clone());
                    let mut text = format!("Backfill: {} matches captured, {} Irelia timelines", b.done, b.games);
                    if b.harvested > 0 {
                        text += &format!(", {} harvested games queued", b.harvested);
                    }
                    if b.missing > 0 {
                        text += &format!(", {} not on disk", b.missing);
                    }
                    me.event(&text);
                    me.wake_all();
                }
                Err(e) => {
                    if !signal.aborted() {
                        let msg = err_text(&e);
                        let mut st = me.state.lock();
                        st.last_error = Some(format!("Backfill failed: {msg}"));
                        if let Some(b) = st.backfill.as_mut() {
                            b.phase = format!("Failed: {msg}");
                        }
                    }
                }
            }
            me.state.lock().backfill_abort = None;
            me.report();
        });
    }

    // ------------------------------------------------------------------------- the user's own games

    fn own_games_now(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            let Some(core) = me.core() else { return };
            let (running, paused, mine) = {
                let st = me.state.lock();
                (st.running, st.game_paused, st.me.clone())
            };
            let key = me.api_key.lock().clone();
            if running || paused || mine.is_none() || key.is_empty() || !load_scope().map(|s| s.own_games).unwrap_or(true) {
                return;
            }
            let (_, platform) = mine.unwrap();
            let signal = Signal::new();
            let r: anyhow::Result<()> = async {
                core.sourcing.list_own_games(&signal).await?;
                loop {
                    let batch: Vec<_> = core.db.lock().next_queued_batch(&[platform.clone()], config().pacing.concurrency as i64 * 4)?.into_iter().filter(|q| q.source == "me").collect();
                    if batch.is_empty() {
                        break;
                    }
                    core.sourcing.process_queued(batch, &signal).await?;
                }
                Ok(())
            }
            .await;
            {
                let mut st = me.state.lock();
                match r {
                    Ok(()) => {
                        if st.own_games_error.is_some() && st.last_error == st.own_games_error {
                            st.last_error = None;
                        }
                        st.own_games_error = None;
                    }
                    Err(e) => {
                        let t = err_text(&e);
                        st.last_error = Some(t.clone());
                        st.own_games_error = Some(t);
                    }
                }
            }
            me.report();
        });
    }

    /// The signed-in account (League client): its Riot ID becomes the Riot API's PUUID, then its games are fetched.
    pub async fn set_me(self: &Arc<Self>, account: Option<ClientAccount>) {
        if let Some(a) = account {
            self.state.lock().client_account = Some(a);
        }
        let Some(a) = self.state.lock().client_account.clone() else { return };
        let Some(core) = self.core() else { return };
        if self.api_key.lock().is_empty() {
            self.state.lock().last_error = Some("Add a Riot API key in Settings to fetch your own games".into());
            self.report();
            return;
        }
        let manual = read_app_config().get("myPlatform").and_then(|v| v.as_str().map(String::from));
        let r = match resolve_account(&*core.riot, &a, manual.as_deref(), &Signal::new()).await {
            Ok(r) => r,
            Err(e) => Resolved::Err(format!("Could not look up your account: {}", err_text(&e))),
        };
        let (puuid, platform, riot_id) = match r {
            Resolved::Err(e) => {
                self.state.lock().last_error = Some(e);
                self.report();
                return;
            }
            Resolved::Ok { puuid, platform, riot_id } => (puuid, platform, riot_id),
        };
        {
            let mut st = self.state.lock();
            if st.last_error.as_ref().map(|e| e.contains("your account") || e.contains("Riot ID") || e.contains("which server")).unwrap_or(false) {
                st.last_error = None;
            }
        }
        let _ = core.db.lock().set_meta("me_riot_id", &riot_id);
        if self.state.lock().me.as_ref() == Some(&(puuid.clone(), platform.clone())) {
            self.report();
            return;
        }
        {
            let db = core.db.lock();
            let _ = db.set_meta("me_puuid", &puuid);
            let _ = db.set_meta("me_platform", &platform);
            let _ = db.tag_own_games(&puuid);
        }
        self.state.lock().me = Some((puuid, platform));
        core.sourcing.request_own_games(&[0]);
        self.sync_lanes();
        self.wake_all();
        self.report();
        self.own_games_now();
    }

    pub fn game_ended(self: &Arc<Self>) {
        let delays = [2 * 60_000, 6 * 60_000, 15 * 60_000];
        if let Some(c) = self.core() {
            c.sourcing.request_own_games(&delays);
        }
        for d in delays {
            let me = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(d as u64 + 1000)).await;
                me.wake_all();
                me.own_games_now();
            });
        }
    }

    fn size_index_later(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let Some(core) = me.core() else { return };
            {
                let mut st = me.state.lock();
                if st.size_indexed || st.game_paused {
                    return;
                }
                st.size_indexed = true;
            }
            let r = tokio::task::spawn_blocking(move || core.cache.lock().ensure_size_index()).await;
            match r {
                Ok(Ok(true)) => eprintln!("[scraper] the cache's size index is built"),
                Ok(Ok(false)) => {}
                Ok(Err(e)) => {
                    me.state.lock().size_indexed = false;
                    eprintln!("[scraper] the cache's size index: {e}");
                }
                Err(_) => me.state.lock().size_indexed = false,
            }
        });
    }

    fn set_game_pause(self: &Arc<Self>, on: bool) {
        {
            let mut st = self.state.lock();
            if on == st.game_paused {
                return;
            }
            st.game_paused = on;
        }
        if on {
            let running = self.state.lock().running;
            if running {
                self.stop_lanes(PAUSED);
            }
            let mut st = self.state.lock();
            if let Some(b) = st.backfill_abort.clone() {
                st.paused_backfill = Some(st.backfill_force);
                b.abort("paused");
            }
            drop(st);
            self.report();
            return;
        }
        let (enabled, paused_bf) = {
            let mut st = self.state.lock();
            if st.phase == PAUSED {
                st.phase = "idle".into();
            }
            (st.enabled, st.paused_backfill.take())
        };
        if !self.state.lock().size_indexed {
            self.size_index_later();
        }
        if enabled && !self.api_key.lock().is_empty() {
            self.start_lanes();
        }
        if let Some(force) = paused_bf {
            self.start_backfill(force);
        }
        self.wake_all();
        self.own_games_now();
        self.report();
    }

    // ------------------------------------------------------------------------- the controller's API

    pub fn start(self: &Arc<Self>) {
        if self.core().is_none() {
            self.respawn();
        }
        if self.api_key.lock().is_empty() {
            self.state.lock().last_error = Some("RIOT_API_KEY is not set".into());
            self.report();
            return;
        }
        if let Some(c) = self.core() {
            let _ = c.db.lock().set_meta("scraper_enabled", "1");
        }
        self.state.lock().enabled = true;
        if self.state.lock().game_paused {
            self.report();
        } else {
            self.start_lanes();
        }
    }

    pub fn stop(self: &Arc<Self>) {
        if let Some(c) = self.core() {
            let _ = c.db.lock().set_meta("scraper_enabled", "0");
        }
        self.state.lock().enabled = false;
        self.stop_lanes("idle");
    }

    pub fn set_patch(&self, patch: &str, previous_patch: &str) {
        self.state.lock().ddragon = Fallback { patch: patch.into(), previous_patch: previous_patch.into() };
        if let Some(c) = self.core() {
            c.sourcing.invalidate();
        }
    }

    pub fn set_api_key(&self, key: &str) {
        *self.api_key.lock() = key.to_string();
        irelia::config::set_riot_api_key(key);
        if let Some(c) = self.core() {
            c.riot.set_api_key(key);
        }
        let mut st = self.state.lock();
        if st.last_error.as_ref().map(|e| e.contains("rejected the key")).unwrap_or(false) {
            st.last_error = None;
            drop(st);
            self.report();
        }
    }

    pub fn backfill(self: &Arc<Self>, force: bool) {
        self.start_backfill(force);
    }

    pub fn set_me_account(self: &Arc<Self>, account: ClientAccount) {
        let me = self.clone();
        tokio::spawn(async move { me.set_me(Some(account)).await });
    }

    pub fn retry_me(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move { me.set_me(None).await });
    }

    pub fn set_pause_in_game(self: &Arc<Self>, on: bool) {
        let in_game = self.in_game.lock().0;
        self.set_in_game(in_game, Some(on));
    }

    pub fn set_in_game(self: &Arc<Self>, in_game: bool, pause_in_game: Option<bool>) {
        let (was, now) = {
            let mut g = self.in_game.lock();
            let was = g.0 && g.1;
            g.0 = in_game;
            if let Some(p) = pause_in_game {
                g.1 = p;
            }
            (was, g.0 && g.1)
        };
        if was != now {
            self.set_game_pause(now);
        }
    }
}
