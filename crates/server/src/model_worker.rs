//! Port of `server/model/worker.ts` + `server/model/client.ts`: the model thread. The lane baseline, the draft-aware
//! model and everything read from it run here, so HTTP, /ws, the League client and the live poll never wait on a
//! model build or an estimate. Builds run on a helper thread with their own connection; requests are answered with
//! the model as it is meanwhile.

use irelia::config::{config, STARTING_ITEM_WINDOW_MS};
use irelia::db::{ChampionTotals, Db, IreliaGameRow, OpenOptions};
use irelia::ddragon::StaticData;
use irelia::engine::adaptive::identify;
use irelia::engine::exclusivity::{build_exclusivity_index, is_purchasable_item, ExclusivityIndex};
use irelia::engine::recommend::{recommend, EngineSettings, ValidatedRows};
use irelia::js;
use irelia::model::buy::{BuyIndex, BuyQuery};
use irelia::model::dataset::{PreparedGame, ScoredGame};
use irelia::model::draft::DraftQuery;
use irelia::model::facts::{matchup_best, matchup_facts};
use irelia::model::live::{LiveHint, LiveReranker};
use irelia::model::personal::{personal_report, review_game};
use irelia::model::run_eval::{model_patches, region_patch_fn};
use irelia::model::service::{current_mode, read_settings, with_switches, Built, BuildJob, ModelEnv, ModelService};
use irelia::model::weights::{PatchAger, RegionPatchFn, MEMORY_PATCHES};
use irelia::patches::{hotfix_json, hotfixes, region_json, region_patches, Fallback};
use irelia::progress::{last_timing, record_timing, Progress, Timing};
use irelia::scope::{default_scope, load_scope, ScrapeScope};
use irelia::types::BuildRecommendation;
use irelia::util::now_ms;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

const IRELIA: i64 = 39;

