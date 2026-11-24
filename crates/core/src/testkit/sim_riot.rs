//! A simulated Riot web API behind `riot::transport::Transport`, for tests of pacing and the shared-key budget on
//! tokio's paused clock (no sockets, no real key).
//!
//! It counts requests the way Riot's application limits do: per host (routing value), per window, in fixed buckets
//! that start with the first request after the previous bucket ended. Every answer carries X-App-Rate-Limit and
//! X-App-Rate-Limit-Count (and the method pair), and a request that would go over a limit gets a 429 with
//! Retry-After (seconds to the bucket's end) and X-Rate-Limit-Type: application.
//!
//! A second app on the same key (Irelia Companion) is simulated with `add_foreign`: its requests are counted in the
//! same buckets at their own times, without going through the client under test.

use crate::riot::rate_limiter::Clock;
use crate::riot::transport::{RawAnswer, Transport};
use anyhow::Result;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};

pub type Handler = Box<dyn Fn(&str, &str) -> Option<Value> + Send + Sync>;

/// One request the simulator answered: (host, path, ms, status).
#[derive(Clone, Debug, PartialEq)]
pub struct SimCall {
    pub host: String,
    pub path: String,
    pub at: i64,
    pub status: u16,
}

#[derive(Clone, Debug)]
struct Foreign {
    host: String,
    every_ms: i64,
    next_at: i64,
    until: i64,
}

#[derive(Default)]
struct SimState {
    /// host → per window (start, count).
    buckets: HashMap<String, Vec<(i64, u32)>>,
    foreign: Vec<Foreign>,
    scripted: VecDeque<(u16, Vec<(String, String)>)>,
    calls: Vec<SimCall>,
    foreign_sent: u64,
    foreign_rejected: u64,
    /// host → the highest count any bucket of the longest window reached.
    peak: HashMap<String, u32>,
}

pub struct SimRiot {
    clock: Clock,
    /// (limit, window ms).
    limits: Vec<(u32, i64)>,
    key: String,
    handler: Handler,
    st: Mutex<SimState>,
}

impl SimRiot {
    /// `limits` as Riot writes them ("20:1,100:120"); `key` the only key it accepts (401 otherwise).
    pub fn new(clock: Clock, limits: &str, key: &str, handler: Handler) -> SimRiot {
        let limits = crate::config::parse_limits(Some(limits)).iter().map(|w| (w.limit as u32, w.window_ms as i64)).collect();
        SimRiot { clock, limits, key: key.into(), handler, st: Mutex::new(SimState::default()) }
    }

    /// Another app spending the same key on `host`: one request every `every_ms` from `from` to `until` (ms).
    pub fn add_foreign(&self, host: &str, every_ms: i64, from: i64, until: i64) {
        self.st.lock().foreign.push(Foreign { host: host.into(), every_ms, next_at: from, until });
    }

    /// The next answers, whatever the request: (status, headers).
    pub fn script(&self, status: u16, headers: &[(&str, &str)]) {
        self.st.lock().scripted.push_back((status, headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()));
    }

    pub fn calls(&self) -> Vec<SimCall> {
        self.st.lock().calls.clone()
    }
    /// (foreign requests sent, of which rejected with a 429).
    pub fn foreign(&self) -> (u64, u64) {
        let st = self.st.lock();
        (st.foreign_sent, st.foreign_rejected)
    }
    /// The highest count a bucket of the longest window reached on `host` (every app included).
    pub fn peak(&self, host: &str) -> u32 {
        self.st.lock().peak.get(host).copied().unwrap_or(0)
    }
    pub fn limits(&self) -> Vec<(u32, i64)> {
        self.limits.clone()
    }

    /// Counts one request at `t` on `host`, or refuses it (Some(ms until the blocking bucket ends)).
    fn admit(&self, st: &mut SimState, host: &str, t: i64) -> Option<i64> {
        let limits = self.limits.clone();
        let b = st.buckets.entry(host.to_string()).or_insert_with(|| limits.iter().map(|_| (i64::MIN / 2, 0)).collect());
        for (i, (_, w)) in limits.iter().enumerate() {
            if t >= b[i].0 + w {
                b[i] = (t, 0);
            }
        }
        if let Some(i) = limits.iter().enumerate().position(|(i, (l, _))| b[i].1 + 1 > *l) {
            return Some(b[i].0 + limits[i].1 - t);
        }
        for x in b.iter_mut() {
            x.1 += 1;
        }
        let longest = limits.iter().enumerate().max_by_key(|x| x.1 .1).map(|x| x.0).unwrap_or(0);
        let c = b[longest].1;
        let p = st.peak.entry(host.to_string()).or_default();
        *p = (*p).max(c);
        None
    }

    fn run_foreign(&self, st: &mut SimState, upto: i64) {
        loop {
            let next = st.foreign.iter().enumerate().filter(|(_, f)| f.next_at <= upto && f.next_at <= f.until).min_by_key(|(_, f)| f.next_at).map(|(i, f)| (i, f.next_at, f.host.clone()));
            let Some((i, at, host)) = next else { break };
            st.foreign[i].next_at += st.foreign[i].every_ms;
            st.foreign_sent += 1;
            if self.admit(st, &host, at).is_some() {
                st.foreign_rejected += 1;
            }
        }
    }

    fn count_header(&self, st: &SimState, host: &str) -> String {
        let b = st.buckets.get(host);
        self.limits.iter().enumerate().map(|(i, (_, w))| format!("{}:{}", b.map(|b| b[i].1).unwrap_or(0), w / 1000)).collect::<Vec<_>>().join(",")
    }
}

impl Transport for SimRiot {
    async fn get(&self, url: &str, key: &str) -> Result<RawAnswer> {
        let t = (self.clock)() as i64;
        let rest = url.split_once("://").map(|x| x.1).unwrap_or(url);
        let (host, path) = rest.split_once('/').map(|(h, p)| (h.to_string(), format!("/{p}"))).unwrap_or((rest.to_string(), "/".into()));
        let mut st = self.st.lock();
        self.run_foreign(&mut st, t);
        let limits = self.limits.iter().map(|(l, w)| format!("{l}:{}", w / 1000)).collect::<Vec<_>>().join(",");
        let mut headers = vec![("x-app-rate-limit".to_string(), limits), ("x-method-rate-limit".to_string(), "2000:10".to_string())];
        let mut answer = |st: &mut SimState, status: u16, mut extra: Vec<(String, String)>, body: Option<Value>| {
            headers.push(("x-app-rate-limit-count".into(), self.count_header(st, &host)));
            headers.push(("x-method-rate-limit-count".into(), "1:10".into()));
            headers.append(&mut extra);
            st.calls.push(SimCall { host: host.clone(), path: path.clone(), at: t, status });
            Ok(RawAnswer { status, headers: headers.clone(), body })
        };
        if let Some((status, h)) = st.scripted.pop_front() {
            // Riot counts the request whatever it answers.
            let _ = self.admit(&mut st, &host, t);
            return answer(&mut st, status, h, None);
        }
        if key != self.key {
            return answer(&mut st, 401, vec![], None);
        }
        if let Some(wait) = self.admit(&mut st, &host, t) {
            let secs = ((wait + 999) / 1000).max(1).to_string();
            return answer(&mut st, 429, vec![("x-rate-limit-type".into(), "application".into()), ("retry-after".into(), secs)], None);
        }
        drop(st);
        let body = (self.handler)(&host, &path);
        let mut st = self.st.lock();
        match body {
            Some(b) => answer(&mut st, 200, vec![], Some(b)),
            None => answer(&mut st, 404, vec![], None),
        }
    }
}
