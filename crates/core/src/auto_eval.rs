//! Port of `server/autoEval.ts`: Evaluate builds by itself (Data tab, Follow the evaluation on). A quick update after
//! enough new games, a full evaluation on a new patch or as many games since the last full one; only after a quiet spell
//! without champ select or a game; champ select or a game pauses it.

use crate::ddragon::compare_patch;
use crate::model::eval_base::EvalPlan;
use crate::model::service::{EvalSummary, Mark};
use crate::patches::VersionRow;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct AutoEvalConfig {
    pub new_games: f64,
    pub new_share: f64,
    pub spacing_ms: f64,
    pub update_spacing_ms: f64,
    pub quiet_ms: f64,
    pub keep_reports: usize,
}
pub const AUTO_EVAL: AutoEvalConfig =
    AutoEvalConfig { new_games: 300.0, new_share: 0.03, spacing_ms: 6.0 * 3_600_000.0, update_spacing_ms: 3_600_000.0, quiet_ms: 10.0 * 60_000.0, keep_reports: 20 };

pub fn newest_patch(versions: &[VersionRow], before: f64) -> Option<String> {
    crate::model::run_eval::newest_patch(versions, before)
}

/// The last evaluation's mark (one saved before 0.8.4 has none: its count is in its dataVersion).
pub fn mark_of(summary: Option<&EvalSummary>, versions: &[VersionRow]) -> Option<Mark> {
    let s = summary?;
    if let Some(m) = &s.mark {
        return Some(m.clone());
    }
    let games = crate::js::to_number(&Value::String(s.data_version.split(':').next().unwrap_or("").into()));
    Some(Mark { row: None, games: if games.is_finite() { games } else { 0.0 }, patch: newest_patch(versions, s.evaluated_at) })
}

/// Games stored since `mark`: by row, or by count when that says more. Never evaluated: every stored game.
pub fn new_games_since(game_mark: (i64, i64), games_after_row: &dyn Fn(i64) -> i64, mark: Option<&Mark>) -> f64 {
    let Some(m) = mark else { return game_mark.1 as f64 };
    let after = m.row.map(|r| games_after_row(r as i64) as f64).unwrap_or(0.0);
    (game_mark.1 as f64 - m.games).max(0.0).max(after)
}

pub trait AutoEvalDeps: Send + Sync {
    fn following(&self) -> bool;
    fn quiet(&self) -> bool;
    fn busy(&self) -> bool;
    fn summary(&self) -> Option<EvalSummary>;
    fn game_mark(&self) -> (i64, i64);
    fn games_after_row(&self, row: i64) -> i64;
    fn game_versions(&self) -> Vec<VersionRow>;
    fn plan(&self) -> anyhow::Result<EvalPlan>;
    /// The evaluation running now: Some(auto reason when the app started it and nobody clicked since).
    fn running(&self) -> Option<Option<String>>;
    /// Starts an automatic evaluation; `done` is called when it ends (Err: stopped or failed).
    fn start(&self, why: String, path: &'static str, done: Box<dyn FnOnce(Result<(), String>) + Send>);
    fn pause(&self);
    fn changed(&self, _status: &Value) {}
    fn now(&self) -> f64 {
        crate::util::now_ms() as f64
    }
    fn log(&self, _line: &str) {}
}

#[derive(Clone, Debug, PartialEq)]
pub struct AutoStatus {
    pub on: bool,
    pub path: Option<String>,
    pub reason: Option<String>,
    pub new_games: f64,
    pub since_full: f64,
    pub needed: f64,
    pub update_games: f64,
    pub new_patch: Option<String>,
    pub waiting: Option<&'static str>,
    pub not_before: Option<f64>,
}

impl AutoStatus {
    pub fn to_json(&self) -> Value {
        json!({
            "on": self.on, "path": self.path, "reason": self.reason, "newGames": crate::js::num(self.new_games), "sinceFull": crate::js::num(self.since_full),
            "needed": crate::js::num(self.needed), "updateGames": crate::js::num(self.update_games), "newPatch": self.new_patch,
            "spacingHours": crate::js::num(AUTO_EVAL.spacing_ms / 3_600_000.0), "updateSpacingHours": crate::js::num(AUTO_EVAL.update_spacing_ms / 3_600_000.0),
            "quietMinutes": crate::js::num(AUTO_EVAL.quiet_ms / 60_000.0), "waiting": self.waiting, "notBefore": self.not_before.map(crate::js::num),
        })
    }
}

fn off() -> AutoStatus {
    AutoStatus {
        on: false,
        path: None,
        reason: None,
        new_games: 0.0,
        since_full: 0.0,
        needed: AUTO_EVAL.new_games,
        update_games: AUTO_EVAL.new_games,
        new_patch: None,
        waiting: None,
        not_before: None,
    }
}

struct Inner {
    quiet_since: Option<f64>,
    attempted_at: f64,
    full_attempted_at: f64,
    pausing: bool,
    paused: bool,
    current: AutoStatus,
    planned: Option<EvalPlan>,
    plan_error: String,
    sent: String,
}

pub struct AutoEvaluation {
    deps: Arc<dyn AutoEvalDeps>,
    inner: Mutex<Inner>,
}

impl AutoEvaluation {
    pub fn new(deps: Arc<dyn AutoEvalDeps>) -> Arc<AutoEvaluation> {
        let now = deps.now();
        Arc::new(AutoEvaluation {
            deps,
            inner: Mutex::new(Inner {
                quiet_since: Some(now),
                attempted_at: 0.0,
                full_attempted_at: 0.0,
                pausing: false,
                paused: false,
                current: off(),
                planned: None,
                plan_error: String::new(),
                sent: String::new(),
            }),
        })
    }

