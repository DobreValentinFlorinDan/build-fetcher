//! Port of `server/riot/client.ts`: every call is disk-cache first; misses go through a per-host rate limiter.

use super::disk_cache::DiskCache;
use super::rate_limiter::{limit_scope, retry_after_ms, sleep, wall_clock, Clock, Pacer, RateLimiter, Signal};
use super::routing::{account_host, is_apex, platform_host, regional_host};
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

pub struct RiotClient {
    pub request_timeout_ms: u64,
    pub requests: AtomicU64,
    pub default_pace_ms: f64,
    api_key: Mutex<String>,
    cache: Arc<Mutex<DiskCache>>,
    limits: Vec<RateWindow>,
    pacing: PacingOptions,
    limiters: Mutex<indexmap::IndexMap<String, Arc<RateLimiter>>>,
    throttle: Mutex<Option<Arc<dyn Fn(ThrottleInfo) + Send + Sync>>>,
    http: reqwest::Client,
    clock: Clock,
    base_url: Mutex<Option<String>>,
}

impl RiotClient {
    pub fn new(api_key: &str, cache: Arc<Mutex<DiskCache>>, limits: Vec<RateWindow>, pacing: PacingOptions) -> RiotClient {
        let clock = wall_clock();
        RiotClient {
            request_timeout_ms: REQUEST_TIMEOUT_MS,
            requests: AtomicU64::new(0),
            default_pace_ms: Pacer::new(&limits, pacing).spacing(clock()),
            api_key: Mutex::new(api_key.to_string()),
            cache,
            limits,
            pacing,
            limiters: Mutex::new(indexmap::IndexMap::new()),
            throttle: Mutex::new(None),
            http: reqwest::Client::builder().gzip(true).build().expect("http client"),
            clock,
            base_url: Mutex::new(None),
        }
    }

    /// Tests: send every request to `base` (http://127.0.0.1:port) instead of https://host.
    pub fn with_base_url(self, base: &str) -> RiotClient {
        *self.base_url.lock() = Some(base.to_string());
        self
    }

    pub fn cache_hits(&self) -> u64 {
        self.cache.lock().hits
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
        recent.sort_by(|a, b| crate::engine::recommend::js_order(f(b, "lastStartAt") - f(a, "lastStartAt")));
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
            let send = async {
                let res = self.http.get(&url).header("X-Riot-Token", key).send().await?;
                let headers = res.headers().clone();
                let status = res.status().as_u16();
                let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).map(String::from);
                limiter.update_limits(family, h("x-app-rate-limit").as_deref(), h("x-method-rate-limit").as_deref());
                let body = if (200..300).contains(&status) { Some(res.json::<Value>().await?) } else { None };
                Ok::<_, anyhow::Error>((status, body, h("x-rate-limit-type"), h("retry-after")))
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
                    "Riot API rejected the key ({status}). Development keys expire every 24h - update RIOT_API_KEY in .env."
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

    /// Cache-first GET. `None` (404) results are not cached.
    #[allow(clippy::too_many_arguments)]
    async fn cached(&self, kind: &'static str, scope: &str, key: &str, host: &str, path: &str, signal: &Signal, fresh: bool, since: Option<i64>) -> Result<Option<Value>> {
        if !fresh {
            if let Some(hit) = self.cache.lock().get(kind, scope, key, since) {
                return Ok(Some(hit));
            }
        }
        let value = self.get(host, kind, path, signal).await?;
        if let Some(v) = &value {
            self.cache.lock().set(kind, scope, key, v)?;
        }
        Ok(value)
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

impl RiotApi for RiotClient {
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
