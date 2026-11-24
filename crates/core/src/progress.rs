//! Port of `server/progress.ts`: how long the app's long actions take (timings.json next to the settings, this
//! computer only), and how long a running one has left.

use crate::app_config::app_config_dir;
use crate::js;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::PathBuf;

/// time ∝ size^exponent (1 unless listed).
fn growth(kind: &str) -> f64 {
    if kind == "evaluate" {
        1.3
    } else {
        1.0
    }
}

fn on_threads_kind(kind: &str) -> bool {
    kind == "evaluate" || kind == "evaluate-update"
}

fn default_ms(kind: &str, size: f64) -> Option<f64> {
    match kind {
        "evaluate" => Some(10.0 * 60_000.0 * crate::jsmath::pow(js_max(1.0, size) / 54_000.0, 1.3)),
        "compact" => Some(size / (58.0 * 1024.0 * 1024.0) * 1000.0),
        _ => None,
    }
}

fn js_max(a: f64, b: f64) -> f64 {
    crate::model::logistic::js_max(a, b)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Timing {
    pub size: f64,
    pub ms: f64,
    pub at: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threads: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_per_game: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heap_per_game: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<f64>,
}

pub const EVAL_SERIAL_SHARE: f64 = 0.16;
fn on_threads(threads: f64) -> f64 {
    EVAL_SERIAL_SHARE + (1.0 - EVAL_SERIAL_SHARE) / js_max(1.0, threads)
}
const KEEP: usize = 5;

fn timings_file() -> PathBuf {
    app_config_dir().join("timings.json")
}

fn read_timings() -> Map<String, Value> {
    std::fs::read_to_string(timings_file()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

fn timings_of(all: &Map<String, Value>, kind: &str) -> Vec<Timing> {
    all.get(kind).and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default()
}

/// Keeps a finished action's time (only complete runs).
pub fn record_timing(kind: &str, t: Timing) {
    if !(t.ms > 0.0) || !(t.size > 0.0) {
        return;
    }
    let mut all = read_timings();
    let mut list = vec![serde_json::to_value(&t).unwrap()];
    if let Some(Value::Array(old)) = all.get(kind) {
        list.extend(old.iter().cloned());
    }
    list.truncate(KEEP);
    all.insert(kind.into(), Value::Array(list));
    let _ = std::fs::create_dir_all(app_config_dir());
    let _ = crate::util::write_atomic(&timings_file(), format!("{}\n", js::stringify(&Value::Object(all))).as_bytes());
}

pub fn last_timing(kind: &str) -> Option<Timing> {
    timings_of(&read_timings(), kind).into_iter().next()
}

pub fn thread_measure(layout: f64) -> Option<f64> {
    timings_of(&read_timings(), "evaluate").into_iter().find(|t| t.bytes_per_game.map(|b| b != 0.0).unwrap_or(false) && t.layout.unwrap_or(1.0) == layout).and_then(|t| t.bytes_per_game)
}

pub fn heap_measure(layout: f64) -> Option<f64> {
    timings_of(&read_timings(), "evaluate").into_iter().find(|t| t.bytes_per_game.map(|b| b != 0.0).unwrap_or(false) && t.layout.unwrap_or(1.0) == layout).and_then(|t| t.heap_per_game)
}

/// How long an action of this size should take here; None: nothing to go on.
pub fn expected_ms(kind: &str, size: f64, threads: Option<f64>) -> Option<f64> {
    let last = last_timing(kind);
    let scale = |ms: f64| match threads {
        Some(t) if on_threads_kind(kind) => ms / on_threads(last.as_ref().and_then(|l| l.threads).unwrap_or(1.0)) * on_threads(t),
        _ => ms,
    };
    if let Some(l) = &last {
        return Some(scale(l.ms * crate::jsmath::pow(js_max(1.0, size) / l.size, growth(kind))));
    }
    default_ms(kind, size).map(scale)
}

const PACE_AFTER_FRACTION: f64 = 0.01;
const PACE_AFTER_MS: f64 = 3000.0;

/// A running action's progress.
#[derive(Clone, Debug)]
pub struct Progress {
    pub started_at: i64,
    expected: Option<f64>,
}

impl Progress {
    pub fn new(expected: Option<f64>) -> Progress {
        Progress { started_at: crate::util::now_ms(), expected }
    }
    pub fn starting_at(expected: Option<f64>, started_at: i64) -> Progress {
        Progress { started_at, expected }
    }
    pub fn expect(&mut self, ms: Option<f64>) {
        if ms.is_some() {
            self.expected = ms;
        }
    }
    /// ProgressInfo as JSON.
    pub fn info(&self, fraction: Option<f64>) -> Value {
        self.info_at(fraction, crate::util::now_ms())
    }
    pub fn info_at(&self, fraction: Option<f64>, now: i64) -> Value {
        let f = fraction.map(|f| crate::model::logistic::js_min(1.0, js_max(0.0, f)));
        let elapsed = (now - self.started_at) as f64;
        let left = self.expected.map(|e| js_max(0.0, e - elapsed));
        let mut eta = left;
        if let Some(f) = f {
            if f >= PACE_AFTER_FRACTION && elapsed >= PACE_AFTER_MS {
                let pace = elapsed * (1.0 - f) / f;
                let w = if left.is_none() { 1.0 } else { crate::model::logistic::js_min(1.0, f * 2.0) };
                eta = Some(w * pace + (1.0 - w) * left.unwrap_or(0.0));
            }
        }
        json!({ "fraction": f.map(js::num), "etaMs": eta.map(|e| js::num(js::round(e))), "startedAt": self.started_at, "at": now })
    }
}

/// How far a run of steps is, each step weighed by how long it took last time (else equally).
pub fn step_fraction(step: usize, steps: usize, part: f64, weights: Option<&[f64]>) -> f64 {
    if steps == 0 {
        return 0.0;
    }
    let w = weights.filter(|w| w.len() == steps && w.iter().all(|x| *x >= 0.0) && w.iter().any(|x| *x > 0.0));
    let total = w.map(|w| w.iter().fold(0.0, |s, x| s + x)).unwrap_or(steps as f64);
    let mut done = 0.0;
    for i in 0..(step.saturating_sub(1)).min(steps) {
        done += w.map(|w| w[i]).unwrap_or(1.0);
    }
    let cur = if step >= 1 { w.map(|w| w.get(step - 1).copied().unwrap_or(0.0)).unwrap_or(1.0) } else { w.map(|_| 0.0).unwrap_or(1.0) };
    done += cur * crate::model::logistic::js_min(1.0, js_max(0.0, part));
    done / total
}
