//! The Riot web API client (from irelia-companion): every call goes through a per-host rate limiter (one host per
//! routing value: `europe` for EUN1 + EUW1 match-v5, `asia` for KR, and each platform host for league-v4). Every
//! answer's X-App-Rate-Limit-Count / X-Method-Rate-Limit-Count is fed to that limiter, so requests another app makes
//! with the same key (Irelia Companion) slow this client down instead of tipping the key over its limit. 429s follow
//! Retry-After; a service 429 without one backs off exponentially. Answers are not cached here.
//!
//! The key is only ever sent in the X-Riot-Token header: errors name the status and the path, never the key.
//!
//! Extra keys (proof of concept, for testing): the client can hold any number of keys. Each key has its own limiter
//! per host, so each keeps its own budget, and every request goes to the least busy key for its host. A key Riot
//! rejects (401 / 403) is set aside and the others carry on; only when every key is rejected do the lanes stop.

use super::rate_limiter::{limit_scope, retry_after_ms, sleep, wall_clock, Clock, Pacer, RateLimiter, Signal};
use super::routing::{account_host, is_apex, platform_host, regional_host};
use super::transport::{HttpTransport, Transport};
use crate::build::BudgetStatus;
use crate::config::{PacingOptions, RateWindow};
use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The key was rejected (401 / 403): the lanes stop.
#[derive(Debug)]
pub struct RiotAuthError(pub String);
impl std::fmt::Display for RiotAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for RiotAuthError {}

