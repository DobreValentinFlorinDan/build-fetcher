//! From irelia-companion's `riot/rate_limiter.rs`: Riot API request pacing, one RateLimiter per host. Request starts are spaced
//! evenly (GCRA per window, a sliding log as backstop), method limits pace each endpoint family on top, waiters form
//! one FIFO queue per host and at most `concurrency` requests are in flight.

use crate::config::{PacingOptions, RateWindow};
use crate::js::{js_max, js_min};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub use crate::config::parse_limits;

/// X-Rate-Limit-Type of a 429: application / method used up a budget; service (or none) means Riot is busy.
pub fn limit_scope(header: Option<&str>) -> &'static str {
    match header {
        Some("application") => "application",
        Some("method") => "method",
        _ => "service",
    }
}

/// Retry-After is in seconds.
pub fn retry_after_ms(header: Option<&str>) -> Option<f64> {
    let h = header?;
    if h.is_empty() {
        return None;
    }
    let s = crate::js::to_number(&Value::String(h.into()));
    (s.is_finite() && s >= 0.0).then_some(s * 1000.0)
}

const MIN_PACE_FACTOR: f64 = 0.25;
const UNKNOWN_LIMITS_HORIZON_MS: f64 = 600_000.0;
const SERVICE_BACKOFF_MAX_MS: f64 = 30_000.0;
const DEFAULT_RETRY_AFTER_MS: f64 = 5_000.0;

#[derive(Clone, Debug)]
struct Lane {
    limit: f64,
    window_ms: f64,
    cap: f64,
    burst: f64,
    interval: f64,
    tat: f64,
}

/// Paces one scope: the app limits of a host, or one endpoint family's method limits on that host.
#[derive(Clone, Debug)]
pub struct Pacer {
    lanes: Vec<Lane>,
    starts: VecDeque<f64>,
    blocked_until: f64,
    slow: (f64, f64),
    o: PacingOptions,
}

impl Pacer {
    pub fn new(limits: &[RateWindow], o: PacingOptions) -> Pacer {
        let mut p = Pacer { lanes: vec![], starts: VecDeque::new(), blocked_until: 0.0, slow: (1.0, 0.0), o };
        p.set_limits(limits);
        p
    }

    pub fn limits(&self) -> Vec<RateWindow> {
        self.lanes.iter().map(|l| RateWindow { limit: l.limit, window_ms: l.window_ms }).collect()
    }

    pub fn set_limits(&mut self, limits: &[RateWindow]) -> bool {
        if limits.is_empty() {
            return false;
        }
        let same = limits.len() == self.lanes.len() && limits.iter().zip(&self.lanes).all(|(l, x)| l.limit == x.limit && l.window_ms == x.window_ms);
        if same {
            return false;
        }
        self.lanes = limits
            .iter()
            .map(|l| {
                let cap = js_max(1.0, (l.limit * self.o.safety).floor());
                let burst = js_max(1.0, js_min(self.o.burst.floor(), l.limit - cap + 1.0));
                let mut lane = Lane { limit: l.limit, window_ms: l.window_ms, cap, burst, interval: l.window_ms / cap, tat: 0.0 };
                for t in &self.starts {
                    lane.tat = js_max(lane.tat, *t) + lane.interval;
                }
                lane
            })
            .collect();
        true
    }

    pub fn factor(&self, now: f64) -> f64 {
        let climbed = self.slow.0 + (js_max(0.0, now - self.slow.1) * (1.0 - self.o.slowdown)) / self.o.recovery_ms;
        js_min(1.0, climbed)
    }

    pub fn spacing(&self, now: f64) -> f64 {
        let base = self.lanes.iter().fold(0.0, |m, l| js_max(m, l.interval));
        base / self.factor(now)
    }

    pub fn earliest(&mut self, now: f64) -> f64 {
        self.prune(now);
        let f = self.factor(now);
        let mut t = js_max(now, self.blocked_until);
        for i in 0..self.lanes.len() {
            let lane = self.lanes[i].clone();
            let interval = lane.interval / f;
            t = js_max(t, lane.tat - (lane.burst - 1.0) * interval);
            let room = lane.cap + lane.burst - 1.0;
            if self.in_window(lane.window_ms, now) as f64 >= room {
                let idx = self.starts.len() - room as usize;
                t = js_max(t, self.starts[idx] + lane.window_ms);
            }
        }
        t
    }