/// What the app asks the model thread.
#[derive(Clone, Debug)]
pub enum Request {
    Draft { lane: &'static str, opponent: Option<i64>, enemies: Vec<i64>, allies: Vec<i64>, regions: Option<Vec<String>> },
    Recommendation { enemy: Option<i64>, lane: Option<&'static str> },
    Live { raw: Value, hint: LiveHint, plan_target: Option<(i64, bool)> },
    Personal { lane: Option<&'static str> },
    Review { match_id: String },
    Status,
    Stats,
}

pub enum ModelEvent {
    /// ModelStatus as JSON, the champion profile rows after a build.
    Status(Value, Option<Arc<(Vec<ChampionTotals>, Vec<ChampionTotals>)>>),
    /// The stored games changed: the baseline may have too.
    Data,
}

type Reply = tokio::sync::oneshot::Sender<Result<Value, String>>;

enum Msg {
    Request(Request, Reply),
    Env(EnvPatch),
    Patch(Arc<StaticData>),
    Evaluated,
    Rebuild,
    Release,
    LiveReset,
    Options,
    BuildDone(BuildJob, Box<anyhow::Result<Built>>),
    Stop,
    /// Tests only: the thread stops as an unexpected crash would.
    Crash,
}

#[derive(Clone, Debug, Default)]
pub struct EnvPatch {
    pub own_platform: Option<Option<String>>,
    pub busy: Option<bool>,
    pub queued: Option<bool>,
    pub evaluating: Option<bool>,
}

#[derive(Default)]
struct Flags {
    busy: AtomicBool,
    queued: AtomicBool,
    evaluating: AtomicBool,
    has_model: AtomicBool,
}

pub type Emit = Arc<dyn Fn(ModelEvent) + Send + Sync>;

/// The handle the app holds (client.ts ModelClient).
pub struct ModelClient {
    tx: Mutex<Option<Sender<Msg>>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    pub status: Arc<Mutex<Value>>,
    env: Mutex<EnvPatch>,
    data: Mutex<Arc<StaticData>>,
    emit: Emit,
    paused: AtomicBool,
}

const RESTARTING: &str = "The model is restarting. Try again in a moment.";

fn initial_status() -> Value {
    json!({ "state": "loading", "info": null, "mode": "baseline", "error": null })
}

impl ModelClient {
    pub fn new(data: Arc<StaticData>, emit: Emit) -> Arc<ModelClient> {
        let c = Arc::new(ModelClient {
            tx: Mutex::new(None),
            thread: Mutex::new(None),
            status: Arc::new(Mutex::new(initial_status())),
            env: Mutex::new(EnvPatch::default()),
            data: Mutex::new(data),
            emit,
            paused: AtomicBool::new(false),
        });
        c.respawn();
        c
    }

    /// Starts the thread on the current data folder (after a move, or after Slim stored data).
    pub fn respawn(self: &Arc<Self>) {
        self.paused.store(false, Ordering::SeqCst);
        if self.tx.lock().is_some() {
            return;
        }
        let (tx, rx) = channel::<Msg>();
        let status = self.status.clone();
        let emit = self.emit.clone();
        let wrapped: Emit = Arc::new(move |ev: ModelEvent| {
            if let ModelEvent::Status(s, rows) = &ev {
                if let Some(b) = s.get("__built") {
                    record_timing("model", Timing { size: b["games"].as_f64().unwrap_or(0.0), ms: b["ms"].as_f64().unwrap_or(0.0), at: now_ms() as f64, ..Default::default() });
                }
                let mut clean = s.clone();
                if let Some(o) = clean.as_object_mut() {
                    o.remove("__built");
                }
                *status.lock() = clean.clone();
                emit(ModelEvent::Status(clean, rows.clone()));
                return;
            }
            emit(ev);
        });
        let data = self.data.lock().clone();
        let env = self.env.lock().clone();
        let tx2 = tx.clone();
        let handle = std::thread::Builder::new()
            .name("model".into())
            .spawn(move || {
                // A crash restarts the thread's state (the worker restarted itself in 0.22.0), waiting longer after
                // each failure in a row, up to a minute.
                let mut failures = 0u32;
                loop {
                    let started = Instant::now();
                    let r = std::panic::catch_unwind(AssertUnwindSafe(|| match Worker::start(data.clone(), wrapped.clone(), tx2.clone()) {
                        Ok(mut w) => {
                            w.apply_env(&env);
                            w.run(&rx);
                        }
                        Err(e) => {
                            eprintln!("[model] could not start: {e}");
                            let mut s = initial_status();
                            s["error"] = json!(e.to_string());
                            wrapped(ModelEvent::Status(s, None));
                        }
                    }));
                    match r {
                        Ok(()) => return,
                        Err(p) => {
                            if started.elapsed() > Duration::from_secs(60) {
                                failures = 0;
                            }
                            let wait = Duration::from_millis((1000u64 << failures.min(6)).min(60_000));
                            failures += 1;
                            eprintln!("[model] the model thread stopped ({}); restarting in {} s", panic_text(&p), wait.as_secs());
                            wrapped(ModelEvent::Status(initial_status(), None));
                            // Until it is back, requests fail at once; the other messages wait for the new state.
                            let until = Instant::now() + wait;
                            let mut kept = vec![];
                            loop {
                                let left = until.saturating_duration_since(Instant::now());
                                if left.is_zero() {
                                    break;
                                }
                                match rx.recv_timeout(left) {
                                    Ok(Msg::Request(_, reply)) => {
                                        let _ = reply.send(Err(RESTARTING.into()));
                                    }
                                    Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                                    Ok(Msg::BuildDone(..)) | Ok(Msg::Crash) => {}
                                    Ok(m) => kept.push(m),
                                    Err(RecvTimeoutError::Timeout) => break,
                                }
                            }
                            for m in kept {
                                let _ = tx2.send(m);
                            }
                        }
                    }
                }
            })
            .expect("model thread");
        *self.tx.lock() = Some(tx);
        *self.thread.lock() = Some(handle);
    }

    /// Stops the thread (the data folder is moving): its database connection closes with it.
    pub fn terminate(&self) {
        self.paused.store(true, Ordering::SeqCst);
        if let Some(tx) = self.tx.lock().take() {
            let _ = tx.send(Msg::Stop);
        }
        if let Some(h) = self.thread.lock().take() {
            let _ = h.join();
        }
        let mut s = self.status.lock();
        s["state"] = json!("loading");
    }

    fn post(&self, m: Msg) {
        if let Some(tx) = self.tx.lock().as_ref() {
            let _ = tx.send(m);
        }
    }

    pub async fn request(&self, req: Request, timeout_ms: u64) -> Result<Value, String> {
        let (otx, orx) = tokio::sync::oneshot::channel();
        {
            let g = self.tx.lock();
            let Some(tx) = g.as_ref() else { return Err(RESTARTING.into()) };
            if tx.send(Msg::Request(req, otx)).is_err() {
                return Err(RESTARTING.into());
            }
        }
        match tokio::time::timeout(Duration::from_millis(timeout_ms), orx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("The model worker stopped".into()),
            Err(_) => Err("The model is busy (building from new data). Try again in a moment.".into()),
        }
    }

    pub fn set_env(&self, patch: EnvPatch) {
        let mut env = self.env.lock();
        let mut changed = false;
        if let Some(v) = &patch.own_platform {
            if env.own_platform.as_ref() != Some(v) {
                env.own_platform = Some(v.clone());
                changed = true;
            }
        }
        macro_rules! flag {
            ($f:ident) => {
                if let Some(v) = patch.$f {
                    if env.$f != Some(v) {
                        env.$f = Some(v);
                        changed = true;
                    }
                }
            };
        }
        flag!(busy);
        flag!(queued);
        flag!(evaluating);
        drop(env);
        if changed {
            self.post(Msg::Env(patch));
        }
    }
    pub fn set_patch(&self, data: Arc<StaticData>) {
        *self.data.lock() = data.clone();
        self.post(Msg::Patch(data));
    }
    pub fn evaluated(&self) {
        self.post(Msg::Evaluated);
    }
    pub fn rebuild(&self) {
        self.post(Msg::Rebuild);
    }
    pub fn release(&self) {
        self.post(Msg::Release);
    }
    pub fn live_reset(&self) {
        self.post(Msg::LiveReset);
    }
    /// A Data tab switch was saved: returns the status with the switches as saved now.
    pub fn options_changed(&self) -> Value {
        self.post(Msg::Options);
        let mut s = self.status.lock();
        if !s["info"].is_null() {
            let info = with_switches(s["info"].clone());
            s["info"] = info;
        }
        s.clone()
    }
    pub fn status(&self) -> Value {
        self.status.lock().clone()
    }
    /// Tests only: the model thread crashes, as a bug would make it; it restarts itself.
    #[doc(hidden)]
    pub fn crash_for_tests(&self) {
        self.post(Msg::Crash);
    }
}

#[derive(Clone)]
struct Recent {
    scored: Arc<Vec<ScoredGame>>,
    prepared: Arc<Vec<Arc<PreparedGame>>>,
}

struct BaselineRows {
    version: String,
    rows: Vec<Arc<IreliaGameRow>>,
    used_at: i64,
    validated: parking_lot::Mutex<HashMap<String, Arc<ValidatedRows>>>,
}

const MAX_BASELINES: usize = 32;
const ROWS_IDLE_MS: i64 = 3 * 60_000;

struct Worker {
    db: Db,
    data: Arc<StaticData>,
    excl: Arc<ExclusivityIndex>,
    own_platform: Option<String>,
    flags: Arc<Flags>,
    region_cache: Option<(i64, RegionPatchFn)>,
    service: ModelService,
    reranker: LiveReranker,
    buy_index: Option<(i64, String, Arc<BuyIndex>)>,
    recent_cache: Option<((i64, String), i64, Recent)>,
    baselines: (String, indexmap::IndexMap<String, Arc<BuildRecommendation>>),
    baseline_rows: Option<BaselineRows>,
    state: &'static str,
    building: Option<Progress>,
    last_build_ms: Option<f64>,
    build_running: bool,
    build_again: bool,
    build_at: Option<Instant>,
    emit: Emit,
    tx: Sender<Msg>,
    seen: String,
    next_check: Instant,
}

fn scope() -> ScrapeScope {
    load_scope().unwrap_or_else(|_| default_scope())
}

impl Worker {
    fn start(data: Arc<StaticData>, emit: Emit, tx: Sender<Msg>) -> anyhow::Result<Worker> {
        let db = Db::open(&config().db_path, OpenOptions { migrate: false, ..Default::default() })?;
        let excl = Arc::new(build_exclusivity_index(&data));
        let seen = db.data_version();
        let w = Worker {
            db,
            data,
            excl,
            own_platform: None,
            flags: Arc::new(Flags::default()),
            region_cache: None,
            service: ModelService::default(),
            reranker: LiveReranker::default(),
            buy_index: None,
            recent_cache: None,
            baselines: (String::new(), indexmap::IndexMap::new()),
            baseline_rows: None,
            state: "loading",
            building: None,
            last_build_ms: last_timing("model").map(|t| t.ms),
            build_running: false,
            build_again: false,
            build_at: None,
            emit,
            tx,
            seen,
            next_check: Instant::now() + Duration::from_secs(30),
        };
        Ok(w)
    }

    fn held(&self) -> bool {
        self.flags.busy.load(Ordering::SeqCst) || self.flags.queued.load(Ordering::SeqCst) || self.flags.evaluating.load(Ordering::SeqCst)
    }

    fn apply_env(&mut self, e: &EnvPatch) {
        if let Some(p) = &e.own_platform {
            self.own_platform = p.clone();
        }
        if let Some(b) = e.busy {
            self.flags.busy.store(b, Ordering::SeqCst);
        }
        if let Some(b) = e.queued {
            self.flags.queued.store(b, Ordering::SeqCst);
        }
        if let Some(b) = e.evaluating {
            self.flags.evaluating.store(b, Ordering::SeqCst);
        }
    }

    fn region_patch_of(&mut self) -> RegionPatchFn {
        if let Some((at, f)) = &self.region_cache {
            if now_ms() - at < 30_000 {
                return f.clone();
            }
        }
        let f = region_patch_fn(&self.db, &self.data).unwrap_or_else(|_| Arc::new(|_: &str| None));
        self.region_cache = Some((now_ms(), f.clone()));
        f
    }

    fn env(&mut self) -> ModelEnv {
        ModelEnv {
            data: self.data.clone(),
            excl: self.excl.clone(),
            scope: scope(),
            region_patch: self.region_patch_of(),
            own_platform: self.own_platform.clone(),
            busy: self.held(),
            starting_item_window_ms: STARTING_ITEM_WINDOW_MS,
        }
    }

    /// The model's games from each region's current and previous patch (with patch memory, the model holds more):
    /// what the matchup facts, the high-elo timings and the best buy describe. Kept per model build.
    fn recent(&mut self) -> Recent {
        let built_at = self.service.current().map(|b| b.built_at).unwrap_or(-1);
        let key = (built_at, self.data.version.clone());
        if let Some((k, at, r)) = &self.recent_cache {
            if *k == key && now_ms() - at < 5 * 60_000 {
                return r.clone();
            }
        }
        let (scored, prepared) = match self.service.current() {
            Some(b) => (b.model.scored.clone(), b.prepared.clone()),
            None => (vec![], vec![]),
        };
        let r = if !self.service.holds_older_patches() {
            Recent { scored: Arc::new(scored), prepared: Arc::new(prepared) }
        } else {
            let ager = PatchAger::new(self.data.clone(), self.region_patch_of());
            Recent {
                scored: Arc::new(scored.into_iter().filter(|x| ager.age(&x.g.platform, &x.g.patch) <= 1.0).collect()),
                prepared: Arc::new(prepared.into_iter().filter(|g| ager.age(&g.platform, &g.patch) <= 1.0).collect()),
            }
        };
        self.recent_cache = Some((key, now_ms(), r.clone()));
        r
    }

    fn buys(&mut self) -> Arc<BuyIndex> {
        let built_at = self.service.current().map(|b| b.built_at).unwrap_or(0);
        let fresh = match &self.buy_index {
            Some((b, v, _)) => *b != built_at || *v != self.data.version,
            None => true,
        };
        if fresh {
            let state = self.service.current().and_then(|b| b.model.state.clone());
            let window = self.service.current().map(|b| b.model.window).unwrap_or(false);
            let scored = self.recent().scored;
            self.buy_index = Some((built_at, self.data.version.clone(), Arc::new(BuyIndex::new(scored, state, self.data.clone()).with_window(window))));
        }
        self.buy_index.as_ref().unwrap().2.clone()
    }

    fn baseline(&mut self, enemy: Option<i64>, lane: Option<&'static str>) -> anyhow::Result<Arc<BuildRecommendation>> {
        let sources = scope().build_sources;
        let version = format!("{}|{}|{}", self.db.data_version(), self.data.version, sources.join(","));
        if self.baselines.0 != version {
            self.baselines = (version.clone(), indexmap::IndexMap::new());
        }
        let opponent = enemy.filter(|e| self.data.champions.contains_key(e));
        let key = format!("{}|{}", lane.unwrap_or("null"), opponent.map(|o| o.to_string()).unwrap_or_else(|| "null".into()));
        if let Some(b) = self.baselines.1.get(&key) {
            return Ok(b.clone());
        }
        if self.baseline_rows.as_ref().map(|r| r.version != version).unwrap_or(true) {
            let patches: Vec<String> = self.db.patch_breakdown(&sources)?.into_iter().map(|p| p.0).collect();
            let rows = self.db.games_for_patches(&patches, &sources)?.into_iter().map(Arc::new).collect();
            self.baseline_rows = Some(BaselineRows { version: version.clone(), rows, used_at: 0, validated: Default::default() });
        }
        let rp = self.region_patch_of();
        let br = self.baseline_rows.as_mut().unwrap();
        br.used_at = now_ms();
        let b = Arc::new(recommend(&br.rows, &self.data, &self.excl, opponent, &EngineSettings::default(), lane, &*rp, Some(&br.validated)));
        if self.baselines.1.len() >= MAX_BASELINES {
            self.baselines.1.shift_remove_index(0);
        }
        self.baselines.1.insert(key, b.clone());
        Ok(b)
    }

    fn trim_rows(&mut self) -> bool {
        match &self.baseline_rows {
            Some(r) if now_ms() - r.used_at >= ROWS_IDLE_MS => {
                self.baseline_rows = None;
                true
            }
            _ => false,
        }
    }

    fn status(&mut self) -> Value {
        let env = self.env();
        let mut s = json!({
            "state": self.state,
            "info": self.service.info(&env),
            "mode": current_mode(),
            "error": self.service.error(),
        });
        if let Some(p) = &self.building {
            s["progress"] = p.info(None);
        }
        s
    }

    fn send_status(&mut self, profiles: Option<Arc<(Vec<ChampionTotals>, Vec<ChampionTotals>)>>, built: Option<(f64, usize)>) {
        let mut s = self.status();
        if let Some((ms, games)) = built {
            s["__built"] = json!({ "ms": ms, "games": games });
        }
        (self.emit)(ModelEvent::Status(s, profiles));
    }

    fn build(&mut self) {
        if self.build_running {
            self.build_again = true;
            return;
        }
        self.build_running = true;
        self.state = "building";
        self.building = Some(Progress::new(self.last_build_ms));
        self.send_status(None, None);
        let env = self.env();
        let job = self.service.begin_build(&env);
        let tx = self.tx.clone();
        let flags = self.flags.clone();
        flags.has_model.store(self.service.current().is_some(), Ordering::SeqCst);
        let db_path = config().db_path;
        let threads = irelia::sys::prepare_threads();
        std::thread::Builder::new()
            .name("model-build".into())
            .spawn(move || {
                let stop = || flags.has_model.load(Ordering::SeqCst) && (flags.busy.load(Ordering::SeqCst) || flags.queued.load(Ordering::SeqCst));
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    let db = Db::open(&db_path, OpenOptions { migrate: false, ..Default::default() })?;
                    job.run(&db, &env, threads, &stop)
                }));
                let result = match result {
                    Ok(r) => r,
                    Err(p) => Err(anyhow::anyhow!("build crashed: {}", panic_text(&p))),
                };
                let _ = tx.send(Msg::BuildDone(job, Box::new(result)));
            })
            .expect("model build thread");
    }