    pub fn status(&self) -> AutoStatus {
        self.inner.lock().current.clone()
    }
    pub fn plan(&self) -> Option<EvalPlan> {
        self.inner.lock().planned.clone()
    }

    fn due(s: &AutoStatus) -> bool {
        if s.path.as_deref() == Some("none") {
            return false;
        }
        s.new_patch.is_some() || (if s.path.as_deref() == Some("update") { s.new_games } else { s.since_full }) >= s.needed
    }

    /// Called every minute, after each evaluation, and whenever champ select or a game begins or ends.
    pub fn check(self: &Arc<Self>) {
        let now = self.deps.now();
        let quiet = self.deps.quiet();
        {
            let mut i = self.inner.lock();
            if !quiet {
                i.quiet_since = None;
            } else if i.quiet_since.is_none() {
                i.quiet_since = Some(now);
            }
        }
        let running = self.deps.running();
        let auto_running = matches!(&running, Some(Some(_)));
        let pause = {
            let mut i = self.inner.lock();
            if auto_running && !quiet && !i.pausing {
                i.pausing = true;
                i.paused = true;
                true
            } else {
                false
            }
        };
        if pause {
            self.deps.log("champ select or a game began: the automatic evaluation stops and runs again later");
            self.deps.pause();
            self.inner.lock().pausing = false;
        }
        if self.deps.busy() {
            let mut i = self.inner.lock();
            if i.current.on && running.is_none() && Self::due(&i.current) {
                i.current.waiting = Some("busy");
                i.current.not_before = None;
            }
        } else {
            if running.is_none() {
                self.read_plan();
            }
            let current = self.compute(now, running.is_some());
            self.inner.lock().current = current.clone();
            if current.on && running.is_none() && Self::due(&current) && current.waiting.is_none() {
                self.begin(now);
            }
        }
        let (json, changed) = {
            let mut i = self.inner.lock();
            let j = json!([i.current.to_json(), i.planned.as_ref().map(|p| serde_json::to_value(p).unwrap())]).to_string();
            let changed = j != i.sent;
            if changed {
                i.sent = j;
            }
            (i.current.to_json(), changed)
        };
        if changed {
            self.deps.changed(&json);
        }
    }

    fn read_plan(&self) {
        match self.deps.plan() {
            Ok(p) => {
                let mut i = self.inner.lock();
                i.planned = Some(p);
                i.plan_error.clear();
            }
            Err(e) => {
                let m = e.to_string();
                let log = {
                    let mut i = self.inner.lock();
                    let l = m != i.plan_error;
                    i.plan_error = m.clone();
                    l
                };
                if log {
                    self.deps.log(&format!("what Evaluate builds would run could not be worked out: {m}"));
                }
            }
        }
    }

