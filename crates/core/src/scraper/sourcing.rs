//! Port of `server/scraper/sourcing.ts` (sourcing v2): which Riot request each host makes next, and what it does with
//! the answer. One lane per platform host (ladders, mastery, ranks) and per regional host (match ids, matches,
//! timelines); each asks `next_job` for the most valuable thing to do.

use super::fetch::{fetch_harvested, fetch_match, IngestContext, IngestEnv};
use super::ingest::{store_game, store_match, Fetched};
use super::pilots::{classify_pilot, history_score, irelia_share, match_score, OWN_GAMES_SCORE};
use super::pool::in_order;
use crate::config::{config, CHAMPION_ID, RANKED_FLEX_QUEUE, RANKED_SOLO_QUEUE, RECHECK_OTHERS_AFTER_MS, RECHECK_OTP_AFTER_MS, RESCAN_PLAYER_AFTER_MS, RESEED_AFTER_MS};
use crate::db::{Db, PilotUpdate, PlayerRow, QueuedMatch};
use crate::patches::{region_patch, Fallback, RegionPatch, PATCH_CYCLE_MS};
use crate::riot::client::{MasteryDto, MatchIdQuery, RiotApi};
use crate::riot::rate_limiter::Signal;
use crate::riot::routing::{normalize_platform, region_of, PLATFORMS};
use crate::scope::ScrapeScope;
use anyhow::Result;
use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

/// What sourcing reads from the app.
pub struct SourcingEnv<A: RiotApi> {
    pub db: Arc<Mutex<Db>>,
    pub riot: Arc<A>,
    pub scope: Arc<dyn Fn() -> ScrapeScope + Send + Sync>,
    pub fallback_patch: Arc<dyn Fn() -> Fallback + Send + Sync>,
    pub me: Arc<dyn Fn() -> Option<(String, String)> + Send + Sync>,
    pub concurrency: usize,
    pub log: Arc<dyn Fn(&str) + Send + Sync>,
    pub now: Option<Arc<dyn Fn() -> i64 + Send + Sync>>,
    pub recheck_pilots_ms: Option<i64>,
    pub recheck_others_ms: Option<i64>,
    pub reseed_ms: Option<i64>,
    pub rescan_pilot_ms: Option<i64>,
}

/// A unit of work for one host.
pub struct Job {
    pub label: String,
    pub score: f64,
    pub run: Box<dyn FnOnce(Signal) -> BoxFuture<'static, Result<()>> + Send>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LaneKey {
    Platform(String),
    Regional { region: String, platforms: Vec<String> },
}

pub fn lane_host(k: &LaneKey) -> String {
    match k {
        LaneKey::Platform(p) => format!("{}.api.riotgames.com", p.to_lowercase()),
        LaneKey::Regional { region, .. } => format!("{}.api.riotgames.com", region.to_lowercase()),
    }
}

fn title_case(t: &str) -> String {
    let mut c = t.chars();
    match c.next() {
        Some(f) => f.to_string() + &c.as_str().to_lowercase(),
        None => String::new(),
    }
}

fn match_number(id: &str) -> i64 {
    id.find('_').and_then(|i| id[i + 1..].parse::<i64>().ok()).unwrap_or(0)
}

pub struct Sourcing<A: RiotApi> {
    env: SourcingEnv<A>,
    region_cache: Mutex<HashMap<String, (i64, RegionPatch)>>,
    own_due_at: Mutex<Vec<i64>>,
    own_last_at: Mutex<i64>,
}