    fn build_done(&mut self, job: BuildJob, result: anyhow::Result<Built>) {
        self.build_running = false;
        let env = self.env();
        let fresh = self.service.finish_build(job, result, &env);
        self.region_cache = None;
        self.building = None;
        self.state = if self.service.current().is_some() {
            "ready"
        } else if self.service.error().is_some() {
            "failed"
        } else {
            "loading"
        };
        let (profiles, built) = match (fresh, self.service.current()) {
            (true, Some(b)) => {
                self.last_build_ms = Some(b.build_ms);
                (Some(Arc::new(b.profile_rows.clone())), Some((b.build_ms, b.prepared.len())))
            }
            _ => (None, None),
        };
        self.send_status(profiles, built);
        if self.build_again {
            self.build_again = false;
            if !self.held() {
                self.build();
            }
        }
    }

    fn build_soon(&mut self, ms: u64) {
        self.build_at = Some(Instant::now() + Duration::from_millis(ms));
    }

    fn needs_build(&mut self) -> bool {
        let env = self.env();
        self.service.needs_build(&self.db, &env)
    }

    fn handle(&mut self, req: Request) -> anyhow::Result<Value> {
        match req {
            Request::Draft { lane, opponent, enemies, allies, regions } => {
                let baseline = self.baseline(opponent, Some(lane))?;
                let has = self.service.current().is_some();
                let recent = if has { Some(self.recent()) } else { None };
                let facts = recent.as_ref().and_then(|r| matchup_facts(&r.prepared, lane, opponent, 3));
                let best = match (&facts, &recent) {
                    (Some(_), Some(r)) => {
                        let data = self.data.clone();
                        matchup_best(&r.scored, lane, opponent, &|id| is_purchasable_item(data.item(id)))
                    }
                    _ => None,
                };
                let env = self.env();
                let keep = self.flags.busy.load(Ordering::SeqCst);
                let q = DraftQuery { lane, opponent, enemies, allies };
                let draft = self.service.draft(&env, &q, regions, &baseline, keep);
                let facts = facts.map(|mut f| {
                    f["best"] = best.unwrap_or(Value::Null);
                    f
                });
                Ok(json!({
                    "baseline": serde_json::to_value(&*baseline)?,
                    "draft": draft.map(|d| serde_json::to_value(d)).transpose()?,
                    "mode": current_mode(),
                    "model": self.service.info(&env),
                    "facts": facts,
                }))
            }
            Request::Recommendation { enemy, lane } => Ok(serde_json::to_value(&*self.baseline(enemy, lane)?)?),
            Request::Live { raw, hint, plan_target } => {
                let env = self.env();
                let next = match self.service.current() {
                    Some(b) if current_mode() == "draft" && !plan_target.map(|p| p.1).unwrap_or(false) => {
                        let settings = read_settings(&env.scope, env.own_platform.clone(), None);
                        self.reranker.rerank(&b.model, &b.version, &raw, &self.data, &self.excl, &env.region_patch, &settings, &hint)
                    }
                    _ => None,
                };
                let (me, _) = identify(&raw, hint.opponent_name.as_ref().map(|o| o.as_deref()));
                let me = me.map(|i| raw["allPlayers"][i].clone());
                let target = next.as_ref().map(|n| n.item_id).or(plan_target.map(|p| p.0));
                let is_irelia = me.as_ref().and_then(|m| m["championName"].as_str()).and_then(|n| self.data.champion_by_key(n)).map(|c| c.id) == Some(IRELIA);
                let buy = match (is_irelia, target, me) {
                    (true, Some(target), Some(me)) => {
                        let owned: Vec<i64> = me["items"].as_array().map(|a| a.iter().filter_map(|i| i["itemID"].as_i64()).filter(|id| *id > 0).collect()).unwrap_or_default();
                        let opponent = hint.opponent_name.as_ref().and_then(|o| o.as_deref()).filter(|n| !n.is_empty()).and_then(|n| self.data.champion_by_key(n)).map(|c| c.id);
                        let gold = raw["activePlayer"]["currentGold"].as_f64().unwrap_or(0.0);
                        self.buys().best(&BuyQuery { lane: hint.lane.unwrap_or("TOP"), opponent, target, owned: &owned, gold })
                    }
                    _ => None,
                };
                Ok(json!({ "next": next.map(|n| serde_json::to_value(n)).transpose()?, "buy": buy }))
            }
            Request::Personal { lane } => {
                if self.service.current().is_none() {
                    return Ok(Value::Null);
                }
                Ok(personal_report(&self.recent().scored, lane))
            }
            Request::Review { match_id } => {
                let prepared: Vec<Arc<PreparedGame>> = self.service.current().map(|b| b.prepared.clone()).unwrap_or_default();
                let recent = self.recent();
                let ps = self.db.participants_of(&match_id)?;
                Ok(review_game(&match_id, &prepared, &recent.scored, &self.db, &self.data, &ps)?.unwrap_or(Value::Null))
            }
            Request::Status => Ok(self.status()),
            Request::Stats => self.stats(),
        }
    }