    fn compute(&self, now: f64, running: bool) -> AutoStatus {
        if !self.deps.following() {
            return off();
        }
        let summary = self.deps.summary();
        let versions = self.deps.game_versions();
        let mark = mark_of(summary.as_ref(), &versions);
        let newest = newest_patch(&versions, f64::INFINITY);
        let full = if summary.as_ref().and_then(|s| s.path.as_deref()) == Some("update") { summary.as_ref().and_then(|s| s.full.clone()) } else { None };
        let full_at = full.as_ref().map(|f| f.evaluated_at).or(summary.as_ref().map(|s| s.evaluated_at)).unwrap_or(0.0);
        let gm = self.deps.game_mark();
        let after = |row: i64| self.deps.games_after_row(row);
        let new_games = new_games_since(gm, &after, mark.as_ref());
        let planned = self.inner.lock().planned.clone();
        let update = planned.as_ref().map(|p| p.path == "update").unwrap_or(false);
        let games_of = |m: Option<&Mark>| m.map(|m| m.games).unwrap_or(0.0);
        let needed_mark = if update { mark.as_ref() } else { full.as_ref().map(|f| &f.mark).or(mark.as_ref()) };
        let mut s = AutoStatus {
            on: true,
            path: planned.as_ref().map(|p| p.path.clone()),
            reason: planned.as_ref().map(|p| p.reason.clone()),
            new_games,
            since_full: match &full {
                Some(f) => new_games_since(gm, &after, Some(&f.mark)),
                None => new_games,
            },
            update_games: AUTO_EVAL.new_games.max((AUTO_EVAL.new_share * games_of(mark.as_ref())).ceil()),
            needed: AUTO_EVAL.new_games.max((AUTO_EVAL.new_share * games_of(needed_mark)).ceil()),
            new_patch: match (&newest, mark.as_ref().and_then(|m| m.patch.as_ref())) {
                (Some(n), Some(m)) if compare_patch(n, m) > 0.0 => Some(n.clone()),
                _ => None,
            },
            waiting: None,
            not_before: None,
        };
        if running || !Self::due(&s) {
            return s;
        }
        let (attempted, full_attempted, quiet_since) = {
            let i = self.inner.lock();
            (i.attempted_at, i.full_attempted_at, i.quiet_since)
        };
        let not_before = if update {
            summary.as_ref().map(|x| x.evaluated_at).unwrap_or(0.0).max(attempted) + AUTO_EVAL.update_spacing_ms
        } else {
            full_at.max(full_attempted) + AUTO_EVAL.spacing_ms
        };
        if now < not_before {
            s.waiting = Some("spacing");
            s.not_before = Some(not_before);
            return s;
        }
        match quiet_since {
            Some(q) if now - q >= AUTO_EVAL.quiet_ms => s,
            q => {
                s.waiting = Some("quiet");
                s.not_before = q.map(|q| q + AUTO_EVAL.quiet_ms);
                s
            }
        }
    }

    fn begin(self: &Arc<Self>, now: f64) {
        let s = self.inner.lock().current.clone();
        let path: &'static str = if s.path.as_deref() == Some("update") { "update" } else { "full" };
        let count = crate::js::thousands;
        let why = if self.deps.summary().is_none() {
            "first evaluation".to_string()
        } else if let Some(p) = &s.new_patch {
            format!("new patch {p}")
        } else if path == "update" || s.since_full == s.new_games {
            format!("{} new games", count(if path == "update" { s.new_games } else { s.since_full }))
        } else {
            format!("{} games since the last full evaluation", count(s.since_full))
        };
        let before = {
            let mut i = self.inner.lock();
            let b = (i.attempted_at, i.full_attempted_at);
            i.attempted_at = now;
            if path == "full" {
                i.full_attempted_at = now;
            }
            i.paused = false;
            b
        };
        self.deps.log(&format!("starting {} ({why})", if path == "update" { "a quick update" } else { "a full evaluation" }));
        let me = self.clone();
        self.deps.start(why, path, Box::new(move |r| {
            match r {
                Ok(()) => me.deps.log("finished"),
                Err(e) => {
                    let paused = me.inner.lock().paused;
                    if paused {
                        let mut i = me.inner.lock();
                        i.attempted_at = before.0;
                        i.full_attempted_at = before.1;
                    } else {
                        me.deps.log(&format!("stopped: {e}"));
                    }
                }
            }
            me.inner.lock().paused = false;
            me.check();
        }));
    }
}
