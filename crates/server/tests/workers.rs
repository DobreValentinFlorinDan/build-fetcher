//! Port of server/workers.test.ts: the worker threads on a real data folder. The model thread answers before and after
//! its first build and starts again by itself after a crash, so a request never talks to a thread that is gone; the
//! scraper's Stop is answered at once, a data-folder pause keeps it stopped until it is resumed, and Pause fetching
//! during games works. The job thread reports its progress and can be cancelled. Offline: Data Dragon is the tests'
//! snapshot.
//!
//! Not ported: 0.22.0's "job as a process" tests (IRELIA_EVAL_PROCESSES). 1.0 runs evaluations on threads in the one
//! process, so there is no process to start or to fall back from. The scraper has no worker thread of its own to kill
//! (its lanes are tasks of the app's runtime); its restart after a data-folder move is tested instead.

mod common;

use irelia::testkit::load_test_static_data;
use irelia::testkit_model::{ap_ad_games, store_games, ApAd, AP_TEAM};
use irelia_server::jobs::{JobKind, Jobs, CANCELLED};
use irelia_server::model_worker::Request;
use parking_lot::Mutex;
use serde_json::Value;
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

async fn wait_for(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !ok() {
        assert!(Instant::now() < end, "no {what} in {secs} s");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn until_status(s: &irelia_server::scraper::ScraperController, ok: impl Fn(&Value) -> bool) -> Value {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let v = s.status.lock().clone().unwrap_or(Value::Null);
        if ok(&v) {
            return v;
        }
        assert!(Instant::now() < end, "scraper status: {v}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn draft() -> Request {
    Request::Draft { lane: "TOP", opponent: None, enemies: AP_TEAM.to_vec(), allies: vec![], regions: None }
}

/// A job runner of its own on the app's data folder, keeping every status it reports.
fn jobs_recording() -> (Arc<Jobs>, Arc<Mutex<Vec<Value>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let me: Arc<OnceLock<Weak<Jobs>>> = Arc::new(OnceLock::new());
    let (s2, m2) = (seen.clone(), me.clone());
    let jobs = Jobs::new(
        load_test_static_data(),
        Arc::new(move || {
            if let Some(j) = m2.get().and_then(|w| w.upgrade()) {
                s2.lock().push(j.status());
            }
        }),
        Arc::new(|games| Some(irelia_server::system::planned_threads(games))),
        Arc::new(|_| anyhow::bail!("no audit here")),
    );
    let _ = me.set(Arc::downgrade(&jobs));
    (jobs, seen)
}

fn evaluate(quick: bool, auto: Option<&str>, path: &'static str, planned: &'static str) -> JobKind {
    JobKind::Evaluate { quick, auto: auto.map(String::from), path, planned }
}

fn spawn_run(jobs: &Arc<Jobs>, job: JobKind) -> tokio::task::JoinHandle<Result<Value, String>> {
    let j = jobs.clone();
    tokio::spawn(async move { j.run(job, None).await })
}

async fn running(jobs: &Arc<Jobs>, n: usize) {
    wait_for("running job", 10, || jobs.status()["running"].as_array().map(|a| a.len()).unwrap_or(0) == n).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_worker_threads() {
    let app = common::start(|db, _| store_games(db, &ap_ad_games(120, 30, 7, ApAd::default()), Some(&load_test_static_data().patch))).await;
    let a = app.app.clone();

    // ---- model thread: builds on its own, answers drafts from the model, and comes back after a crash.
    wait_for("ready model", 120, || a.models.status()["state"] == "ready").await;
    let r = a.models.request(draft(), 60_000).await.unwrap();
    assert!(r["baseline"]["sample"]["totalGames"].as_f64().unwrap() > 0.0, "the lane baseline");
    assert!(!r["draft"].is_null(), "and the draft-adjusted build");
    assert_eq!(r["model"]["games"], 120);

    a.models.crash_for_tests();
    wait_for("restarting model", 10, || a.models.status()["state"] == "loading").await;
    // Until the thread is back, requests fail at once instead of waiting.
    let t0 = Instant::now();
    let e = a.models.request(Request::Status, 10_000).await.unwrap_err();
    assert!(e.contains("restarting"), "{e}");
    assert!(t0.elapsed() < Duration::from_millis(500), "answered at once");
    wait_for("ready model again", 120, || a.models.status()["state"] == "ready").await;
    assert!(!a.models.request(draft(), 60_000).await.unwrap()["draft"].is_null());

    // ---- scraper: Stop is answered at once; paused for a data-folder move, it stays stopped until resumed.
    let scraper = a.scraper.clone();
    let t0 = Instant::now();
    scraper.stop();
    assert!(t0.elapsed() < Duration::from_secs(2), "answered at once");
    scraper.terminate("moving the data folder").await;
    scraper.stop();
    scraper.respawn();

    // ---- scraper: Pause fetching during games stops fetching in champ select and resumes it after.
    // A key that looks right (nothing is fetched while paused; the lanes are stopped again before they get far).
    scraper.set_api_key("RGAPI-00000000-0000-0000-0000-000000000000");
    scraper.set_in_game(true, Some(true));
    scraper.start();
    let st = || scraper.status.lock().clone().unwrap_or(Value::Null);
    let paused = until_status(&scraper, |s| s["paused"] == "game").await;
    assert_eq!(paused["running"], false, "no lanes in champ select");
    assert!(paused["phase"].as_str().unwrap().contains("Paused during champ select"), "{}", paused["phase"]);
    scraper.set_in_game(false, None);
    let resumed = until_status(&scraper, |s| s["running"] == true).await;
    assert!(resumed.get("paused").map(|p| p.is_null()).unwrap_or(true));
    // In a game with the setting off: fetching goes on.
    scraper.set_in_game(true, Some(false));
    assert!(st().get("paused").map(|p| p.is_null()).unwrap_or(true));
    scraper.stop();
    let stopped = until_status(&scraper, |s| s["running"] == false).await;
    assert!(stopped.get("paused").map(|p| p.is_null()).unwrap_or(true));
    // Paused again, then stopped: it doesn't come back after the game.
    scraper.set_pause_in_game(true);
    scraper.set_in_game(false, None);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let idle = st();
    assert_eq!(idle["running"], false);
    assert!(idle.get("paused").map(|p| p.is_null()).unwrap_or(true));

    // ---- job thread: the evaluation reports each step and its result, and Cancel stops it.
    let (jobs, seen) = jobs_recording();
    let result = jobs.run(evaluate(true, None, "auto", "full"), None).await.unwrap();
    let text = result["text"].as_str().unwrap().to_string();
    assert!(text.contains("Gate: FAILED"), "too few games for the gate:\n{text}");
    let statuses = seen.lock().clone();
    let runs: Vec<&Value> = statuses.iter().flat_map(|s| s["running"].as_array().unwrap().iter()).collect();
    let steps: Vec<i64> = runs.iter().filter_map(|r| r["step"].as_i64()).collect();
    let mut sorted = steps.clone();
    sorted.sort();
    assert_eq!(steps, sorted, "steps only move forward");
    assert_eq!(steps[0], 1);
    assert_eq!(Some(*steps.last().unwrap()), runs.last().unwrap()["steps"].as_i64(), "up to the last step");
    // How far it is: from 0, only forward, never past the end.
    let fractions: Vec<f64> = runs.iter().map(|r| r["progress"]["fraction"].as_f64().unwrap_or(-1.0)).collect();
    assert!(!fractions.is_empty() && fractions.iter().all(|f| (0.0..=1.0).contains(f)), "fractions {fractions:?}");
    assert!(fractions.windows(2).all(|w| w[0] <= w[1]), "only forward: {fractions:?}");
    let status = jobs.status();
    assert_eq!(status["running"], serde_json::json!([]));
    assert_eq!(status["last"]["evaluate"]["ok"], true);
    assert_eq!(status["last"]["evaluate"]["text"].as_str(), Some(text.as_str()));
    // Its threads: as many as the cores and the memory allow, said while it runs and kept with its result.
    let threads: Vec<i64> = runs.iter().filter_map(|r| r["threads"].as_i64()).collect();
    assert!(!threads.is_empty() && threads.iter().all(|t| *t >= 1), "threads {threads:?}");
    assert_eq!(status["last"]["evaluate"]["threads"].as_i64(), threads.last().copied());

    // Asked again while it runs: the same run. Cancelled: its callers get "Cancelled", and the status says so.
    let first = spawn_run(&jobs, evaluate(false, None, "auto", "full"));
    running(&jobs, 1).await;
    let second = spawn_run(&jobs, evaluate(false, None, "auto", "full"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(jobs.status()["running"].as_array().unwrap().len(), 1);
    jobs.cancel("evaluate", false).await;
    assert_eq!(first.await.unwrap().unwrap_err(), CANCELLED);
    assert_eq!(second.await.unwrap().unwrap_err(), CANCELLED);
    let status = jobs.status();
    assert_eq!(status["running"], serde_json::json!([]));
    assert_eq!(status["last"]["evaluate"]["cancelled"], true);
    assert!(status["last"]["evaluate"].get("text").is_none());

    // ---- job thread: an automatic evaluation says why, saves its report as eval-auto-… with what it saw, can be
    // paused, and a click joining it makes it yours.
    let result = jobs.run(evaluate(true, Some("300 new games"), "auto", "full"), None).await.unwrap();
    let file = std::path::PathBuf::from(result["file"].as_str().unwrap());
    assert!(file.file_name().unwrap().to_string_lossy().starts_with("eval-auto-"), "{file:?}");
    assert_eq!(jobs.status()["last"]["evaluate"]["auto"], "300 new games");
    let summary: Value = serde_json::from_str(&std::fs::read_to_string(file.parent().unwrap().join("latest.json")).unwrap()).unwrap();
    let mark = a.db.lock().game_mark();
    assert_eq!(summary["mark"]["row"].as_f64(), Some(mark.0 as f64));
    assert_eq!(summary["mark"]["games"].as_f64(), Some(mark.1 as f64));
    assert_eq!(summary["mark"]["patch"].as_str(), Some(load_test_static_data().patch.as_str()));

    let paused = spawn_run(&jobs, evaluate(false, Some("new patch 16.20"), "auto", "full"));
    running(&jobs, 1).await;
    assert_eq!(jobs.status()["running"][0]["auto"], "new patch 16.20");
    jobs.cancel("evaluate", true).await;
    assert_eq!(paused.await.unwrap().unwrap_err(), CANCELLED);
    let last = &jobs.status()["last"]["evaluate"];
    assert_eq!((last["paused"].clone(), last["auto"].clone()), (Value::Bool(true), Value::from("new patch 16.20")));

    let auto = spawn_run(&jobs, evaluate(false, Some("300 new games"), "auto", "full"));
    running(&jobs, 1).await;
    let mine = spawn_run(&jobs, evaluate(false, None, "auto", "full"));
    wait_for("a click making it yours", 10, || jobs.status()["running"][0].get("auto").is_none()).await;
    assert!(jobs.running_job("evaluate").unwrap().get("auto").is_none());
    jobs.cancel("evaluate", false).await;
    assert_eq!(mine.await.unwrap().unwrap_err(), CANCELLED);
    assert_eq!(auto.await.unwrap().unwrap_err(), CANCELLED);
    let last = &jobs.status()["last"]["evaluate"];
    assert!(last.get("auto").is_none() && last.get("paused").is_none());

    // ---- job thread: a full evaluation asked for while a quick update runs replaces it; anything else joins.
    let quick = spawn_run(&jobs, evaluate(false, Some("300 new games"), "auto", "update"));
    running(&jobs, 1).await;
    assert_eq!(jobs.status()["running"][0]["path"], "update");
    let joined = spawn_run(&jobs, evaluate(false, None, "auto", "full"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(jobs.status()["running"].as_array().unwrap().len(), 1, "an automatic request joins it");
    let full = spawn_run(&jobs, evaluate(false, None, "full", "full"));
    assert_eq!(quick.await.unwrap().unwrap_err(), CANCELLED);
    assert_eq!(joined.await.unwrap().unwrap_err(), CANCELLED);
    wait_for("the full one", 10, || jobs.status()["running"].as_array().map(|r| r.len() == 1 && r[0]["path"] != "update").unwrap_or(false)).await;
    let again = spawn_run(&jobs, evaluate(false, None, "full", "full"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(jobs.status()["running"].as_array().unwrap().len(), 1, "a second full request joins the full one");
    jobs.cancel("evaluate", false).await;
    assert_eq!(full.await.unwrap().unwrap_err(), CANCELLED);
    assert_eq!(again.await.unwrap().unwrap_err(), CANCELLED);
}