    pub fn commit(&mut self, at: f64) {
        let t = js_max(at, self.starts.back().copied().unwrap_or(at));
        let f = self.factor(t);
        for lane in &mut self.lanes {
            lane.tat = js_max(lane.tat, t) + lane.interval / f;
        }
        self.starts.push_back(t);
    }

    pub fn penalize(&mut self, now: f64, retry_after: f64, slow_down: bool) {
        self.blocked_until = js_max(self.blocked_until, now + retry_after);
        if slow_down {
            self.slow = (js_max(MIN_PACE_FACTOR, self.factor(now) * self.o.slowdown), self.blocked_until);
        }
        let b = self.blocked_until;
        self.resume_at(b);
    }

    pub fn resume_at(&mut self, t: f64) {
        let f = self.factor(t);
        for lane in &mut self.lanes {
            lane.tat = js_max(lane.tat, t + ((lane.burst - 1.0) * lane.interval) / f);
        }
    }

    pub fn retry_until(&self, now: f64) -> f64 {
        if self.blocked_until > now {
            self.blocked_until
        } else {
            0.0
        }
    }

    pub fn usage(&mut self, now: f64) -> Vec<Value> {
        self.prune(now);
        self.lanes.iter().map(|l| json!({ "used": self.in_window(l.window_ms, now), "limit": crate::js::num(l.limit), "windowSec": crate::js::num(l.window_ms / 1000.0) })).collect()
    }

