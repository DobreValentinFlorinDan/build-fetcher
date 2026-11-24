//! The fetcher: what to ask Riot for next, under the key's budget, and when to stay quiet.
//!
//! One worker per routing value (europe for EUN1 + EUW1, asia for KR) runs in parallel: each has its own Riot
//! budget, and league-v4 calls go to the platform hosts, which have theirs. The budget itself is enforced by the
//! API client (`bf_core::riot::client::RiotClient`): it reads Riot's X-App-Rate-Limit-Count on every answer, so
//! requests another app (Irelia Companion) makes with the same key slow us down, and it follows Retry-After.
//!
//! Each worker picks its next piece of work in this order (`plan`):
//! 1. the user's own new games (on their platform), every `my_games_every_ms` and right after a game ends;
//! 2. tier A champions: players seen on a tier A champion-role whose history wasn't checked lately;
//! 3. thin champion-role pairs on the newest patch (fewest games first): their Master+ players, else their
//!    Diamond players, else (widening) Diamond players in general, which is how Diamond players of the pair are
//!    found;
//! 4. popular pairs: any Master+ player of the pool not checked lately (every game feeds every pair);
//! 5. a pool refresh (league-v4) when a pool is empty or a day old.
//!
//! Tier A = the user's top 5 champions by games, plus the Settings list, plus any champion the user played 3 times
//! on the newest patch. Every tier A game gets its timeline; other games get one with probability
//! `Settings::timeline_sample`, decided by a hash of the match id (stable across restarts).
//!
//! Fetching pauses in champ select and in game (the `Activity` watch): requests waiting for the budget are
//! abandoned at once and nothing new starts until the user is out of the game.

use crate::parse;
use crate::settings::Settings;
use crate::store::{id_hash, Ingest, Outcome, Store};
use anyhow::Result;
use bf_core::build::{Activity, FetchStatus, MatchRecord, Platform, RankBand, Role, Routing};
use bf_core::riot::client::{MatchIdQuery, RiotApi, RiotAuthError};
use bf_core::riot::rate_limiter::{wall_clock, Aborted, Clock, Signal};
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Clone, Debug)]
pub struct FetcherConfig {
    pub my_games_every_ms: i64,
    /// A player's history is checked again after this long.
    pub recheck_player_ms: i64,
    pub pool_refresh_ms: i64,
    /// Match ids asked per player (one request).
    pub ids_per_player: i64,
    /// Only games this recent are asked for.
    pub history_ms: i64,
    /// Diamond league pages per division when widening (205 players a page).
    pub diamond_pages: i64,
    pub tier_a_top: usize,
    pub tier_a_patch_games: usize,
    /// The thinnest pairs looked at per plan.
    pub thin_pairs: usize,
    /// Wait when there is nothing to do, and after an error.
    pub idle_ms: u64,
    pub error_backoff_ms: u64,
}

impl Default for FetcherConfig {
    fn default() -> Self {
        FetcherConfig {
            my_games_every_ms: 10 * 60_000,
            recheck_player_ms: 6 * 3_600_000,
            pool_refresh_ms: 24 * 3_600_000,
            ids_per_player: 20,
            history_ms: 14 * 24 * 3_600_000,
            diamond_pages: 2,
            tier_a_top: 5,
            tier_a_patch_games: 3,
            thin_pairs: 40,
            idle_ms: 60_000,
            error_backoff_ms: 5_000,
        }
    }
}

/// One piece of work.
#[derive(Clone, Debug, PartialEq)]
pub enum Work {
    /// Resolve the user's Riot ID to a puuid (account-v1).
    ResolveMe { platform: Platform, riot_id: String },
    MyGames { platform: Platform, puuid: String },
    Player { puuid: String, platform: Platform, rank: RankBand, label: String },
    Pool { platform: Platform, rank: RankBand },
    Idle,
}

#[derive(Default)]
struct State {
    paused: Option<String>,
    current: HashMap<Routing, String>,
    key_error: Option<String>,
    key_present: bool,
    requests: VecDeque<i64>,
    matches: VecDeque<i64>,
    last_my_games: i64,
    pools: HashMap<(Platform, RankBand), i64>,
    me: Option<String>,
    running: usize,
}

