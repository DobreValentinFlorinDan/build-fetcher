//! Port of `server/jobs.ts` + `server/jobWorker.ts`: one-shot jobs (Evaluate builds, the data audit), each on a thread
//! of its own. The same job asked for again while it runs joins it; `cancel` stops one.

use irelia::config::config;
use irelia::db::{Db, OpenOptions};
use irelia::ddragon::StaticData;
use irelia::model::evaluate::{EvalControl, EvalStopped};
use irelia::model::run_eval::{evaluate_data_folder, FolderOptions};
use irelia::progress::{expected_ms, last_timing, record_timing, step_fraction, Progress, Timing};
use irelia::util::now_ms;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub const CANCELLED: &str = "Cancelled";

#[derive(Clone, Debug)]
pub enum JobKind {
    Audit,
    Evaluate { quick: bool, auto: Option<String>, path: &'static str, planned: &'static str },
}

impl JobKind {
    fn name(&self) -> &'static str {
        match self {
            JobKind::Audit => "audit",
            JobKind::Evaluate { .. } => "evaluate",
        }
    }
}

type Waiter = tokio::sync::oneshot::Sender<Result<Value, String>>;

struct Running {
    info: Value,
    control: Arc<EvalControl>,
    stop_reason: Arc<Mutex<Option<(String, bool)>>>,
    waiters: Vec<Waiter>,
}

pub struct Jobs {
    running: Mutex<indexmap::IndexMap<&'static str, Running>>,
    last: Mutex<serde_json::Map<String, Value>>,
    busy: std::sync::atomic::AtomicBool,
    data: Mutex<Arc<StaticData>>,
    emit: Arc<dyn Fn() + Send + Sync>,
    threads: Arc<dyn Fn(f64) -> Option<f64> + Send + Sync>,
    audit: Arc<dyn Fn(&dyn Fn(Value)) -> anyhow::Result<Value> + Send + Sync>,
}

fn panic_text(p: &Box<dyn std::any::Any + Send>) -> String {
    crate::model_worker::panic_text(p)
}

impl Jobs {
    pub fn new(
        data: Arc<StaticData>,
        emit: Arc<dyn Fn() + Send + Sync>,
        threads: Arc<dyn Fn(f64) -> Option<f64> + Send + Sync>,
        audit: Arc<dyn Fn(&dyn Fn(Value)) -> anyhow::Result<Value> + Send + Sync>,
    ) -> Arc<Jobs> {
        Arc::new(Jobs {
            running: Mutex::new(indexmap::IndexMap::new()),
            last: Mutex::new(serde_json::Map::new()),
            busy: Default::default(),
            data: Mutex::new(data),
            emit,
            threads,
            audit,
        })
    }

    pub fn set_data(&self, data: Arc<StaticData>) {
        *self.data.lock() = data;
    }

    /// Champ select or a game began or ended: a running evaluation finishes on one thread.
    pub fn set_busy(&self, on: bool) {
        self.busy.store(on, Ordering::SeqCst);
        for r in self.running.lock().values() {
            r.control.busy.store(on, Ordering::SeqCst);
        }
    }
    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    /// JobsStatus without auto / plan.
    pub fn status(&self) -> Value {
        json!({
            "running": self.running.lock().values().map(|r| r.info.clone()).collect::<Vec<_>>(),
            "last": Value::Object(self.last.lock().clone()),
        })
    }

    /// The running job of that name: its info (its `auto` says whether the app started it).
    pub fn running_job(&self, name: &str) -> Option<Value> {
        self.running.lock().get(name).map(|r| r.info.clone())
    }

    /// A full evaluation asked for while a quick update runs doesn't join it: the quick update stops (Cancelled).
    fn stop_update_for_full(&self, name: &str, job: &JobKind) -> bool {
        let running = self.running.lock();
        let Some(existing) = running.get(name) else { return false };
        if let JobKind::Evaluate { path: "full", .. } = job {
            if existing.info["path"] == "update" {
                *existing.stop_reason.lock() = Some((CANCELLED.into(), false));
                existing.control.stop.store(true, Ordering::SeqCst);
                return true;
            }
        }
        false
    }

    /// Joins a running job of the same name (a click makes an automatic one yours): true, or hands the sender back.
    fn join_running(&self, name: &'static str, job: &JobKind, tx: Waiter) -> (bool, Option<Waiter>) {
        let auto = matches!(job, JobKind::Evaluate { auto: Some(_), .. });
        let mut emit = false;
        let joined = {
            let mut running = self.running.lock();
            match running.get_mut(name) {
                Some(existing) => {
                    if !auto && existing.info.get("auto").is_some() {
                        existing.info.as_object_mut().unwrap().remove("auto");
                        emit = true;
                    }
                    existing.waiters.push(tx);
                    None
                }
                None => Some(tx),
            }
        };
        if emit {
            (self.emit)();
        }
        match joined {
            None => (true, None),
            Some(tx) => (false, Some(tx)),
        }
    }