    fn in_window(&self, window_ms: f64, now: f64) -> usize {
        let edge = now - window_ms;
        let (mut lo, mut hi) = (0usize, self.starts.len());
        while lo < hi {
            let mid = (lo + hi) >> 1;
            if self.starts[mid] > edge {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        self.starts.len() - lo
    }

    fn prune(&mut self, now: f64) {
        let horizon = if self.lanes.is_empty() { UNKNOWN_LIMITS_HORIZON_MS } else { self.lanes.iter().fold(f64::NEG_INFINITY, |m, l| js_max(m, l.window_ms)) };
        let expired = self.starts.len() - self.in_window(horizon, now);
        for _ in 0..expired {
            self.starts.pop_front();
        }
    }
}

pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;

pub fn wall_clock() -> Clock {
    Arc::new(|| crate::util::now_ms() as f64)
}

struct Waiter {
    id: u64,
    family: &'static str,
}

/// The limiter's state (all of it behind one lock).
pub struct LimiterState {
    pub app: Pacer,
    methods: HashMap<&'static str, Pacer>,
    queue: VecDeque<Waiter>,
    in_flight: usize,
    service_until: f64,
    service_strikes: f64,
    last_family: Option<&'static str>,
    last_start_at: f64,
    o: PacingOptions,
}

impl LimiterState {
    fn method(&mut self, family: &'static str) -> &mut Pacer {
        let o = self.o;
        self.methods.entry(family).or_insert_with(|| Pacer::new(&[], o))
    }
    fn blocked_until(&self, family: &str, now: f64) -> f64 {
        js_max(js_max(self.service_until, self.app.retry_until(now)), self.methods.get(family).map(|m| m.retry_until(now)).unwrap_or(0.0))
    }
    fn blocking_scope(&self, family: Option<&str>, until: f64, now: f64) -> &'static str {
        if until == self.service_until {
            return "service";
        }
        if until == self.app.retry_until(now) {
            return "application";
        }
        match family.and_then(|f| self.methods.get(f)) {
            Some(m) if m.retry_until(now) == until => "method",
            _ => "service",
        }
    }
    fn earliest(&mut self, family: &'static str, now: f64) -> f64 {
        if self.in_flight >= self.o.concurrency {
            return f64::INFINITY;
        }
        let a = self.app.earliest(now);
        let m = self.method(family).earliest(now);
        js_max(js_max(js_max(now, self.service_until), a), m)
    }
}

/// All Riot traffic to one host.
pub struct RateLimiter {
    pub host: String,
    pub state: Mutex<LimiterState>,
    notify: tokio::sync::Notify,
    next_id: AtomicU64,
    clock: Clock,
}

/// Held while a request is in flight: dropped (or `done()`) once its response has been read.
pub struct Permit {
    pub started_at: f64,
    limiter: Arc<RateLimiter>,
    released: bool,
}

impl Permit {
    pub fn done(mut self) {
        self.release();
    }
    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        self.limiter.state.lock().in_flight -= 1;
        self.limiter.notify.notify_waiters();
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Aborted(pub String);

impl std::fmt::Display for Aborted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Aborted {}

/// A stop signal shared by the lanes (AbortSignal): cancelled once, with its reason.
#[derive(Clone, Default)]
pub struct Signal(Arc<SignalInner>);

#[derive(Default)]
pub struct SignalInner {
    reason: Mutex<Option<String>>,
    notify: tokio::sync::Notify,
    parents: Mutex<Vec<Signal>>,
}

impl Signal {
    pub fn new() -> Signal {
        Signal::default()
    }
    /// A signal that also stops when any of `parents` does (AbortSignal.any).
    pub fn any(parents: &[&Signal]) -> Signal {
        let s = Signal::new();
        *s.0.parents.lock() = parents.iter().map(|p| (*p).clone()).collect();
        s
    }
    pub fn abort(&self, reason: &str) {
        let mut r = self.0.reason.lock();
        if r.is_none() {
            *r = Some(reason.to_string());
        }
        drop(r);
        self.0.notify.notify_waiters();
    }
    pub fn aborted(&self) -> bool {
        self.reason().is_some()
    }
    pub fn reason(&self) -> Option<String> {
        if let Some(r) = self.0.reason.lock().clone() {
            return Some(r);
        }
        self.0.parents.lock().iter().find_map(|p| p.reason())
    }
    pub fn check(&self) -> Result<(), Aborted> {
        match self.reason() {
            Some(r) => Err(Aborted(r)),
            None => Ok(()),
        }
    }
    /// Resolves once aborted (polling the parents every 50 ms).
    pub async fn cancelled(&self) {
        loop {
            if self.aborted() {
                return;
            }
            let n = self.0.notify.notified();
            tokio::pin!(n);
            n.as_mut().enable();
            if self.aborted() {
                return;
            }
            let parents = !self.0.parents.lock().is_empty();
            if parents {
                tokio::select! {
                    _ = &mut n => {}
                    _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
                }
            } else {
                n.await;
            }
        }
    }
}

/// Sleeps `ms`, or fails when `signal` stops first.
pub async fn sleep(ms: f64, signal: Option<&Signal>) -> Result<(), Aborted> {
    if let Some(s) = signal {
        s.check()?;
    }
    if ms <= 0.0 {
        return Ok(());
    }
    let d = std::time::Duration::from_micros((ms * 1000.0) as u64);
    match signal {
        Some(s) => tokio::select! {
            _ = tokio::time::sleep(d) => Ok(()),
            _ = s.cancelled() => Err(Aborted(s.reason().unwrap_or_default())),
        },
        None => {
            tokio::time::sleep(d).await;
            Ok(())
        }
    }
}

impl RateLimiter {
    pub fn new(host: &str, limits: &[RateWindow], o: PacingOptions, clock: Clock) -> Arc<RateLimiter> {
        Arc::new(RateLimiter {
            host: host.to_string(),
            state: Mutex::new(LimiterState {
                app: Pacer::new(limits, o),
                methods: HashMap::new(),
                queue: VecDeque::new(),
                in_flight: 0,
                service_until: 0.0,
                service_strikes: 0.0,
                last_family: None,
                last_start_at: 0.0,
                o,
            }),
            notify: tokio::sync::Notify::new(),
            next_id: AtomicU64::new(1),
            clock,
        })
    }

    fn now(&self) -> f64 {
        (self.clock)()
    }