impl<A: RiotApi + 'static> Sourcing<A> {
    pub fn new(env: SourcingEnv<A>) -> Arc<Sourcing<A>> {
        Arc::new(Sourcing { env, region_cache: Mutex::new(HashMap::new()), own_due_at: Mutex::new(vec![]), own_last_at: Mutex::new(0) })
    }

    pub fn now(&self) -> i64 {
        self.env.now.as_ref().map(|f| f()).unwrap_or_else(crate::util::now_ms)
    }
    fn scope(&self) -> ScrapeScope {
        (self.env.scope)()
    }
    pub fn db(&self) -> &Arc<Mutex<Db>> {
        &self.env.db
    }

    pub fn region(&self, platform: &str) -> RegionPatch {
        let now = self.now();
        if let Some((at, v)) = self.region_cache.lock().get(platform) {
            if now - at < 2000 {
                return v.clone();
            }
        }
        let versions = self.env.db.lock().game_versions().unwrap_or_default();
        let value = region_patch(platform, &versions, &(self.env.fallback_patch)(), now);
        self.region_cache.lock().insert(platform.to_string(), (now, value.clone()));
        value
    }
    pub fn invalidate(&self) {
        self.region_cache.lock().clear();
    }

    fn ingest_env<'a>(self: &'a Arc<Self>, region: &'a (dyn Fn(&str) -> RegionPatch + Send + Sync)) -> IngestEnv<'a, A> {
        IngestEnv {
            db: &self.env.db,
            riot: &*self.env.riot,
            champion_id: CHAMPION_ID,
            region,
            harvest: self.scope().harvest_irelias,
            me_puuid: (self.env.me)().map(|m| m.0),
        }
    }

    pub fn lanes(&self) -> Vec<LaneKey> {
        let scope = self.scope();
        let me = (self.env.me)();
        let mut platforms: Vec<String> = Vec::new();
        for p in scope.platforms.iter().cloned().chain(me.filter(|_| scope.own_games).map(|m| m.1)) {
            if !platforms.contains(&p) {
                platforms.push(p);
            }
        }
        let mut regions: indexmap::IndexMap<String, Vec<String>> = indexmap::IndexMap::new();
        for p in &platforms {
            regions.entry(region_of(p).to_string()).or_default().push(p.clone());
        }
        let _ = PLATFORMS;
        scope
            .platforms
            .iter()
            .map(|p| LaneKey::Platform(p.clone()))
            .chain(regions.into_iter().map(|(region, platforms)| LaneKey::Regional { region, platforms }))
            .collect()
    }

    pub fn next_job(self: &Arc<Self>, lane: &LaneKey) -> Result<Option<Job>> {
        match lane {
            LaneKey::Platform(p) => self.platform_job(p),
            LaneKey::Regional { platforms, .. } => self.regional_job(platforms),
        }
    }

    // ------------------------------------------------------------------------------- platform host

    fn platform_job(self: &Arc<Self>, platform: &str) -> Result<Option<Job>> {
        let scope = self.scope();
        let now = self.now();
        let reseed = self.env.reseed_ms.unwrap_or(RESEED_AFTER_MS);
        let tier = scope.tiers.iter().find(|t| {
            let at = self.env.db.lock().get_meta(&format!("seeded_at:{platform}:{t}")).map(|v| crate::js::to_number(&serde_json::Value::String(v))).unwrap_or(0.0);
            now as f64 - at > reseed as f64
        });
        if let Some(tier) = tier.cloned() {
            let me = self.clone();
            let p = platform.to_string();
            return Ok(Some(Job {
                label: format!("Seeding {platform} {} ladder", title_case(&tier)),
                score: 3.0,
                run: Box::new(move |signal| {
                    Box::pin(async move {
                        let n = me.seed_tier(&p, &tier, &signal).await?;
                        (me.env.log)(&format!("Loaded {n} {p} {} players", title_case(&tier)));
                        Ok(())
                    })
                }),
            }));
        }
        let region = self.region(platform);
        let patch_began = if region.start_known { region.start } else { 0 };
        let marker = format!("activity_patch:{platform}");
        if region.start_known && self.env.db.lock().get_meta(&marker).as_deref() != Some(region.patch.as_str()) {
            let again = {
                let db = self.env.db.lock();
                let n = db.forget_activity_checks(platform, region.start)?;
                db.set_meta(&marker, &region.patch)?;
                n
            };
            if again > 0 {
                (self.env.log)(&format!("{platform} {}: checking {again} pilots' Irelia activity again", region.patch));
            }
        }
        let c = self.env.concurrency as i64;
        let work = |lo: i64, hi: i64| -> Result<Vec<PlayerRow>> {
            self.env.db.lock().mastery_work(
                platform,
                (now - self.env.recheck_pilots_ms.unwrap_or(RECHECK_OTP_AFTER_MS)).max(patch_began),
                now - self.env.recheck_others_ms.unwrap_or(RECHECK_OTHERS_AFTER_MS),
                lo,
                hi,
                c * 4,
            )
        };
        let urgent = work(0, 1)?;
        if !urgent.is_empty() {
            let label = if urgent[0].prio == Some(0) { "Checking harvested Irelia players" } else { "Refreshing pilot activity" };
            return Ok(Some(self.mastery_job(platform, urgent, label.into(), 2.0)));
        }
        if scope.lookup_harvested_ranks {
            let unranked = self.env.db.lock().unranked_candidates(platform, c * 4)?;
            if !unranked.is_empty() {
                let me = self.clone();
                let p = platform.to_string();
                return Ok(Some(Job {
                    label: format!("Looking up {platform} harvested pilots' ranks"),
                    score: 1.5,
                    run: Box::new(move |signal| Box::pin(async move { me.lookup_ranks(&p, unranked, &signal).await })),
                }));
            }
        }
        let rest = work(2, 3)?;
        if !rest.is_empty() {
            return Ok(Some(self.mastery_job(platform, rest, format!("Identifying {platform} Irelia pilots"), 1.0)));
        }
        Ok(None)
    }

    fn mastery_job(self: &Arc<Self>, platform: &str, players: Vec<PlayerRow>, label: String, score: f64) -> Job {
        let me = self.clone();
        let p = platform.to_string();
        Job {
            label,
            score,
            run: Box::new(move |signal| {
                Box::pin(async move {
                    let log = me.env.log.clone();
                    me.check_players(&players, &signal, &mut |pl, u| {
                        if u.pilot_class != "none" && pl.pilot_class.as_deref() != Some(u.pilot_class.as_str()) {
                            log(&format!(
                                "New {p} Irelia {} found ({}k mastery)",
                                if u.pilot_class == "otp" { "OTP" } else { "main" },
                                crate::js::num_to_string(crate::js::round(u.irelia_points as f64 / 1000.0))
                            ));
                        }
                        true
                    })
                    .await
                })
            }),
        }
    }

    pub async fn seed_tier(&self, platform: &str, tier: &str, signal: &Signal) -> Result<usize> {
        let scope = self.scope();
        let entries = self.env.riot.league_entries(platform, tier, &scope.divisions, scope.max_league_pages_per_division as i64, signal).await?;
        {
            let db = self.env.db.lock();
            db.tx(|| {
                for e in &entries {
                    if !e.puuid.is_empty() {
                        db.upsert_player(&e.puuid, platform, tier, e.league_points)?;
                    }
                }
                Ok(())
            })?;
            db.set_meta(&format!("seeded_at:{platform}:{tier}"), &self.now().to_string())?;
        }
        Ok(entries.len())
    }

    pub async fn mastery_of(&self, player: &PlayerRow, signal: &Signal) -> Result<PilotUpdate> {
        let platform = normalize_platform(&player.platform)?;
        let region = self.region(&platform);
        let since = if region.start_known { Some(region.start) } else { None };
        let scope = self.scope();
        let top: Vec<MasteryDto> = self.env.riot.top_masteries(&platform, &player.puuid, scope.mastery_top_count as i64, signal, since).await?;
        let mut irelia = None;
        if !top.iter().any(|m| m.champion_id == CHAMPION_ID) && !player.found_by.is_empty() && player.found_by != "ladder" {
            irelia = self.env.riot.champion_mastery(&platform, &player.puuid, CHAMPION_ID, signal, since).await?;
        }
        Ok(classify_pilot(&top, irelia.as_ref(), CHAMPION_ID, &scope))
    }

    pub async fn check_players(&self, players: &[PlayerRow], signal: &Signal, on_checked: &mut (dyn FnMut(&PlayerRow, &PilotUpdate) -> bool + Send)) -> Result<()> {
        in_order(players, self.env.concurrency, signal, |p, _, s| async move { self.mastery_of(&p, &s).await }, |u, p, _| {
            self.env.db.lock().set_pilot(&p.puuid, &u)?;
            Ok(on_checked(p, &u))
        })
        .await
    }

    async fn lookup_ranks(&self, platform: &str, players: Vec<PlayerRow>, signal: &Signal) -> Result<()> {
        in_order(&players, self.env.concurrency, signal, |p, _, s| async move { self.env.riot.ranked_entries(platform, &p.puuid, &s).await }, |entries, p, _| {
            let solo = entries.iter().find(|e| e.queue_type == "RANKED_SOLO_5x5");
            self.env.db.lock().set_rank(&p.puuid, solo.map(|s| s.tier.as_str()).unwrap_or("UNRANKED"), solo.map(|s| s.league_points).unwrap_or(0))?;
            Ok(true)
        })
        .await
    }

    // ------------------------------------------------------------------------------- regional host

    fn regional_job(self: &Arc<Self>, platforms: &[String]) -> Result<Option<Job>> {
        let scope = self.scope();
        let now = self.now();
        let me = (self.env.me)();
        if let Some((_, plat)) = &me {
            if scope.own_games && platforms.contains(plat) && self.own_games_due(now) {
                let s = self.clone();
                return Ok(Some(Job {
                    label: "Fetching your Irelia games".into(),
                    score: OWN_GAMES_SCORE + 1.0,
                    run: Box::new(move |signal| Box::pin(async move { s.list_own_games(&signal).await })),
                }));
            }
        }
        let queued = self.env.db.lock().next_queued_batch(platforms, self.env.concurrency as i64 * 4)?;
        let mut best: Option<(PlayerRow, f64)> = None;
        for platform in platforms.iter().filter(|p| scope.platforms.contains(p)) {
            let region = self.region(platform);
            let due = self.env.db.lock().due_pilots(platform, &scope.pilot_classes, now - self.env.rescan_pilot_ms.unwrap_or(RESCAN_PLAYER_AFTER_MS), region.start, 25)?;
            for pilot in due {
                let score = history_score(&pilot, now, scope.max_matches_per_pilot, None);
                if best.as_ref().map(|b| score > b.1).unwrap_or(true) {
                    best = Some((pilot, score));
                }
            }
        }
        if !queued.is_empty() && best.as_ref().map(|b| queued[0].score >= b.1).unwrap_or(true) {
            let harvest = queued[0].kind == "timeline";
            let label = if harvest {
                format!("Fetching harvested Irelia timelines ({})", queued[0].platform)
            } else {
                format!("Fetching {} matches", queued[0].platform)
            };
            let kind = queued[0].kind.clone();
            let score = queued[0].score;
            let items: Vec<QueuedMatch> = queued.into_iter().filter(|q| q.kind == kind).collect();
            let s = self.clone();
            return Ok(Some(Job { label, score, run: Box::new(move |signal| Box::pin(async move { s.process_queued(items, &signal).await })) }));
        }
        if let Some((pilot, score)) = best {
            let s = self.clone();
            return Ok(Some(Job {
                label: format!("Reading {} {} match history", pilot.platform, if pilot.pilot_class.as_deref() == Some("main") { "main" } else { "OTP" }),
                score,
                run: Box::new(move |signal| Box::pin(async move { s.queue_history(&pilot, &signal).await })),
            }));
        }
        Ok(None)
    }

    pub async fn list_history(&self, puuid: &str, platform: &str, signal: &Signal, queue: Option<i64>, since: Option<i64>, cap: Option<f64>, fresh: bool) -> Result<Vec<String>> {
        let scope = self.scope();
        let platform = normalize_platform(platform)?;
        let queue = queue.unwrap_or(RANKED_SOLO_QUEUE);
        if scope.history_window == "newest" && since.is_none() {
            return self.env.riot.match_ids(&platform, puuid, queue, scope.match_ids_per_player as i64, signal).await;
        }
        let start_time = (since.unwrap_or_else(|| self.region(&platform).start) as f64 / 1000.0).floor() as i64;
        let cap = cap.unwrap_or(scope.max_matches_per_pilot) as i64;
        let mut ids: Vec<String> = Vec::new();
        let mut start = 0;
        while start < cap {
            let count = 100.min(cap - start);
            let page = self
                .env
                .riot
                .match_ids_page(&platform, puuid, &MatchIdQuery { queue, start_time: Some(start_time), start, count, fresh: fresh && start == 0 }, signal)
                .await?;
            let n = page.len() as i64;
            let all_known = {
                let db = self.env.db.lock();
                page.iter().all(|id| db.has_match(id))
            };
            ids.extend(page);
            if n < count || all_known {
                break;
            }
            start += 100;
        }
        Ok(ids)
    }

    pub async fn queue_history(&self, pilot: &PlayerRow, signal: &Signal) -> Result<()> {
        let platform = normalize_platform(&pilot.platform)?;
        let ids = self.list_history(&pilot.puuid, &platform, signal, None, None, None, false).await?;
        let source = if pilot.pilot_class.as_deref() == Some("main") { "main" } else { "otp" };
        let score = match_score(irelia_share(pilot, Some(source)));
        let mut queued = 0;
        {
            let db = self.env.db.lock();
            db.tx(|| {
                for id in &ids {
                    if !db.has_match(id) {
                        db.enqueue(&QueuedMatch { match_id: id.clone(), platform: platform.clone(), kind: "match".into(), puuid: pilot.puuid.clone(), source: source.into(), score })?;
                        queued += 1;
                    }
                }
                db.set_scanned(&pilot.puuid)?;
                Ok(())
            })?;
        }
        if queued > 0 {
            (self.env.log)(&format!("{platform} {} history: {queued} new matches queued", if source == "main" { "main" } else { "OTP" }));
        }
        Ok(())
    }

    pub async fn process_queued(self: &Arc<Self>, items: Vec<QueuedMatch>, signal: &Signal) -> Result<()> {
        let me = self.clone();
        let region = move |p: &str| me.region(p);
        let env = self.ingest_env(&region);
        enum R {
            Harvested(QueuedMatch, Option<super::ingest::GameCapture>),
            Fetched(QueuedMatch, Fetched),
        }
        in_order(
            &items,
            self.env.concurrency,
            signal,
            |q, _, s| {
                let env = &env;
                async move {
                    let q = &q;
                    let platform = normalize_platform(&q.platform)?;
                    if q.kind == "timeline" {
                        return Ok(R::Harvested(q.clone(), fetch_harvested(env, &platform, &q.match_id, &q.puuid, &s).await?));
                    }
                    let ctx = IngestContext { pilot_puuid: Some(q.puuid.clone()), source: q.source.clone(), keep_previous_patch: q.source == "me" };
                    Ok(R::Fetched(q.clone(), fetch_match(env, &platform, &q.match_id, &ctx, &s).await?))
                }
            },
            |r, _, _| {
                let (q, result, src, fetched_patch) = match r {
                    R::Harvested(q, g) => {
                        let db = self.env.db.lock();
                        db.tx(|| {
                            if let Some(g) = &g {
                                store_game(&db, g)?;
                            }
                            db.dequeue(&q.match_id)
                        })?;
                        let res = if g.is_some() { "harvested" } else { "skipped" };
                        (q, res, Some("harvested".to_string()), None)
                    }
                    R::Fetched(q, f) => {
                        let (res, src, patch) = {
                            let db = self.env.db.lock();
                            let res = store_match(&db, &q.match_id, &f)?;
                            db.dequeue(&q.match_id)?;
                            if res == "stale" && q.source != "me" {
                                db.drop_older_queued(&q.puuid, &q.platform, match_number(&q.match_id))?;
                            }
                            let (src, patch) = match &f {
                                Fetched::Fetched(m) => (m.game.as_ref().and_then(|g| g.row.pilot_source.clone()), Some(m.row.patch.clone())),
                                _ => (Some(q.source.clone()), None),
                            };
                            (res, src, patch)
                        };
                        (q, res, src, patch)
                    }
                };
                if result == "irelia" || result == "harvested" {
                    let src = src.unwrap_or_default();
                    (self.env.log)(&format!("Stored {} Irelia game {}", if src == "me" { "your".to_string() } else { src }, q.match_id).replace("  ", " "));
                }
                if let Some(p) = fetched_patch {
                    if let Ok(plat) = normalize_platform(&q.platform) {
                        if p != self.region(&plat).patch {
                            self.invalidate();
                        }
                    }
                }
                Ok(true)
            },
        )
        .await
    }

    // ------------------------------------------------------------------------------- the user's own games

    pub fn own_games_due(&self, now: i64) -> bool {
        if self.own_due_at.lock().first().map(|t| *t <= now).unwrap_or(false) {
            return true;
        }
        now - *self.own_last_at.lock() > 30 * 60_000
    }

    pub fn request_own_games(&self, delays_ms: &[i64]) {
        let now = self.now();
        let mut d = self.own_due_at.lock();
        d.extend(delays_ms.iter().map(|x| now + x));
        d.sort();
    }

    pub async fn list_own_games(&self, signal: &Signal) -> Result<()> {
        let me = (self.env.me)();
        let now = self.now();
        *self.own_last_at.lock() = now;
        self.own_due_at.lock().retain(|t| *t > now);
        let Some((puuid, platform)) = me else { return Ok(()) };
        let since = self.region(&platform).start - PATCH_CYCLE_MS;
        let mut queued = 0;
        for queue in [RANKED_SOLO_QUEUE, RANKED_FLEX_QUEUE] {
            let ids = self.list_history(&puuid, &platform, signal, Some(queue), Some(since), Some(300.0), true).await?;
            let db = self.env.db.lock();
            db.tx(|| {
                for id in &ids {
                    if db.has_match(id) {
                        continue;
                    }
                    db.enqueue(&QueuedMatch { match_id: id.clone(), platform: platform.clone(), kind: "match".into(), puuid: puuid.clone(), source: "me".into(), score: OWN_GAMES_SCORE })?;
                    queued += 1;
                }
                Ok(())
            })?;
        }
        self.env.db.lock().tag_own_games(&puuid)?;
        if queued > 0 {
            (self.env.log)(&format!("Your games: {queued} new ranked matches queued"));
        }
        Ok(())
    }

    /// Work waiting per lane, for the Match Database panel.
    pub fn queued(&self, lane: &LaneKey) -> i64 {
        match lane {
            LaneKey::Regional { platforms, .. } => self.env.db.lock().queue_size().unwrap_or_default().into_iter().filter(|r| platforms.contains(&r.0)).map(|r| r.2).sum(),
            LaneKey::Platform(_) => 0,
        }
    }
}

#[allow(dead_code)]
fn _c() -> crate::config::Config {
    config()
}