    /// Runs (or joins) a job; resolves with its result (the report text, file, gate…) or its error.
    pub async fn run(self: &Arc<Self>, job: JobKind, size: Option<f64>) -> Result<Value, String> {
        let name = job.name();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let restart = self.stop_update_for_full(name, &job);
        if restart {
            // The quick update stops as Cancel stops it; the full one starts once it has.
            while self.running.lock().contains_key(name) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
        let (joined, tx) = self.join_running(name, &job, tx);
        if joined {
            return rx.await.unwrap_or_else(|_| Err("The job stopped".into()));
        }
        let tx = tx.expect("a new job keeps its sender");
        let started_at = now_ms();
        let quick = matches!(&job, JobKind::Evaluate { quick: true, .. });
        let planned = match &job {
            JobKind::Evaluate { planned, .. } => Some(*planned),
            JobKind::Audit => None,
        };
        let kind: &'static str = if planned == Some("update") { "evaluate-update" } else { name };
        let timed = !quick && size.map(|s| s > 0.0).unwrap_or(false);
        let threads = if name == "evaluate" { size.and_then(|s| (self.threads)(s)) } else { None };
        let tracker = Arc::new(Mutex::new(Progress::starting_at(if timed { expected_ms(kind, size.unwrap(), threads) } else { None }, started_at)));
        let weights = last_timing(kind).and_then(|t| t.steps);
        let mut info = json!({ "job": name, "startedAt": started_at, "step": null, "steps": null, "label": null });
        if let JobKind::Evaluate { auto: Some(a), .. } = &job {
            info["auto"] = json!(a);
        }
        if let Some(p) = planned {
            info["path"] = json!(p);
        }
        info["progress"] = tracker.lock().info_at(Some(0.0), started_at);
        let control = Arc::new(EvalControl::default());
        control.busy.store(self.busy(), Ordering::SeqCst);
        let stop_reason = Arc::new(Mutex::new(None));
        self.running.lock().insert(name, Running { info, control: control.clone(), stop_reason: stop_reason.clone(), waiters: vec![tx] });
        (self.emit)();

        let me = self.clone();
        let data = self.data.lock().clone();
        let step_starts: Arc<Mutex<Vec<Option<i64>>>> = Arc::new(Mutex::new(vec![]));
        let ss = step_starts.clone();
        let tr = tracker.clone();
        let w2 = weights.clone();
        let me2 = self.clone();
        let progress = move |step: usize, steps: usize, label: &str, part: Option<f64>| {
            {
                let mut s = ss.lock();
                if step >= 1 {
                    if s.len() < step {
                        s.resize(step, None);
                    }
                    if s[step - 1].is_none() {
                        s[step - 1] = Some(now_ms());
                    }
                }
            }
            let fraction = step_fraction(step, steps, part.unwrap_or(0.0), w2.as_deref());
            let p = tr.lock().info(Some(fraction));
            if let Some(r) = me2.running.lock().get_mut(name) {
                r.info["step"] = json!(step);
                r.info["steps"] = json!(steps);
                r.info["label"] = json!(label);
                r.info["progress"] = p;
            }
            (me2.emit)();
        };
        let job2 = job.clone();
        let handle = std::thread::Builder::new()
            .name(format!("job-{name}"))
            .spawn(move || {
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| -> anyhow::Result<Value> {
                    match &job2 {
                        JobKind::Audit => (me.audit)(&|p: Value| {
                            progress(p["step"].as_u64().unwrap_or(0) as usize, p["steps"].as_u64().unwrap_or(0) as usize, p["label"].as_str().unwrap_or(""), p["part"].as_f64())
                        }),
                        JobKind::Evaluate { quick, auto, path, .. } => {
                            let db = Db::open(&config().db_path, OpenOptions { migrate: false, ..Default::default() })?;
                            let threads = crate::system::planned_threads(irelia::model::run_eval::eval_size(&db, &data)) as usize;
                            if let Some(r) = me.running.lock().get_mut(name) {
                                r.info["threads"] = json!(threads);
                            }
                            let r = evaluate_data_folder(&db, data, FolderOptions {
                                quick: *quick,
                                auto: auto.is_some(),
                                path,
                                threads,
                                log: Box::new(|l: &str| {
                                    if l.starts_with("threads:") || l.starts_with("time:") || l.starts_with("quick update not kept") {
                                        eprintln!("[eval] {l}");
                                    }
                                }),
                                progress: Box::new(|p| progress(p.step, p.steps, &p.label, p.part)),
                                save: true,
                                control: Some(control.clone()),
                                grouping: None,
                                gpu: irelia::app_config::get_bool("useGpu") == Some(true),
                            })?;
                            Ok(json!({
                                "text": r.text, "file": r.file, "gate": r.report.gate, "threads": r.threads, "path": r.path, "reason": r.reason,
                            }))
                        }
                    }
                }));
                let result: Result<Value, String> = match result {
                    Ok(Ok(v)) => Ok(v),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(p) if p.downcast_ref::<EvalStopped>().is_some() => Err(CANCELLED.into()),
                    Err(p) => Err(panic_text(&p)),
                };
                me.finish(name, kind, started_at, size, timed, planned, result, &step_starts);
            });
        if let Err(e) = handle {
            let r = self.running.lock().shift_remove(name);
            self.last.lock().insert(name.into(), json!({ "startedAt": started_at, "finishedAt": now_ms(), "ok": false, "error": e.to_string() }));
            (self.emit)();
            drop(r);
            return Err(e.to_string());
        }
        rx.await.unwrap_or_else(|_| Err("The job stopped".into()))
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(&self, name: &'static str, kind: &'static str, started_at: i64, size: Option<f64>, timed: bool, planned: Option<&'static str>, result: Result<Value, String>, step_starts: &Mutex<Vec<Option<i64>>>) {
        let finished_at = now_ms();
        let Some(r) = self.running.lock().shift_remove(name) else { return };
        let auto = r.info.get("auto").cloned();
        let stop = r.stop_reason.lock().clone();
        let result = match (result, &stop) {
            (Err(e), Some((reason, _))) if e == CANCELLED => Err(reason.clone()),
            (r, _) => r,
        };
        let mut last = match &result {
            Ok(v) => {
                let mut m = json!({ "startedAt": started_at, "finishedAt": finished_at, "ok": true });
                for (k, val) in v.as_object().unwrap() {
                    m[k] = val.clone();
                }
                let ran = v["path"].as_str().unwrap_or("full");
                if timed && (planned.is_none() || Some(ran) == planned) {
                    let steps_n = r.info["steps"].as_u64().map(|n| n as usize);
                    let starts = step_starts.lock().clone();
                    let steps = match steps_n {
                        Some(n) if starts.len() == n && starts.iter().all(|x| x.is_some()) => {
                            Some((0..n).map(|i| (starts.get(i + 1).and_then(|x| *x).unwrap_or(finished_at) - starts[i].unwrap()) as f64).collect())
                        }
                        _ => None,
                    };
                    record_timing(kind, Timing {
                        size: size.unwrap_or(0.0),
                        ms: (finished_at - started_at) as f64,
                        at: finished_at as f64,
                        steps,
                        threads: v["threads"].as_f64().filter(|t| *t > 0.0),
                        ..Default::default()
                    });
                }
                m
            }
            Err(e) if e == CANCELLED => {
                let mut m = json!({ "startedAt": started_at, "finishedAt": finished_at, "ok": false, "cancelled": true });
                if stop.as_ref().map(|s| s.1).unwrap_or(false) {
                    m["paused"] = json!(true);
                }
                m
            }
            Err(e) => json!({ "startedAt": started_at, "finishedAt": finished_at, "ok": false, "error": e }),
        };
        if let Some(a) = auto {
            last["auto"] = a;
        }
        self.last.lock().insert(name.into(), last);
        (self.emit)();
        for w in r.waiters {
            let _ = w.send(result.clone());
        }
    }

    /// Stops a running job; its caller gets "Cancelled". `paused`: an automatic evaluation champ select or a game stopped.
    pub async fn cancel(&self, name: &str, paused: bool) {
        self.stop(name, CANCELLED, paused);
        while self.running.lock().contains_key(name) {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    fn stop(&self, name: &str, reason: &str, paused: bool) {
        if let Some(r) = self.running.lock().get(name) {
            *r.stop_reason.lock() = Some((reason.into(), paused));
            r.control.stop.store(true, Ordering::SeqCst);
        }
    }

    /// Stops every job (the data folder moves). The audit stops at its next step.
    pub async fn cancel_all(&self, what: &str) {
        let names: Vec<&'static str> = self.running.lock().keys().copied().collect();
        for n in &names {
            self.stop(n, &format!("Stopped while {what}"), false);
        }
        while !self.running.lock().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// The audit's stop flag (it checks it between steps).
    pub fn audit_stopped(&self) -> bool {
        self.running.lock().get("audit").map(|r| r.control.stop.load(Ordering::SeqCst)).unwrap_or(false)
    }
}