    /// Waits for this request's turn and start slot.
    pub async fn acquire(self: &Arc<Self>, family: &'static str, signal: Option<&Signal>) -> Result<Permit, Aborted> {
        if let Some(s) = signal {
            s.check()?;
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.state.lock().queue.push_back(Waiter { id, family });
        struct Leave<'a>(&'a RateLimiter, u64, bool);
        impl Drop for Leave<'_> {
            fn drop(&mut self) {
                if self.2 {
                    return;
                }
                let mut st = self.0.state.lock();
                if let Some(i) = st.queue.iter().position(|w| w.id == self.1) {
                    st.queue.remove(i);
                }
                drop(st);
                self.0.notify.notify_waiters();
            }
        }
        let mut leave = Leave(self, id, false);
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let wait = {
                let mut st = self.state.lock();
                let now = self.now();
                if st.queue.front().map(|w| w.id) == Some(id) {
                    let at = st.earliest(family, now);
                    if at.is_finite() && at - now <= 0.0 {
                        st.queue.pop_front();
                        st.app.commit(now);
                        st.method(family).commit(now);
                        st.in_flight += 1;
                        st.last_family = Some(family);
                        st.last_start_at = now;
                        drop(st);
                        leave.2 = true;
                        self.notify.notify_waiters();
                        return Ok(Permit { started_at: now, limiter: self.clone(), released: false });
                    }
                    if at.is_finite() {
                        Some((at - now).ceil())
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            let sleep_for = async {
                match wait {
                    Some(ms) => tokio::time::sleep(std::time::Duration::from_millis(ms as u64)).await,
                    None => std::future::pending::<()>().await,
                }
            };
            match signal {
                Some(s) => {
                    tokio::select! {
                        _ = &mut notified => {}
                        _ = sleep_for => {}
                        _ = s.cancelled() => return Err(Aborted(s.reason().unwrap_or_default())),
                    }
                }
                None => {
                    tokio::select! {
                        _ = &mut notified => {}
                        _ = sleep_for => {}
                    }
                }
            }
        }
    }

    pub fn update_limits(&self, family: &'static str, app_header: Option<&str>, method_header: Option<&str>) {
        let changed = {
            let mut st = self.state.lock();
            let a = st.app.set_limits(&parse_limits(app_header));
            let m = st.method(family).set_limits(&parse_limits(method_header));
            a || m
        };
        if changed {
            self.notify.notify_waiters();
        }
    }

    /// Riot answered 429; returns how long the next request of this family waits.
    pub fn rate_limited(&self, family: &'static str, scope: &str, retry_after: Option<f64>) -> f64 {
        let now = self.now();
        let wait = {
            let mut st = self.state.lock();
            if scope == "service" {
                let wait = retry_after.unwrap_or_else(|| js_min(SERVICE_BACKOFF_MAX_MS, 1000.0 * 2f64.powf(st.service_strikes)));
                st.service_strikes += 1.0;
                st.service_until = js_max(st.service_until, now + wait);
                let until = st.service_until;
                st.app.resume_at(until);
                for m in st.methods.values_mut() {
                    m.resume_at(until);
                }
            } else if scope == "application" {
                st.app.penalize(now, retry_after.unwrap_or(DEFAULT_RETRY_AFTER_MS), true);
            } else {
                st.method(family).penalize(now, retry_after.unwrap_or(DEFAULT_RETRY_AFTER_MS), true);
            }
            js_max(0.0, st.blocked_until(family, now) - now)
        };
        self.notify.notify_waiters();
        wait
    }

    pub fn succeeded(&self) {
        self.state.lock().service_strikes = 0.0;
    }

    /// RateHostState as JSON.
    pub fn host_state(&self, now: f64) -> Value {
        let mut st = self.state.lock();
        let head = st.queue.front().map(|w| w.family);
        let next = head.map(|f| st.earliest(f, now));
        let family = head.or(st.last_family);
        let method_spacing = family.and_then(|f| st.methods.get(f)).map(|m| m.spacing(now)).unwrap_or(0.0);
        let until = match family {
            Some(f) => st.blocked_until(f, now),
            None => js_max(st.service_until, st.app.retry_until(now)),
        };
        let windows = st.app.usage(now);
        json!({
            "host": self.host,
            "windows": windows,
            "paceMs": crate::js::num(js_max(st.app.spacing(now), method_spacing)),
            "nextAt": match next { Some(n) if n.is_finite() => crate::js::num(n), _ => Value::Null },
            "queued": st.queue.len(),
            "inFlight": st.in_flight,
            "retryAfter": if until > now { json!({ "until": crate::js::num(until), "scope": st.blocking_scope(family, until, now) }) } else { Value::Null },
            "lastStartAt": crate::js::num(st.last_start_at),
        })
    }

    pub fn usage(&self) -> Vec<Value> {
        let now = self.now();
        self.state.lock().app.usage(now)
    }
}

#[cfg(test)]
mod tests {
    //! Port of server/riot/rateLimiter.test.ts (the limiter part), on tokio's paused clock.
    use super::*;
    use std::sync::atomic::AtomicUsize;