#[derive(Clone, Debug, PartialEq)]
pub struct LeagueEntry {
    pub puuid: String,
    pub league_points: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MasteryDto {
    pub champion_id: i64,
    pub champion_points: i64,
    pub last_play_time: Option<i64>,
}

impl MasteryDto {
    pub fn from_json(v: &Value) -> MasteryDto {
        MasteryDto {
            champion_id: v["championId"].as_i64().unwrap_or(0),
            champion_points: v["championPoints"].as_i64().unwrap_or(0),
            last_play_time: v["lastPlayTime"].as_i64(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RankedEntry {
    pub queue_type: String,
    pub tier: String,
    pub league_points: i64,
}

#[derive(Clone, Debug, Default)]
pub struct MatchIdQuery {
    pub queue: i64,
    pub start_time: Option<i64>,
    pub start: i64,
    pub count: i64,
    pub fresh: bool,
}

/// The Riot calls the scraper makes (a fake implements it in tests).
pub trait RiotApi: Send + Sync {
    fn league_entries(&self, platform: &str, tier: &str, divisions: &[String], max_pages: i64, signal: &Signal) -> impl Future<Output = Result<Vec<LeagueEntry>>> + Send;
    fn top_masteries(&self, platform: &str, puuid: &str, count: i64, signal: &Signal, since: Option<i64>) -> impl Future<Output = Result<Vec<MasteryDto>>> + Send;
    fn champion_mastery(&self, platform: &str, puuid: &str, champion_id: i64, signal: &Signal, since: Option<i64>) -> impl Future<Output = Result<Option<MasteryDto>>> + Send;
    fn ranked_entries(&self, platform: &str, puuid: &str, signal: &Signal) -> impl Future<Output = Result<Vec<RankedEntry>>> + Send;
    fn match_ids(&self, platform: &str, puuid: &str, queue: i64, count: i64, signal: &Signal) -> impl Future<Output = Result<Vec<String>>> + Send;
    /// One page of a non-apex league (`/entries/RANKED_SOLO_5x5/{tier}/{division}?page=`); apex tiers have one page.
    /// The default asks `league_entries` for page 1 only (fakes that don't page).
    fn league_page(&self, platform: &str, tier: &str, division: &str, page: i64, signal: &Signal) -> impl Future<Output = Result<Vec<LeagueEntry>>> + Send {
        async move {
            if page == 1 {
                self.league_entries(platform, tier, &[division.to_string()], 1, signal).await
            } else {
                Ok(vec![])
            }
        }
    }
    /// Each routing value's budget as last seen (empty for fakes without one).
    fn budgets(&self) -> Vec<BudgetStatus> {
        vec![]
    }
    /// How many requests the fetcher may have going at once on one routing value: one per usable key (1 for fakes).
    fn parallelism(&self) -> usize {
        1
    }
    fn match_ids_page(&self, platform: &str, puuid: &str, q: &MatchIdQuery, signal: &Signal) -> impl Future<Output = Result<Vec<String>>> + Send;
    fn get_match(&self, platform: &str, id: &str, signal: &Signal) -> impl Future<Output = Result<Option<Value>>> + Send;
    fn timeline(&self, platform: &str, id: &str, signal: &Signal) -> impl Future<Output = Result<Option<Value>>> + Send;
    fn account_region(&self, puuid: &str, signal: &Signal) -> impl Future<Output = Result<Option<String>>> + Send;
    fn account_by_riot_id(&self, game_name: &str, tag_line: &str, host: &str, signal: &Signal) -> impl Future<Output = Result<Option<String>>> + Send;
}

const MAX_ATTEMPTS: u32 = 5;
pub const REQUEST_TIMEOUT_MS: u64 = 30_000;

pub struct ThrottleInfo {
    pub host: String,
    pub family: &'static str,
    pub scope: &'static str,
    pub wait_ms: f64,
}

/// One API key and whether Riot has rejected it since it was set.
#[derive(Clone)]
struct KeySlot {
    key: String,
    rejected: bool,
}

fn slots(keys: &[String]) -> Vec<KeySlot> {
    let mut out: Vec<KeySlot> = Vec::new();
    for k in keys.iter().map(|k| k.trim()).filter(|k| !k.is_empty()) {
        if !out.iter().any(|s| s.key == k) {
            out.push(KeySlot { key: k.to_string(), rejected: false });
        }
    }
    if out.is_empty() {
        // No key: requests still go out (with an empty key) and Riot's 401 says so, as before.
        out.push(KeySlot { key: String::new(), rejected: false });
    }
    out
}

/// Limiters by (key, host).
type Limiters = indexmap::IndexMap<(String, String), Arc<RateLimiter>>;

pub struct RiotClient<T: Transport = HttpTransport> {
    pub request_timeout_ms: u64,
    pub requests: AtomicU64,
    pub default_pace_ms: f64,
    keys: Mutex<Vec<KeySlot>>,
    limits: Vec<RateWindow>,
    pacing: PacingOptions,
    /// One limiter per (key, host). The map's key is internal only: nothing shows or logs it.
    limiters: Mutex<Limiters>,
    throttle: Mutex<Option<Arc<dyn Fn(ThrottleInfo) + Send + Sync>>>,
    http: T,
    clock: Clock,
    base_url: Mutex<Option<String>>,
}

impl RiotClient<HttpTransport> {
    pub fn new(api_key: &str, limits: Vec<RateWindow>, pacing: PacingOptions) -> RiotClient {
        RiotClient::with_transport(api_key, limits, pacing, HttpTransport::default(), wall_clock())
    }
}

impl<T: Transport> RiotClient<T> {
    /// A client over any transport and clock (tests: a simulated Riot on tokio's paused clock).
    pub fn with_transport(api_key: &str, limits: Vec<RateWindow>, pacing: PacingOptions, transport: T, clock: Clock) -> RiotClient<T> {
        RiotClient {
            request_timeout_ms: REQUEST_TIMEOUT_MS,
            requests: AtomicU64::new(0),
            default_pace_ms: Pacer::new(&limits, pacing).spacing(clock()),
            keys: Mutex::new(slots(&[api_key.to_string()])),
            limits,
            pacing,
            limiters: Mutex::new(indexmap::IndexMap::new()),
            throttle: Mutex::new(None),
            http: transport,
            clock,
            base_url: Mutex::new(None),
        }
    }

    /// Tests: send every request to `base` (http://127.0.0.1:port) instead of https://host.
    pub fn with_base_url(self, base: &str) -> RiotClient<T> {
        *self.base_url.lock() = Some(base.to_string());
        self
    }

    /// The transport (tests inspect their fake through it).
    pub fn transport(&self) -> &T {
        &self.http
    }

    /// Each host's application budget as the dashboard meter shows it: Riot's count in the longest window (every
    /// app on the key), how much of it was ours, and whether another app is spending the same key.
    /// With several keys, a host's meter adds up every key's budget ("20:1,100:120 × 3 keys").
    pub fn budget_statuses(&self) -> Vec<BudgetStatus> {
        let mut by_host: indexmap::IndexMap<String, (BudgetStatus, usize)> = indexmap::IndexMap::new();
        for b in self.key_budget_statuses() {
            match by_host.get_mut(&b.scope) {
                Some((sum, n)) => {
                    sum.used += b.used;
                    sum.ours += b.ours;
                    sum.limit += b.limit;
                    sum.shared |= b.shared;
                    *n += 1;
                }
                None => {
                    by_host.insert(b.scope.clone(), (b, 1));
                }
            }
        }
        by_host
            .into_values()
            .map(|(mut b, n)| {
                if n > 1 {
                    b.limits = format!("{} × {n} keys", b.limits);
                }
                b
            })
            .collect()
    }

    /// One budget per (usable key, host): a key Riot rejected adds nothing to the budget (with every key rejected,
    /// they are all shown).
    fn key_budget_statuses(&self) -> Vec<BudgetStatus> {
        let keys: Vec<String> = {
            let slots = self.keys.lock();
            let usable: Vec<String> = slots.iter().filter(|k| !k.rejected).map(|k| k.key.clone()).collect();
            if usable.is_empty() {
                slots.iter().map(|k| k.key.clone()).collect()
            } else {
                usable
            }
        };
        let limiters: Vec<Arc<RateLimiter>> = self.limiters.lock().iter().filter(|((k, _), _)| keys.contains(k)).map(|(_, l)| l.clone()).collect();
        limiters
            .iter()
            .map(|l| {
                let windows = l.app_windows();
                let limits = windows.iter().map(|w| format!("{}:{}", w.limit, w.window_ms / 1000.0)).collect::<Vec<_>>().join(",");
                let longest = windows.iter().copied().fold(None, |m: Option<super::rate_limiter::WindowUsage>, w| match m {
                    Some(m) if m.window_ms >= w.window_ms => Some(m),
                    _ => Some(w),
                });
                BudgetStatus {
                    scope: budget_scope(&l.host),
                    limits,
                    used: longest.map(|w| w.used as u32).unwrap_or(0),
                    ours: longest.map(|w| w.ours as u32).unwrap_or(0),
                    limit: longest.map(|w| w.limit as u32).unwrap_or(0),
                    window_s: longest.map(|w| (w.window_ms / 1000.0) as u32).unwrap_or(0),
                    shared: windows.iter().any(|w| w.foreign > 0.0),
                }
            })
            .collect()
    }

    /// One key (the old single-key setup). Same as `set_api_keys(&[key])`.
    pub fn set_api_key(&self, key: &str) {
        self.set_api_keys(&[key.to_string()]);
    }

    /// Every key to spread requests over: the main key first, then any extra keys. Blank and repeated keys are
    /// skipped. Rejections are forgotten (a new key list gets a fresh chance). A key kept from the old list keeps its
    /// limiters, so its budget carries over.
    pub fn set_api_keys(&self, keys: &[String]) {
        let next = slots(keys);
        let keep: Vec<String> = next.iter().map(|k| k.key.clone()).collect();
        self.limiters.lock().retain(|(k, _), _| keep.contains(k));
        *self.keys.lock() = next;
    }

    /// Keys in use that Riot hasn't rejected (at least 1).
    pub fn key_count(&self) -> usize {
        self.keys.lock().iter().filter(|k| !k.rejected).count().max(1)
    }
    pub fn on_throttle(&self, f: Option<Arc<dyn Fn(ThrottleInfo) + Send + Sync>>) {
        *self.throttle.lock() = f;
    }

    fn limiter(&self, key: &str, host: &str) -> Arc<RateLimiter> {
        self.limiters
            .lock()
            .entry((key.to_string(), host.to_string()))
            .or_insert_with(|| RateLimiter::new(host, &self.limits, self.pacing, self.clock.clone()))
            .clone()
    }

    /// The key (and its limiter for `host`) the next request should use: the least busy key Riot hasn't rejected,
    /// the main key on a tie. None when every key was rejected.
    fn pick(&self, host: &str, family: &'static str) -> Option<(String, Arc<RateLimiter>)> {
        let keys: Vec<String> = self.keys.lock().iter().filter(|k| !k.rejected).map(|k| k.key.clone()).collect();
        let mut best: Option<((usize, f64), String, Arc<RateLimiter>)> = None;
        for k in keys {
            let l = self.limiter(&k, host);
            let load = l.load(family);
            let better = match &best {
                None => true,
                Some((b, _, _)) => load.0 < b.0 || (load.0 == b.0 && load.1 < b.1),
            };
            if better {
                best = Some((load, k, l));
            }
        }
        best.map(|(_, k, l)| (k, l))
    }

    /// Riot rejected `key`: later requests use the other keys. True when another key is left.
    fn reject(&self, key: &str) -> bool {
        let mut keys = self.keys.lock();
        for k in keys.iter_mut().filter(|k| k.key == key) {
            k.rejected = true;
        }
        keys.iter().any(|k| !k.rejected)
    }

    /// Window usage of the busiest host.
    pub fn usage(&self) -> Vec<Value> {
        let all: Vec<Vec<Value>> = self.limiters.lock().values().map(|l| l.usage()).collect();
        if all.is_empty() {
            return self.limits.iter().map(|l| json!({ "used": 0, "limit": crate::js::num(l.limit), "windowSec": crate::js::num(l.window_ms / 1000.0) })).collect();
        }
        let used = |v: &Vec<Value>| v.iter().map(|w| w["used"].as_f64().unwrap_or(0.0)).sum::<f64>();
        let mut best = all[0].clone();
        for b in all.into_iter().skip(1) {
            if used(&b) > used(&best) {
                best = b;
            }
        }
        best
    }

    /// Live pacing state of every host used so far (RateSnapshot without at / idleUntil).
    pub fn rate_state(&self, now: f64) -> Value {
        let limiters: Vec<Arc<RateLimiter>> = self.limiters.lock().values().cloned().collect();
        let hosts: Vec<Value> = limiters.iter().map(|l| l.host_state(now)).collect();
        let used = |h: &Value| h["windows"].as_array().map(|a| a.iter().map(|w| w["used"].as_f64().unwrap_or(0.0)).sum::<f64>()).unwrap_or(0.0);
        let mut busiest: Option<&Value> = None;
        for h in &hosts {
            if busiest.map(|b| used(h) > used(b)).unwrap_or(true) {
                busiest = Some(h);
            }
        }
        let f = |h: &Value, k: &str| h[k].as_f64().unwrap_or(0.0);
        let mut recent: Vec<&Value> = hosts.iter().filter(|h| f(h, "queued") > 0.0 || f(h, "inFlight") > 0.0 || now - f(h, "lastStartAt") < 3000.0).collect();
        recent.sort_by(|a, b| crate::js::js_order(f(b, "lastStartAt") - f(a, "lastStartAt")));
        json!({
            "hosts": hosts.clone(),
            "busiest": busiest.map(|b| b["host"].clone()),
            "active": recent.first().map(|r| r["host"].clone()),
            "defaultPaceMs": crate::js::num(self.default_pace_ms),
        })
    }

    async fn get(&self, host: &str, family: &'static str, path: &str, signal: &Signal) -> Result<Option<Value>> {
        let mut attempt = 0;
        loop {
            let Some((key, limiter)) = self.pick(host, family) else {
                return Err(anyhow!(RiotAuthError("Riot API rejected every key. Check the keys in Settings (developer.riotgames.com).".into())));
            };
            let permit = limiter.acquire(family, Some(signal)).await?;
            self.requests.fetch_add(1, Ordering::SeqCst);
            let url = match self.base_url.lock().clone() {
                Some(b) => format!("{b}{path}"),
                None => format!("https://{host}{path}"),
            };
            let sent_at = permit.started_at;
            let send = async {
                let res = self.http.get(&url, &key).await?;
                let h = |k: &str| res.header(k).map(String::from);
                limiter.update_limits(family, h("x-app-rate-limit").as_deref(), h("x-method-rate-limit").as_deref());
                // Every answer, 429s included: Riot's counts are how another app's use of the key shows up.
                limiter.observe_counts(family, h("x-app-rate-limit-count").as_deref(), h("x-method-rate-limit-count").as_deref(), sent_at);
                Ok::<_, anyhow::Error>((res.status, res.body.clone(), h("x-rate-limit-type"), h("retry-after")))
            };
            let outcome = tokio::select! {
                r = tokio::time::timeout(Duration::from_millis(self.request_timeout_ms), send) => match r {
                    Ok(r) => r,
                    Err(_) => Err(anyhow!("The operation was aborted due to timeout")),
                },
                _ = signal.cancelled() => Err(anyhow!(super::rate_limiter::Aborted(signal.reason().unwrap_or_default()))),
            };
            permit.done();
            let (status, body, kind, retry) = match outcome {
                Ok(x) => x,
                Err(e) => {
                    if signal.aborted() || attempt + 1 >= MAX_ATTEMPTS {
                        return Err(e);
                    }
                    sleep(1000.0 * 2f64.powi(attempt as i32), Some(signal)).await?;
                    attempt += 1;
                    continue;
                }
            };
            if (200..300).contains(&status) {
                limiter.succeeded();
                return Ok(body);
            }
            if status == 404 {
                limiter.succeeded();
                return Ok(None);
            }
            if status == 401 || status == 403 {
                // Another key may still work: try it (this doesn't count as an attempt; each key is rejected once).
                if self.reject(&key) {
                    continue;
                }
                return Err(anyhow!(RiotAuthError(format!(
                    "Riot API rejected the key ({status}). Check the key in Settings (developer.riotgames.com)."
                ))));
            }
            if status == 429 {
                let scope = limit_scope(kind.as_deref());
                let wait_ms = limiter.rate_limited(family, scope, retry_after_ms(retry.as_deref()));
                if let Some(t) = self.throttle.lock().clone() {
                    t(ThrottleInfo { host: host.into(), family, scope, wait_ms });
                }
                if attempt + 1 >= MAX_ATTEMPTS {
                    return Err(anyhow!("Riot API 429 on {path}"));
                }
                attempt += 1;
                continue;
            }
            if status < 500 || attempt + 1 >= MAX_ATTEMPTS {
                return Err(anyhow!("Riot API {status} on {path}"));
            }
            sleep(1000.0 * 2f64.powi(attempt as i32), Some(signal)).await?;
            attempt += 1;
        }
    }

    /// GET (the `fresh` / `since` arguments are kept for the call sites; nothing is cached).
    #[allow(clippy::too_many_arguments)]
    async fn cached(&self, kind: &'static str, _scope: &str, _key: &str, host: &str, path: &str, signal: &Signal, _fresh: bool, _since: Option<i64>) -> Result<Option<Value>> {
        self.get(host, kind, path, signal).await
    }
}

fn hex(s: &str) -> String {
    s.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

fn enc(s: &str) -> String {
    // encodeURIComponent
    let mut out = String::new();
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || "-_.!~*'()".contains(c) {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The dashboard's name for a host's budget: "EUROPE", "ASIA" for regional hosts, the platform ("euw1") otherwise.
pub fn budget_scope(host: &str) -> String {
    let first = host.split('.').next().unwrap_or(host);
    match first {
        "europe" | "asia" | "americas" | "sea" => first.to_uppercase(),
        _ => first.to_string(),
    }
}

impl<T: Transport> RiotApi for RiotClient<T> {
    async fn league_page(&self, platform: &str, tier: &str, division: &str, page: i64, signal: &Signal) -> Result<Vec<LeagueEntry>> {
        if is_apex(tier) {
            return if page == 1 { self.league_entries(platform, tier, &[], 1, signal).await } else { Ok(vec![]) };
        }
        let host = platform_host(platform);
        let res = self.get(&host, "league", &format!("/lol/league/v4/entries/RANKED_SOLO_5x5/{tier}/{division}?page={page}"), signal).await?;
        let list: Vec<Value> = res.and_then(|r| r.as_array().cloned()).unwrap_or_default();
        Ok(list.iter().map(|v| LeagueEntry { puuid: v["puuid"].as_str().unwrap_or("").to_string(), league_points: v["leaguePoints"].as_i64().unwrap_or(0) }).collect())
    }

    fn budgets(&self) -> Vec<BudgetStatus> {
        self.budget_statuses()
    }

    fn parallelism(&self) -> usize {
        self.key_count()
    }

    async fn league_entries(&self, platform: &str, tier: &str, divisions: &[String], max_pages: i64, signal: &Signal) -> Result<Vec<LeagueEntry>> {
        let host = platform_host(platform);
        let parse = |v: &Value| LeagueEntry { puuid: v["puuid"].as_str().unwrap_or("").to_string(), league_points: v["leaguePoints"].as_i64().unwrap_or(0) };
        if is_apex(tier) {
            let res = self.cached("league", platform, tier, &host, &format!("/lol/league/v4/{}leagues/by-queue/RANKED_SOLO_5x5", tier.to_lowercase()), signal, false, None).await?;
            return Ok(res.and_then(|r| r["entries"].as_array().map(|a| a.iter().map(parse).collect())).unwrap_or_default());
        }
        let mut out = Vec::new();
        for division in divisions {
            for page in 1..=max_pages {
                let res = self
                    .cached("league", platform, &format!("{tier}-{division}-p{page}"), &host, &format!("/lol/league/v4/entries/RANKED_SOLO_5x5/{tier}/{division}?page={page}"), signal, false, None)
                    .await?;
                let list: Vec<Value> = res.and_then(|r| r.as_array().cloned()).unwrap_or_default();
                if list.is_empty() {
                    break;
                }
                out.extend(list.iter().map(parse));
            }
        }
        Ok(out)
    }

    async fn top_masteries(&self, platform: &str, puuid: &str, count: i64, signal: &Signal, since: Option<i64>) -> Result<Vec<MasteryDto>> {
        let r = self
            .cached("mastery", platform, &format!("{puuid}-top{count}"), &platform_host(platform), &format!("/lol/champion-mastery/v4/champion-masteries/by-puuid/{puuid}/top?count={count}"), signal, false, since)
            .await?;
        Ok(r.and_then(|v| v.as_array().map(|a| a.iter().map(MasteryDto::from_json).collect())).unwrap_or_default())
    }

    async fn champion_mastery(&self, platform: &str, puuid: &str, champion_id: i64, signal: &Signal, since: Option<i64>) -> Result<Option<MasteryDto>> {
        let r = self
            .cached("mastery", platform, &format!("{puuid}-c{champion_id}"), &platform_host(platform), &format!("/lol/champion-mastery/v4/champion-masteries/by-puuid/{puuid}/by-champion/{champion_id}"), signal, false, since)
            .await?;
        Ok(r.map(|v| MasteryDto::from_json(&v)))
    }

    async fn ranked_entries(&self, platform: &str, puuid: &str, signal: &Signal) -> Result<Vec<RankedEntry>> {
        let r = self.cached("league", platform, &format!("by-puuid-{puuid}"), &platform_host(platform), &format!("/lol/league/v4/entries/by-puuid/{puuid}"), signal, false, None).await?;
        Ok(r
            .and_then(|v| {
                v.as_array().map(|a| {
                    a.iter()
                        .map(|e| RankedEntry {
                            queue_type: e["queueType"].as_str().unwrap_or("").into(),
                            tier: e["tier"].as_str().unwrap_or("").into(),
                            league_points: e["leaguePoints"].as_i64().unwrap_or(0),
                        })
                        .collect()
                })
            })
            .unwrap_or_default())
    }

    async fn match_ids(&self, platform: &str, puuid: &str, queue: i64, count: i64, signal: &Signal) -> Result<Vec<String>> {
        let r = self
            .cached("matchIds", platform, &format!("{puuid}-{queue}-{count}"), regional_host(platform), &format!("/lol/match/v5/matches/by-puuid/{puuid}/ids?queue={queue}&type=ranked&start=0&count={count}"), signal, false, None)
            .await?;
        Ok(ids(r))
    }

    async fn match_ids_page(&self, platform: &str, puuid: &str, q: &MatchIdQuery, signal: &Signal) -> Result<Vec<String>> {
        let since = q.start_time.map(|s| format!("&startTime={s}")).unwrap_or_default();
        let r = self
            .cached(
                "matchIds",
                platform,
                &format!("{puuid}-{}-{}-{}-{}", q.queue, q.start_time.unwrap_or(0), q.start, q.count),
                regional_host(platform),
                &format!("/lol/match/v5/matches/by-puuid/{puuid}/ids?queue={}{since}&start={}&count={}", q.queue, q.start, q.count),
                signal,
                q.fresh,
                None,
            )
            .await?;
        Ok(ids(r))
    }

    async fn get_match(&self, platform: &str, id: &str, signal: &Signal) -> Result<Option<Value>> {
        self.cached("match", platform, id, regional_host(platform), &format!("/lol/match/v5/matches/{id}"), signal, false, None).await
    }

    async fn timeline(&self, platform: &str, id: &str, signal: &Signal) -> Result<Option<Value>> {
        self.cached("timeline", platform, id, regional_host(platform), &format!("/lol/match/v5/matches/{id}/timeline"), signal, false, None).await
    }

    async fn account_region(&self, puuid: &str, signal: &Signal) -> Result<Option<String>> {
        let r = self.cached("account", "lol", puuid, "europe.api.riotgames.com", &format!("/riot/account/v1/region/by-game/lol/by-puuid/{puuid}"), signal, false, None).await?;
        Ok(r.and_then(|v| v["region"].as_str().map(String::from)))
    }

    async fn account_by_riot_id(&self, game_name: &str, tag_line: &str, host: &str, signal: &Signal) -> Result<Option<String>> {
        let key = hex(&format!("{game_name}#{tag_line}").to_lowercase());
        let r = self.cached("account", "riotid", &key, host, &format!("/riot/account/v1/accounts/by-riot-id/{}/{}", enc(game_name), enc(tag_line)), signal, false, None).await?;
        Ok(r.and_then(|v| v["puuid"].as_str().map(String::from)))
    }
}

fn ids(r: Option<Value>) -> Vec<String> {
    r.and_then(|v| v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())).unwrap_or_default()
}

#[allow(dead_code)]
fn _host(p: Option<&str>) -> &'static str {
    account_host(p)
}

#[cfg(test)]
mod tests {
    //! The client against a simulated Riot (testkit::SimRiot) on tokio's paused clock.
    use super::*;
    use crate::config::parse_limits;
    use crate::testkit::SimRiot;

    const KEY: &str = "RGAPI-00000000-test";
    const EUROPE: &str = "europe.api.riotgames.com";
    const PACING: PacingOptions = PacingOptions { safety: 0.9, burst: 3.0, concurrency: 3, slowdown: 0.8, recovery_ms: 60_000.0 };

    fn clock() -> Clock {
        let start = tokio::time::Instant::now();
        Arc::new(move || (tokio::time::Instant::now() - start).as_secs_f64() * 1000.0)
    }

    fn sim(clock: &Clock) -> SimRiot {
        SimRiot::new(clock.clone(), "20:1,100:120", KEY, Box::new(|_, path| Some(json!({ "path": path }))))
    }

    fn client(sim: SimRiot, clock: Clock, key: &str) -> Arc<RiotClient<SimRiot>> {
        Arc::new(RiotClient::with_transport(key, parse_limits(Some("20:1,100:120")), PACING, sim, clock))
    }

    /// Three tasks asking for matches as fast as the client lets them, for `ms`.
    async fn hammer(c: &Arc<RiotClient<SimRiot>>, clock: &Clock, ms: f64) {
        let mut tasks = vec![];
        for _ in 0..3 {
            let (c, clock) = (c.clone(), clock.clone());
            tasks.push(tokio::spawn(async move {
                let s = Signal::new();
                while clock() < ms {
                    c.get_match("EUW1", "EUW1_1", &s).await.unwrap();
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn alone_on_the_key_we_use_most_of_the_budget() {
        let clock = clock();
        let c = client(sim(&clock), clock.clone(), KEY);
        hammer(&c, &clock, 600_000.0).await;
        let calls = c.transport().calls();
        assert!(calls.iter().all(|x| x.status == 200));
        assert!(calls.len() >= 440, "{} requests in 10 minutes", calls.len());
        eprintln!("alone: {} requests in 10 min, peak bucket {}", calls.len(), c.transport().peak(EUROPE));
        assert!(c.transport().peak(EUROPE) <= 100);
        let b = c.budget_statuses();
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].scope.as_str(), b[0].limit, b[0].window_s, b[0].shared), ("EUROPE", 100, 120, false));
        assert_eq!(b[0].limits, "20:1,100:120");
    }

    #[tokio::test(start_paused = true)]
    async fn another_app_on_the_same_key_slows_us_down_and_nothing_goes_over() {
        let clock = clock();
        let s = sim(&clock);
        // Irelia Companion: 40 requests every 2 minutes on the same routing value, for 10 minutes.
        s.add_foreign(EUROPE, 3000, 0, 600_000);
        let c = client(s, clock.clone(), KEY);
        hammer(&c, &clock, 600_000.0).await;
        let calls = c.transport().calls();
        let ours = calls.len();
        assert_eq!(calls.iter().filter(|x| x.status == 429).count(), 0, "we never tipped the key over");
        assert_eq!(c.transport().foreign().1, 0, "nor made the other app hit the limit");
        assert!(c.transport().peak(EUROPE) <= 100);
        // About (92 - 40) per 2 minutes is left for us.
        assert!((180..=330).contains(&ours), "{ours} requests in 10 minutes");
        let b = &c.budget_statuses()[0];
        assert!(b.shared, "{b:?}");
        assert!(b.used > b.ours, "{b:?}");
        // The other app stops: we speed up again.
        let before = c.transport().calls().len();
        hammer(&c, &clock, 900_000.0).await;
        let after = c.transport().calls().len() - before;
        assert!(after >= 190, "{after} requests in the 5 minutes after");
        assert!(!c.budget_statuses()[0].shared);
        eprintln!("shared key: {ours} requests in 10 min beside the other app's {}; {after} in the 5 min after it stopped", c.transport().foreign().0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_by_the_other_app_blocks_us_until_its_window_ends() {
        let clock = clock();
        let s = sim(&clock);
        // 95 requests in the first 10 s.
        s.add_foreign(EUROPE, 105, 0, 9_990);
        let c = client(s, clock.clone(), KEY);
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        let sig = Signal::new();
        c.get_match("EUW1", "EUW1_1", &sig).await.unwrap();
        let t = clock();
        c.get_match("EUW1", "EUW1_1", &sig).await.unwrap();
        let calls = c.transport().calls();
        assert!(calls.iter().all(|x| x.status == 200), "{calls:?}");
        assert!(calls[1].at as f64 >= t + 100_000.0, "waited for the window: {} → {}", t, calls[1].at);
    }

    #[tokio::test(start_paused = true)]
    async fn application_429_follows_retry_after() {
        let clock = clock();
        let s = sim(&clock);
        s.script(429, &[("x-rate-limit-type", "application"), ("retry-after", "7")]);
        let c = client(s, clock.clone(), KEY);
        let r = c.get_match("EUW1", "EUW1_1", &Signal::new()).await.unwrap();
        assert!(r.is_some());
        let calls = c.transport().calls();
        assert_eq!(calls.iter().map(|x| x.status).collect::<Vec<_>>(), vec![429, 200]);
        assert!(calls[1].at - calls[0].at >= 7000, "{calls:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn service_429_without_retry_after_backs_off_exponentially() {
        let clock = clock();
        let s = sim(&clock);
        s.script(429, &[("x-rate-limit-type", "service")]);
        s.script(429, &[]);
        let c = client(s, clock.clone(), KEY);
        c.get_match("EUW1", "EUW1_1", &Signal::new()).await.unwrap();
        let calls = c.transport().calls();
        assert_eq!(calls.iter().map(|x| x.status).collect::<Vec<_>>(), vec![429, 429, 200]);
        assert!(calls[1].at - calls[0].at >= 1000);
        assert!(calls[2].at - calls[1].at >= 2000);
    }

    #[tokio::test(start_paused = true)]
    async fn errors_never_carry_the_key() {
        let clock = clock();
        let other = "RGAPI-11111111-test";
        let c = client(sim(&clock), clock.clone(), other);
        let e = c.get_match("EUW1", "EUW1_1", &Signal::new()).await.unwrap_err();
        assert!(e.downcast_ref::<RiotAuthError>().is_some());
        assert!(!format!("{e:?}").contains("11111111"), "{e:?}");
        let s = sim(&clock);
        for _ in 0..5 {
            s.script(500, &[]);
        }
        let c = client(s, clock.clone(), KEY);
        let e = c.get_match("EUW1", "EUW1_1", &Signal::new()).await.unwrap_err();
        assert!(!format!("{e:?}").contains("00000000"), "{e:?}");
        assert_eq!(budget_scope("eun1.api.riotgames.com"), "eun1");
        assert_eq!(budget_scope("asia.api.riotgames.com"), "ASIA");
    }

    #[tokio::test(start_paused = true)]
    async fn a_rejected_extra_key_is_set_aside_and_the_others_carry_on() {
        let clock = clock();
        let bad = "RGAPI-22222222-test";
        let c = client(sim(&clock), clock.clone(), bad);
        c.set_api_keys(&[bad.to_string(), KEY.to_string(), KEY.to_string(), " ".to_string()]);
        assert_eq!(c.key_count(), 2, "repeats and blanks are skipped");
        assert_eq!(c.parallelism(), 2);
        let r = c.get_match("EUW1", "EUW1_1", &Signal::new()).await.unwrap();
        assert!(r.is_some());
        assert_eq!(c.transport().calls().iter().map(|x| x.status).collect::<Vec<_>>(), vec![401, 200]);
        assert_eq!(c.key_count(), 1);
        c.get_match("EUW1", "EUW1_2", &Signal::new()).await.unwrap();
        assert_eq!(c.transport().calls().last().unwrap().status, 200, "the rejected key isn't tried again");
        assert_eq!(c.budget_statuses().len(), 1);
        assert!(!c.budget_statuses()[0].limits.contains("keys"), "a rejected key adds nothing to the budget: {}", c.budget_statuses()[0].limits);
        // Every key rejected: the auth error, which never carries a key.
        c.set_api_keys(&[bad.to_string()]);
        let e = c.get_match("EUW1", "EUW1_3", &Signal::new()).await.unwrap_err();
        assert!(e.downcast_ref::<RiotAuthError>().is_some());
        assert!(!format!("{e:?}").contains("22222222"), "{e:?}");
    }
}
