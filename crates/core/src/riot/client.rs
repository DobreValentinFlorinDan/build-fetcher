//! The Riot web API client (from irelia-companion): every call goes through a per-host rate limiter (one host per
//! routing value: `europe` for EUN1 + EUW1 match-v5, `asia` for KR, and each platform host for league-v4). Every
//! answer's X-App-Rate-Limit-Count / X-Method-Rate-Limit-Count is fed to that limiter, so requests another app makes
//! with the same key (Irelia Companion) slow this client down instead of tipping the key over its limit. 429s follow
//! Retry-After; a service 429 without one backs off exponentially. Answers are not cached here.
//!
//! The key is only ever sent in the X-Riot-Token header: errors name the status and the path, never the key.

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

pub struct RiotClient<T: Transport = HttpTransport> {
    pub request_timeout_ms: u64,
    pub requests: AtomicU64,
    pub default_pace_ms: f64,
    api_key: Mutex<String>,
    limits: Vec<RateWindow>,
    pacing: PacingOptions,
    limiters: Mutex<indexmap::IndexMap<String, Arc<RateLimiter>>>,
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
            api_key: Mutex::new(api_key.to_string()),
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
    pub fn budget_statuses(&self) -> Vec<BudgetStatus> {
        let limiters: Vec<Arc<RateLimiter>> = self.limiters.lock().values().cloned().collect();
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

    pub fn set_api_key(&self, key: &str) {
        *self.api_key.lock() = key.to_string();
    }
    pub fn on_throttle(&self, f: Option<Arc<dyn Fn(ThrottleInfo) + Send + Sync>>) {
        *self.throttle.lock() = f;
    }

    fn limiter(&self, host: &str) -> Arc<RateLimiter> {
        self.limiters.lock().entry(host.to_string()).or_insert_with(|| RateLimiter::new(host, &self.limits, self.pacing, self.clock.clone())).clone()
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
        let limiter = self.limiter(host);
        let mut attempt = 0;
        loop {
            let permit = limiter.acquire(family, Some(signal)).await?;
            self.requests.fetch_add(1, Ordering::SeqCst);
            let url = match self.base_url.lock().clone() {
                Some(b) => format!("{b}{path}"),
                None => format!("https://{host}{path}"),
            };
            let key = self.api_key.lock().clone();
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