    const OPTS: PacingOptions = PacingOptions { safety: 0.9, burst: 3.0, concurrency: 3, slowdown: 0.8, recovery_ms: 60_000.0 };
    const SPACING: f64 = 120_000.0 / 90.0;
    const STEP: f64 = 5.0;

    fn dev_key() -> Vec<RateWindow> {
        parse_limits(Some("20:1,100:120"))
    }

    fn paused_clock() -> Clock {
        let start = tokio::time::Instant::now();
        Arc::new(move || (tokio::time::Instant::now() - start).as_secs_f64() * 1000.0)
    }

    async fn advance(ms: f64) {
        tokio::time::sleep(std::time::Duration::from_micros((ms * 1000.0) as u64)).await;
    }

    struct Run {
        starts: Vec<f64>,
        max_in_flight: usize,
    }

    async fn simulate(limiter: Arc<RateLimiter>, workers: usize, latency_ms: f64, duration_ms: f64, family: &'static str) -> Run {
        let starts = Arc::new(Mutex::new(vec![]));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in = Arc::new(AtomicUsize::new(0));
        let stop = Signal::new();
        let mut handles = vec![];
        for _ in 0..workers {
            let (l, s, inf, mx, st) = (limiter.clone(), starts.clone(), in_flight.clone(), max_in.clone(), stop.clone());
            handles.push(tokio::spawn(async move {
                loop {
                    let Ok(p) = l.acquire(family, Some(&st)).await else { return };
                    s.lock().push(p.started_at);
                    let n = inf.fetch_add(1, Ordering::SeqCst) + 1;
                    mx.fetch_max(n, Ordering::SeqCst);
                    let _ = sleep(latency_ms, None).await;
                    inf.fetch_sub(1, Ordering::SeqCst);
                    p.done();
                }
            }));
        }
        advance(duration_ms).await;
        stop.abort("done");
        advance(latency_ms + STEP).await;
        for h in handles {
            let _ = h.await;
        }
        let mut v = starts.lock().clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        Run { starts: v, max_in_flight: max_in.load(Ordering::SeqCst) }
    }

    fn busiest_window(starts: &[f64], window_ms: f64) -> usize {
        let (mut max, mut lo) = (0, 0);
        for i in 0..starts.len() {
            while starts[i] - starts[lo] >= window_ms {
                lo += 1;
            }
            max = max.max(i - lo + 1);
        }
        max
    }
    fn gaps(s: &[f64]) -> Vec<f64> {
        s.windows(2).map(|w| w[1] - w[0]).collect()
    }
    fn median(xs: &[f64]) -> f64 {
        let mut v = xs.to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    }
    async fn sequence(l: &Arc<RateLimiter>, n: usize, family: &'static str) -> Vec<f64> {
        let mut out = vec![];
        for _ in 0..n {
            let p = l.acquire(family, None).await.unwrap();
            out.push(p.started_at);
            p.done();
        }
        out
    }

    #[test]
    fn header_parsing() {
        assert_eq!(parse_limits(Some("20:1,100:120")), vec![RateWindow { limit: 20.0, window_ms: 1000.0 }, RateWindow { limit: 100.0, window_ms: 120_000.0 }]);
        assert_eq!(parse_limits(Some(" 500:10 , bogus, 0:5 ")), vec![RateWindow { limit: 500.0, window_ms: 10_000.0 }]);
        assert_eq!(parse_limits(None), vec![]);
        assert_eq!(retry_after_ms(Some("7")), Some(7000.0));
        assert_eq!(retry_after_ms(None), None);
        assert_eq!(limit_scope(Some("application")), "application");
        assert_eq!(limit_scope(Some("method")), "method");
        assert_eq!(limit_scope(Some("service")), "service");
        assert_eq!(limit_scope(None), "service");
    }

    #[tokio::test(start_paused = true)]
    async fn never_exceeds_any_window_and_never_stalls() {
        let limiter = RateLimiter::new("asia.api.riotgames.com", &dev_key(), OPTS, paused_clock());
        let run = simulate(limiter, 3, 150.0, 600_000.0, "match").await;
        for w in dev_key() {
            let cap = (w.limit * OPTS.safety).floor();
            let most = busiest_window(&run.starts, w.window_ms) as f64;
            assert!(most <= w.limit, "{most} in {}", w.window_ms);
            assert!(most <= cap + OPTS.burst - 1.0);
        }
        assert!(run.starts.len() >= 445 && run.starts.len() <= 455, "{} requests in 10 minutes", run.starts.len());
        let longest = gaps(&run.starts).into_iter().fold(0.0, f64::max);
        assert!(longest <= SPACING + STEP, "longest gap {longest}");
        assert!(run.max_in_flight <= OPTS.concurrency);
    }