struct Inner<A: RiotApi> {
    api: Arc<A>,
    store: Arc<Store>,
    settings: Mutex<Settings>,
    cfg: FetcherConfig,
    clock: Clock,
    st: Mutex<State>,
    stop: Signal,
    /// Aborted on pause; replaced on resume.
    pause: Mutex<Signal>,
    wake: tokio::sync::Notify,
}

/// The fetcher. Cheap to clone (an `Arc` inside).
pub struct Fetcher<A: RiotApi + 'static> {
    inner: Arc<Inner<A>>,
}

impl<A: RiotApi + 'static> Clone for Fetcher<A> {
    fn clone(&self) -> Self {
        Fetcher { inner: self.inner.clone() }
    }
}

const HOUR_MS: i64 = 3_600_000;

fn is_paused(a: Activity) -> Option<&'static str> {
    match a {
        Activity::ChampSelect => Some("champ select"),
        Activity::InGame => Some("in game"),
        _ => None,
    }
}

impl<A: RiotApi + 'static> Fetcher<A> {
    pub fn new(api: Arc<A>, store: Arc<Store>, settings: Settings, cfg: FetcherConfig) -> Fetcher<A> {
        Fetcher::with_clock(api, store, settings, cfg, wall_clock())
    }

    /// `clock` gives epoch milliseconds (tests: tokio's paused clock from a fixed date).
    pub fn with_clock(api: Arc<A>, store: Arc<Store>, settings: Settings, cfg: FetcherConfig, clock: Clock) -> Fetcher<A> {
        Fetcher {
            inner: Arc::new(Inner {
                api,
                store,
                settings: Mutex::new(settings),
                cfg,
                clock,
                st: Mutex::new(State { key_present: true, ..Default::default() }),
                stop: Signal::new(),
                pause: Mutex::new(Signal::new()),
                wake: tokio::sync::Notify::new(),
            }),
        }
    }

    fn now(&self) -> i64 {
        (self.inner.clock)() as i64
    }

    pub fn update_settings(&self, s: Settings) {
        *self.inner.settings.lock() = s;
        self.inner.wake.notify_waiters();
    }

    /// The key was set, changed or removed: a rejected key stops fetching until this is called.
    pub fn set_key_present(&self, present: bool) {
        let mut st = self.inner.st.lock();
        st.key_present = present;
        st.key_error = None;
        drop(st);
        self.inner.wake.notify_waiters();
    }

    /// Starts one worker per routing value and the pause watcher. Stop with `stop()`.
    pub fn spawn(&self, activity: watch::Receiver<Activity>) -> Vec<tokio::task::JoinHandle<()>> {
        let mut out = Vec::new();
        {
            let me = self.clone();
            let mut rx = activity.clone();
            out.push(tokio::spawn(async move {
                let mut was_paused = false;
                loop {
                    let a = *rx.borrow_and_update();
                    let paused = is_paused(a);
                    me.inner.st.lock().paused = paused.map(String::from);
                    if paused.is_some() && !was_paused {
                        me.inner.pause.lock().abort("paused");
                    }
                    if paused.is_none() && was_paused {
                        *me.inner.pause.lock() = Signal::new();
                        // A game just ended: the user's new game comes first.
                        if a != Activity::ChampSelect {
                            me.inner.st.lock().last_my_games = 0;
                        }
                        me.inner.wake.notify_waiters();
                    }
                    was_paused = paused.is_some();
                    tokio::select! {
                        r = rx.changed() => if r.is_err() { return },
                        _ = me.inner.stop.cancelled() => return,
                    }
                }
            }));
        }
        for routing in [Routing::Europe, Routing::Asia] {
            let me = self.clone();
            let rx = activity.clone();
            out.push(tokio::spawn(async move { me.worker(routing, rx).await }));
        }
        out
    }

    pub fn stop(&self) {
        self.inner.stop.abort("stopped");
        self.inner.wake.notify_waiters();
    }

    /// What the dashboard shows.
    pub fn status(&self) -> FetchStatus {
        let now = self.now();
        let mut st = self.inner.st.lock();
        while st.requests.front().map(|t| *t < now - HOUR_MS).unwrap_or(false) {
            st.requests.pop_front();
        }
        while st.matches.front().map(|t| *t < now - HOUR_MS).unwrap_or(false) {
            st.matches.pop_front();
        }
        let (counted, _, validation, _) = self.inner.store.seen_counts().unwrap_or_default();
        let paused = st.paused.clone().or_else(|| (!st.key_present).then(|| "no API key".to_string())).or_else(|| st.key_error.as_ref().map(|_| "key rejected".to_string()));
        let mut current: Vec<String> = st.current.values().cloned().collect();
        current.sort();
        FetchStatus {
            running: st.running > 0 && paused.is_none(),
            paused,
            budgets: self.inner.api.budgets(),
            requests_last_hour: st.requests.len() as u32,
            matches_last_hour: st.matches.len() as u32,
            matches_total: counted + validation,
            dropped: self.inner.store.drops(),
            current: (!current.is_empty()).then(|| current.join("; ")),
            key_error: st.key_error.clone(),
        }
    }

    /// The user's puuid: from Settings, or resolved from their Riot ID.
    pub fn my_puuid(&self) -> Option<String> {
        self.inner.settings.lock().puuid.clone().or_else(|| self.inner.st.lock().me.clone())
    }

    /// Tier A now: the Settings list, the user's top champions by games, and any played 3 times on the newest patch.
    pub fn tier_a(&self) -> Result<HashSet<i64>> {
        let s = self.inner.settings.lock().clone();
        let mine = self.inner.store.my_games()?;
        let mut set: HashSet<i64> = s.tier_a.iter().copied().collect();
        let mut by: HashMap<i64, usize> = HashMap::new();
        for g in &mine {
            *by.entry(g.0).or_default() += 1;
        }
        let mut top: Vec<(i64, usize)> = by.into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        set.extend(top.iter().take(self.inner.cfg.tier_a_top).map(|x| x.0));
        if let Some(newest) = mine.iter().map(|g| g.2).max() {
            let mut this: HashMap<i64, usize> = HashMap::new();
            for g in mine.iter().filter(|g| g.2 == newest) {
                *this.entry(g.0).or_default() += 1;
            }
            set.extend(this.into_iter().filter(|x| x.1 >= self.inner.cfg.tier_a_patch_games).map(|x| x.0));
        }
        Ok(set)
    }

    fn tier_a_roles(&self, champion: i64) -> Result<Vec<Role>> {
        let mut roles: Vec<Role> = self.inner.store.my_games()?.iter().filter(|g| g.0 == champion).map(|g| g.1).collect();
        roles.sort();
        roles.dedup();
        if roles.is_empty() {
            use bf_core::build::DataSource;
            let patch = self.inner.store.current_patch()?;
            let vols: Vec<_> = self.inner.store.volumes(&patch.into_iter().collect::<Vec<_>>())?.into_iter().filter(|v| v.champion == champion).collect();
            let total: u32 = vols.iter().map(|v| v.games).sum();
            roles = vols.iter().filter(|v| v.games * 10 >= total).map(|v| v.role).collect();
        }
        Ok(roles)
    }

    /// The next piece of work for one routing value (see the module comment for the order).
    pub fn plan(&self, routing: Routing, now: i64) -> Result<Work> {
        use bf_core::build::DataSource;
        let s = self.inner.settings.lock().clone();
        let cfg = &self.inner.cfg;
        let store = &self.inner.store;
        let platforms: Vec<Platform> = s.regions.iter().copied().filter(|p| p.routing() == routing).collect();
        let due = now - cfg.recheck_player_ms;
        // 1. The user's games.
        if s.my_platform.routing() == routing {
            match self.my_puuid() {
                Some(puuid) if now - self.inner.st.lock().last_my_games >= cfg.my_games_every_ms => return Ok(Work::MyGames { platform: s.my_platform, puuid }),
                None if s.riot_id.is_some() && now - self.inner.st.lock().last_my_games >= cfg.my_games_every_ms => {
                    return Ok(Work::ResolveMe { platform: s.my_platform, riot_id: s.riot_id.clone().unwrap() })
                }
                _ => {}
            }
        }
        if platforms.is_empty() {
            return Ok(Work::Idle);
        }
        let data = store.static_data();
        let name = |c: i64| data.champion(c).map(|c| c.name.clone()).unwrap_or_else(|| c.to_string());
        let bands: Vec<RankBand> = match s.rank_floor {
            RankBand::Diamond => vec![RankBand::MasterPlus, RankBand::Diamond],
            _ if s.widen_to_diamond => vec![RankBand::MasterPlus, RankBand::Diamond],
            _ => vec![RankBand::MasterPlus],
        };
        // 2. Tier A.
        let mut tier_a: Vec<i64> = self.tier_a()?.into_iter().collect();
        tier_a.sort();
        for c in tier_a {
            for role in self.tier_a_roles(c)? {
                for band in &bands {
                    if let Some((puuid, platform)) = store.players_for(c, role, *band, &platforms, due, 1)?.into_iter().next() {
                        return Ok(Work::Player { puuid, platform, rank: *band, label: format!("tier A: {} {}", name(c), role.as_riot()) });
                    }
                }
            }
        }
        // 3. Thin pairs on the newest patch.
        if let Some(patch) = store.current_patch()? {
            let mut vols = store.volumes(&[patch])?;
            vols.retain(|v| v.games < store.opts.thin_games);
            vols.sort_by_key(|v| (v.games, v.champion, v.role));
            for v in vols.iter().take(cfg.thin_pairs) {
                for band in &bands {
                    if let Some((puuid, platform)) = store.players_for(v.champion, v.role, *band, &platforms, due, 1)?.into_iter().next() {
                        return Ok(Work::Player { puuid, platform, rank: *band, label: format!("thin: {} {}", name(v.champion), v.role.as_riot()) });
                    }
                }
            }
            // Nobody known for the thinnest pair: widen to Diamond's ladder, where its players are found.
            if let Some(v) = vols.first() {
                if bands.contains(&RankBand::Diamond) {
                    if let Some(w) = self.ladder_work(&platforms, RankBand::Diamond, due, now, format!("thin, widened to Diamond: {} {}", name(v.champion), v.role.as_riot()))? {
                        return Ok(w);
                    }
                }
            }
        }
        // 4. Popular pairs: the Master+ pool (Diamond too when that is the floor).
        let popular = if s.rank_floor == RankBand::Diamond { RankBand::Diamond } else { RankBand::MasterPlus };
        if let Some(w) = self.ladder_work(&platforms, popular, due, now, "popular".into())? {
            return Ok(w);
        }
        Ok(Work::Idle)
    }

    /// A player of a ladder to check, or the ladder's refresh when it is empty or old.
    fn ladder_work(&self, platforms: &[Platform], rank: RankBand, due: i64, now: i64, label: String) -> Result<Option<Work>> {
        let st = self.inner.st.lock();
        for p in platforms {
            let refreshed = st.pools.get(&(*p, rank)).copied().unwrap_or(i64::MIN / 2);
            if now - refreshed >= self.inner.cfg.pool_refresh_ms {
                return Ok(Some(Work::Pool { platform: *p, rank }));
            }
        }
        drop(st);
        Ok(self.inner.store.due_players(platforms, rank, due, 1)?.into_iter().next().map(|(puuid, platform, _)| Work::Player { puuid, platform, rank, label }))
    }

    async fn worker(self, routing: Routing, mut activity: watch::Receiver<Activity>) {
        self.inner.st.lock().running += 1;
        loop {
            if self.inner.stop.aborted() {
                break;
            }
            // Paused: wait for the game to end (or a stop).
            if is_paused(*activity.borrow_and_update()).is_some() {
                tokio::select! {
                    r = activity.changed() => if r.is_err() { break },
                    _ = self.inner.stop.cancelled() => break,
                }
                continue;
            }
            let blocked = {
                let st = self.inner.st.lock();
                !st.key_present || st.key_error.is_some()
            };
            if blocked {
                self.idle(routing, None, &mut activity).await;
                continue;
            }
            let now = self.now();
            let work = match self.plan(routing, now) {
                Ok(w) => w,
                Err(e) => {
                    self.inner.store.log(format!("fetcher ({routing:?}): planning failed: {e}"));
                    Work::Idle
                }
            };
            if work == Work::Idle {
                self.inner.st.lock().current.remove(&routing);
                self.idle(routing, Some(self.inner.cfg.idle_ms), &mut activity).await;
                continue;
            }
            let signal = Signal::any(&[&self.inner.stop, &self.inner.pause.lock().clone()]);
            match self.run(routing, &work, &signal).await {
                Ok(()) => {}
                Err(e) if e.downcast_ref::<Aborted>().is_some() || signal.aborted() => {}
                Err(e) if e.downcast_ref::<RiotAuthError>().is_some() => {
                    self.inner.st.lock().key_error = Some(e.to_string());
                    self.inner.store.log(format!("fetcher: {e}"));
                }
                Err(e) => {
                    self.inner.store.log(format!("fetcher ({routing:?}): {e}"));
                    self.idle(routing, Some(self.inner.cfg.error_backoff_ms), &mut activity).await;
                }
            }
        }
        let mut st = self.inner.st.lock();
        st.running -= 1;
        st.current.remove(&routing);
    }

    /// Waits `ms` (None: until woken), or less on a stop, a pause, a resume or new settings.
    async fn idle(&self, _routing: Routing, ms: Option<u64>, activity: &mut watch::Receiver<Activity>) {
        let sleep = async {
            match ms {
                Some(ms) => tokio::time::sleep(std::time::Duration::from_millis(ms)).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = sleep => {}
            _ = self.inner.wake.notified() => {}
            _ = activity.changed() => {}
            _ = self.inner.stop.cancelled() => {}
        }
    }

    fn note_request(&self) {
        let now = self.now();
        self.inner.st.lock().requests.push_back(now);
    }

    async fn run(&self, routing: Routing, work: &Work, signal: &Signal) -> Result<()> {
        let label = match work {
            Work::ResolveMe { .. } | Work::MyGames { .. } => "my games".to_string(),
            Work::Player { label, .. } => label.clone(),
            Work::Pool { platform, rank } => format!("ladder: {} {:?}", platform.as_str(), rank),
            Work::Idle => return Ok(()),
        };
        self.inner.st.lock().current.insert(routing, label);
        let api = &self.inner.api;
        match work {
            Work::ResolveMe { platform, riot_id } => {
                let (name, tag) = riot_id.split_once('#').unwrap_or((riot_id.as_str(), ""));
                self.note_request();
                let host = bf_core::riot::routing::account_host(Some(platform.as_str()));
                let puuid = api.account_by_riot_id(name.trim(), tag.trim(), host, signal).await?;
                let mut st = self.inner.st.lock();
                match puuid {
                    Some(p) => st.me = Some(p),
                    None => {
                        // Unknown Riot ID: don't ask again until the next round.
                        st.last_my_games = self.now();
                        drop(st);
                        self.inner.store.log(format!("fetcher: Riot ID {riot_id} not found"));
                    }
                }
            }
            Work::MyGames { platform, puuid } => {
                let since = self.inner.store.newest_mine_ms()?.map(|t| t + 1000).unwrap_or(self.now() - 30 * 24 * HOUR_MS);
                let q = MatchIdQuery { queue: parse::QUEUE_RANKED_SOLO, start_time: Some(since / 1000), start: 0, count: self.inner.cfg.ids_per_player, fresh: true };
                self.note_request();
                let ids = api.match_ids_page(platform.as_str(), puuid, &q, signal).await?;
                self.fetch_matches(*platform, &ids, RankBand::Mine, Some(puuid), signal).await?;
                self.inner.st.lock().last_my_games = self.now();
                let tier_a = self.tier_a()?;
                if tier_a != self.inner.store.tier_a() {
                    self.inner.store.set_tier_a(&tier_a)?;
                }
            }
            Work::Player { puuid, platform, rank, .. } => {
                let q = MatchIdQuery { queue: parse::QUEUE_RANKED_SOLO, start_time: Some((self.now() - self.inner.cfg.history_ms) / 1000), start: 0, count: self.inner.cfg.ids_per_player, fresh: true };
                self.note_request();
                let ids = api.match_ids_page(platform.as_str(), puuid, &q, signal).await?;
                // Checked even if the matches fail below: a broken history shouldn't be asked for again at once.
                self.inner.store.mark_checked(puuid, *platform, *rank, self.now())?;
                let me = self.my_puuid();
                self.fetch_matches(*platform, &ids, *rank, me.as_deref(), signal).await?;
            }
            Work::Pool { platform, rank } => {
                let mut players: Vec<(String, i64)> = Vec::new();
                if *rank == RankBand::Diamond {
                    for division in ["I", "II", "III", "IV"] {
                        for page in 1..=self.inner.cfg.diamond_pages {
                            self.note_request();
                            let list = api.league_page(platform.as_str(), "DIAMOND", division, page, signal).await?;
                            let empty = list.is_empty();
                            players.extend(list.into_iter().map(|e| (e.puuid, e.league_points)));
                            if empty {
                                break;
                            }
                        }
                    }
                } else {
                    for tier in ["CHALLENGER", "GRANDMASTER", "MASTER"] {
                        self.note_request();
                        let list = api.league_entries(platform.as_str(), tier, &[], 1, signal).await?;
                        players.extend(list.into_iter().map(|e| (e.puuid, e.league_points)));
                    }
                }
                let n = self.inner.store.add_players(*platform, *rank, &players)?;
                self.inner.st.lock().pools.insert((*platform, *rank), self.now());
                self.inner.store.log(format!("ladder {} {:?}: {n} players", platform.as_str(), rank));
            }
            Work::Idle => {}
        }
        Ok(())
    }

    /// Fetches the matches not seen yet (with a timeline for tier A and the sample) and ingests each.
    async fn fetch_matches(&self, platform: Platform, ids: &[String], rank: RankBand, me: Option<&str>, signal: &Signal) -> Result<()> {
        let tier_a = self.inner.store.tier_a();
        let sample = self.inner.settings.lock().timeline_sample;
        for id in ids {
            signal.check()?;
            if self.inner.store.is_seen(id)? {
                continue;
            }
            // Match ids carry their platform ("EUW1_…"): fetch on that one's routing.
            let on = id.split('_').next().and_then(Platform::parse).unwrap_or(platform);
            self.note_request();
            let Some(m) = self.inner.api.get_match(on.as_str(), id, signal).await? else { continue };
            let timeline = if self.wants_timeline(&m, rank, me, &tier_a, sample) {
                self.note_request();
                self.inner.api.timeline(on.as_str(), id, signal).await?
            } else {
                None
            };
            let out = self.inner.store.ingest(&[Ingest { m: &m, timeline: timeline.as_ref(), rank, me }])?;
            if matches!(out.first(), Some(Outcome::Counted { .. }) | Some(Outcome::Validation)) {
                let now = self.now();
                self.inner.st.lock().matches.push_back(now);
            }
        }
        Ok(())
    }

    /// A timeline costs a request: only for games that will be kept with one.
    fn wants_timeline(&self, m: &Value, rank: RankBand, me: Option<&str>, tier_a: &HashSet<i64>, sample: f64) -> bool {
        let Ok(p) = parse::parse(m, None, rank, me) else { return false };
        let r: &MatchRecord = &p.record;
        if r.rank == Some(RankBand::Mine) || r.players.iter().any(|pl| tier_a.contains(&pl.champion)) {
            return true;
        }
        // A different hash from the validation sample's, so the two samples don't line up.
        (id_hash(&format!("timeline:{}", r.match_id)) % 10_000) < (sample.clamp(0.0, 1.0) * 10_000.0) as u64
    }
}