    fn stats(&mut self) -> anyhow::Result<Value> {
        let sc = scope();
        let fb = Fallback { patch: self.data.patch.clone(), previous_patch: self.data.previous_patch.clone() };
        let regions: Vec<Value> = region_patches(&self.db.game_versions()?, &fb, &sc.platforms, now_ms())
            .into_iter()
            .map(|(_, r)| {
                let mut v = region_json(&r);
                v["hotfixes"] = Value::Array(hotfixes(&r, &sc.hotfixes).iter().map(hotfix_json).collect());
                v
            })
            .collect();
        let c = self.db.counts(&self.data.patch)?;
        let patches: Vec<Value> = self
            .db
            .patch_breakdown(&sc.build_sources)?
            .into_iter()
            .map(|(patch, games, lanes)| {
                let mut l = serde_json::Map::new();
                for (pos, n) in lanes {
                    l.insert(pos, json!(n));
                }
                json!({ "patch": patch, "games": games, "lanes": l })
            })
            .collect();
        let mut sources = serde_json::Map::new();
        for (s, n) in self.db.source_counts()? {
            sources.insert(s, json!(n));
        }
        Ok(json!({
            "players": c.players, "otp": c.otp, "mains": c.mains, "matches": c.matches, "games": c.games, "current": c.current,
            "patches": patches, "currentPatch": self.data.patch, "previousPatch": self.data.previous_patch,
            "sources": sources, "buildSources": sc.build_sources, "regions": regions,
            "windowPatches": model_patches(&self.db, &self.data, MEMORY_PATCHES)?,
            "archive": self.db.archive_summary()?.into_iter().map(|(patch, games, bytes)| json!({ "patch": patch, "games": games, "bytes": bytes })).collect::<Vec<_>>(),
        }))
    }