    #[tokio::test(start_paused = true)]
    async fn spacing_converges_after_a_small_burst() {
        let limiter = RateLimiter::new("kr.api.riotgames.com", &dev_key(), OPTS, paused_clock());
        let run = simulate(limiter.clone(), 3, 80.0, 120_000.0, "match").await;
        assert_eq!(&run.starts[..3], &[0.0, 0.0, 0.0]);
        assert!(run.starts[3] >= SPACING - STEP);
        let steady = gaps(&run.starts[3..]);
        assert!((median(&steady) - SPACING).abs() <= STEP, "median gap {}", median(&steady));
        let mean = steady.iter().sum::<f64>() / steady.len() as f64;
        assert!((mean - SPACING).abs() < 1.5, "mean gap {mean}");
        let now = (limiter.clock)();
        assert!((limiter.host_state(now)["paceMs"].as_f64().unwrap() - SPACING).abs() < 0.01);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrency_overlaps_latency_but_never_raises_the_rate() {
        let single = simulate(RateLimiter::new("h", &dev_key(), PacingOptions { concurrency: 1, ..OPTS }, paused_clock()), 1, 400.0, 300_000.0, "match").await;
        let three = simulate(RateLimiter::new("h", &dev_key(), OPTS, paused_clock()), 6, 400.0, 300_000.0, "match").await;
        assert!((single.starts.len() as i64 - three.starts.len() as i64).abs() <= 3);
        assert_eq!(three.max_in_flight, 3);
        let fast = parse_limits(Some("500:10,30000:600"));
        let slow1 = simulate(RateLimiter::new("h", &fast, PacingOptions { concurrency: 1, ..OPTS }, paused_clock()), 1, 60.0, 10_000.0, "match").await;
        let slow3 = simulate(RateLimiter::new("h", &fast, OPTS, paused_clock()), 3, 60.0, 10_000.0, "match").await;
        assert!(slow3.starts.len() as f64 > 2.5 * slow1.starts.len() as f64, "{} -> {}", slow1.starts.len(), slow3.starts.len());
        assert!(busiest_window(&slow3.starts, 10_000.0) <= 452);
    }

    #[tokio::test(start_paused = true)]
    async fn application_429_waits_and_slows_then_recovers() {
        let clock = paused_clock();
        let limiter = RateLimiter::new("asia.api.riotgames.com", &dev_key(), OPTS, clock.clone());
        sequence(&limiter, 3, "match").await;
        let wait = limiter.rate_limited("match", "application", Some(10_000.0));
        assert_eq!(wait, 10_000.0);
        let st = limiter.host_state(clock());
        assert_eq!(st["retryAfter"]["scope"], "application");
        assert!((st["paceMs"].as_f64().unwrap() - SPACING / 0.8).abs() < 0.01);
        let t429 = clock();
        let s = sequence(&limiter, 2, "match").await;
        assert!(s[0] >= t429 + 10_000.0);
        assert!(s[1] - s[0] >= SPACING / 0.8 - STEP);
        advance(60_000.0).await;
        assert!((limiter.host_state(clock())["paceMs"].as_f64().unwrap() - SPACING).abs() < 0.01);
        assert!(limiter.host_state(clock())["retryAfter"].is_null());
    }

    #[tokio::test(start_paused = true)]
    async fn service_429_backs_off_only_that_host() {
        let clock = paused_clock();
        let kr = RateLimiter::new("kr.api.riotgames.com", &dev_key(), OPTS, clock.clone());
        let asia = RateLimiter::new("asia.api.riotgames.com", &dev_key(), OPTS, clock.clone());
        sequence(&kr, 3, "match").await;
        sequence(&asia, 3, "match").await;
        let t = clock();
        assert_eq!(kr.rate_limited("mastery", "service", None), 1000.0);
        assert_eq!(kr.host_state(t)["retryAfter"]["scope"], "service");
        assert!((kr.host_state(t)["paceMs"].as_f64().unwrap() - SPACING).abs() < 0.01);
        assert!((kr.state.lock().app.factor(t) - 1.0).abs() < 1e-9);
        assert!(asia.host_state(t)["retryAfter"].is_null());
        let k = sequence(&kr, 1, "mastery").await;
        assert!(k[0] >= t + 1000.0);
        assert_eq!(kr.rate_limited("mastery", "service", None), 2000.0);
        sequence(&kr, 1, "mastery").await;
        kr.succeeded();
        assert_eq!(kr.rate_limited("mastery", "service", None), 1000.0);
        assert_eq!(kr.rate_limited("mastery", "service", Some(7000.0)), 7000.0);
        asia.rate_limited("match", "application", Some(1000.0));
        assert!(asia.host_state(clock())["paceMs"].as_f64().unwrap() > SPACING * 1.2);
    }

    #[tokio::test(start_paused = true)]
    async fn method_limits_pace_their_family() {
        let clock = paused_clock();
        let limiter = RateLimiter::new("kr.api.riotgames.com", &dev_key(), OPTS, clock.clone());
        sequence(&limiter, 1, "league").await;
        limiter.update_limits("league", Some("20:1,100:120"), Some("5:10"));
        let league = sequence(&limiter, 12, "league").await;
        assert!(median(&gaps(&league)) >= 2500.0 - STEP);
        assert!(busiest_window(&league, 10_000.0) <= 5);
        assert!((limiter.host_state(clock())["paceMs"].as_f64().unwrap() - 2500.0).abs() < 0.01);
        advance(30_000.0).await;
        let mastery = sequence(&limiter, 8, "mastery").await;
        assert!((median(&gaps(&mastery[3..])) - SPACING).abs() <= STEP);
        let t = clock();
        limiter.rate_limited("league", "method", Some(20_000.0));
        let m = sequence(&limiter, 1, "mastery").await;
        assert!(m[0] < t + 20_000.0);
        let l = sequence(&limiter, 1, "league").await;
        assert!(l[0] >= t + 20_000.0);
    }

    #[tokio::test(start_paused = true)]
    async fn abort_mid_wait_frees_the_slot() {
        let clock = paused_clock();
        let limiter = RateLimiter::new("asia.api.riotgames.com", &dev_key(), OPTS, clock.clone());
        sequence(&limiter, 3, "match").await;
        let cancelled = Signal::new();
        let (l1, c1) = (limiter.clone(), cancelled.clone());
        let a = tokio::spawn(async move { l1.acquire("match", Some(&c1)).await.map(|p| p.started_at) });
        let l2 = limiter.clone();
        let b = tokio::spawn(async move { l2.acquire("match", None).await.map(|p| p.started_at) });
        advance(500.0).await;
        cancelled.abort("scan cancelled");
        assert_eq!(a.await.unwrap(), Err(Aborted("scan cancelled".into())));
        advance(1000.0).await;
        let started = b.await.unwrap().unwrap();
        assert!(started <= SPACING + STEP, "b started at {started}");
        limiter.rate_limited("match", "application", Some(30_000.0));
        let late = Signal::new();
        let (l3, c3) = (limiter.clone(), late.clone());
        let c = tokio::spawn(async move { l3.acquire("match", Some(&c3)).await.map(|p| p.started_at) });
        advance(2000.0).await;
        late.abort("stop");
        assert_eq!(c.await.unwrap(), Err(Aborted("stop".into())));
        assert_eq!(limiter.host_state(clock())["queued"], json!(0));
        let early = Signal::new();
        early.abort("early");
        assert!(limiter.acquire("match", Some(&early)).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn refreshed_limits_keep_the_schedule() {
        let clock = paused_clock();
        let limiter = RateLimiter::new("kr.api.riotgames.com", &dev_key(), OPTS, clock.clone());
        let before = sequence(&limiter, 5, "match").await;
        limiter.update_limits("match", Some("20:1,100:120"), None);
        let next = sequence(&limiter, 1, "match").await;
        assert!(next[0] - before[4] >= SPACING - STEP);
        limiter.update_limits("match", Some("10:1,50:120"), None);
        assert!((limiter.host_state(clock())["paceMs"].as_f64().unwrap() - 120_000.0 / 45.0).abs() < 0.01);
        let after = sequence(&limiter, 3, "match").await;
        assert!(gaps(&after).into_iter().fold(f64::INFINITY, f64::min) >= 120_000.0 / 45.0 - STEP);
    }
}