    fn tick(&mut self) {
        let v = self.db.data_version();
        if v != self.seen {
            self.seen = v;
            (self.emit)(ModelEvent::Data);
        }
        if self.build_at.is_none() && !self.build_running && self.needs_build() {
            self.build_soon(0);
        }
        self.service.trim_caches(5 * 60_000);
        self.trim_rows();
    }

    fn run(&mut self, rx: &Receiver<Msg>) {
        self.send_status(None, None);
        self.build_soon(300);
        loop {
            let now = Instant::now();
            let mut wait = self.next_check.saturating_duration_since(now);
            if let Some(at) = self.build_at {
                wait = wait.min(at.saturating_duration_since(now));
            }
            match rx.recv_timeout(wait) {
                Ok(Msg::Stop) => return,
                Ok(m) => self.on_message(m),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let now = Instant::now();
            if let Some(at) = self.build_at {
                if now >= at {
                    self.build_at = None;
                    if !self.build_running && self.needs_build() {
                        self.build();
                    }
                }
            }
            if now >= self.next_check {
                self.next_check = now + Duration::from_secs(30);
                if let Err(p) = std::panic::catch_unwind(AssertUnwindSafe(|| self.tick())) {
                    eprintln!("[model] data check failed: {}", panic_text(&p));
                }
            }
        }
    }

    fn on_message(&mut self, m: Msg) {
        match m {
            Msg::Request(req, reply) => {
                let r = std::panic::catch_unwind(AssertUnwindSafe(|| self.handle(req)));
                let r = match r {
                    Ok(Ok(v)) => Ok(v),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(p) => Err(panic_text(&p)),
                };
                let _ = reply.send(r);
                if self.state != "loading" && self.state != "building" {
                    self.build_soon(2000);
                }
            }
            Msg::Env(e) => self.apply_env(&e),
            Msg::Patch(data) => {
                self.excl = Arc::new(build_exclusivity_index(&data));
                self.data = data;
                self.region_cache = None;
                (self.emit)(ModelEvent::Data);
                if !self.held() {
                    self.build();
                }
            }
            Msg::Evaluated => {
                self.service.evaluated();
                self.send_status(None, None);
            }
            Msg::Rebuild => {
                if !self.held() {
                    self.build();
                }
            }
            Msg::Release => {
                self.baseline_rows = None;
                self.baselines = (String::new(), indexmap::IndexMap::new());
                self.buy_index = None;
                self.recent_cache = None;
                self.service.trim_caches(0);
            }
            Msg::LiveReset => {
                self.reranker.reset();
                self.reranker.release();
            }
            Msg::Options => {
                self.service.options_changed();
                self.send_status(None, None);
                self.build_soon(0);
            }
            Msg::BuildDone(job, result) => self.build_done(job, *result),
            Msg::Stop => {}
            Msg::Crash => panic!("crash asked for by a test"),
        }
    }
}

pub fn panic_text(p: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".into()
    }
}

#[allow(dead_code)]
fn _js(_: f64) -> Value {
    js::num(0.0)
}
