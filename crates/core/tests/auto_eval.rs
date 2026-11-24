//! Port of server/autoEval.test.ts: Evaluate builds by itself. With Follow the evaluation on, what the plan says: a quick
//! update after 300 new games, at most every hour; a full evaluation after a new patch or 300 games (3 % on a large
//! database) since the last full one, at most every 6 hours after it; each after 10 minutes without champ select or a
//! game; champ select or a game pauses it. The plan is worked out on or off, for the Data tab. The new-game count
//! survives archiving and compaction; summaries from before 0.8.4 still give one.

use irelia::auto_eval::{mark_of, new_games_since, newest_patch, AutoEvalDeps, AutoEvaluation, AUTO_EVAL};
use irelia::db::Db;
use irelia::model::eval_base::EvalPlan;
use irelia::model::run_eval::{eval_mark, prune_auto_reports};
use irelia::model::service::{EvalSummary, Mark};
use irelia::patches::VersionRow;
use irelia::testkit_model::{ap_ad_games, store_games, ApAd, SynthGame};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;

const H: f64 = 3_600_000.0;
const MIN: f64 = 60_000.0;
const CANCELLED: &str = "Cancelled";

type Done = Box<dyn FnOnce(Result<(), String>) + Send>;

/// A database stand-in: stored game rows (ids only grow) and the builds seen per patch.
#[derive(Default)]
struct FakeDb {
    rows: Vec<i64>,
    versions: Vec<(String, i64)>,
    reads: usize,
}

impl FakeDb {
    fn add(&mut self, n: i64) {
        let top = self.rows.last().copied().unwrap_or(0);
        self.rows.extend((1..=n).map(|i| top + i));
    }
    fn mark(&self) -> (i64, i64) {
        (self.rows.last().copied().unwrap_or(0), self.rows.len() as i64)
    }
    fn after(&self, row: i64) -> i64 {
        self.rows.iter().filter(|r| **r > row).count() as i64
    }
    fn version_rows(&self) -> Vec<VersionRow> {
        self.versions
            .iter()
            .map(|(p, first)| VersionRow { platform: "KR".into(), game_version: format!("{p}.1.1"), patch: p.clone(), first_seen: *first, last_seen: *first, games: 1 })
            .collect()
    }
}

struct State {
    db: FakeDb,
    following: bool,
    quiet: bool,
    busy: bool,
    summary: Option<EvalSummary>,
    running: Option<Option<String>>,
    plan: EvalPlan,
    plan_error: Option<String>,
    started: Vec<String>,
    paths: Vec<String>,
    paused: Vec<f64>,
    plans: usize,
    statuses: Vec<Value>,
    now: f64,
    pending: Option<Done>,
}

struct Fake(Mutex<State>);

impl AutoEvalDeps for Fake {
    fn following(&self) -> bool {
        self.0.lock().following
    }
    fn quiet(&self) -> bool {
        self.0.lock().quiet
    }
    fn busy(&self) -> bool {
        self.0.lock().busy
    }
    fn summary(&self) -> Option<EvalSummary> {
        self.0.lock().summary.clone()
    }
    fn game_mark(&self) -> (i64, i64) {
        let mut s = self.0.lock();
        s.db.reads += 1;
        s.db.mark()
    }
    fn games_after_row(&self, row: i64) -> i64 {
        self.0.lock().db.after(row)
    }
    fn game_versions(&self) -> Vec<VersionRow> {
        self.0.lock().db.version_rows()
    }
    fn plan(&self) -> anyhow::Result<EvalPlan> {
        let mut s = self.0.lock();
        s.plans += 1;
        if let Some(e) = &s.plan_error {
            anyhow::bail!("{e}");
        }
        Ok(s.plan.clone())
    }
    fn running(&self) -> Option<Option<String>> {
        self.0.lock().running.clone()
    }
    fn start(&self, why: String, path: &'static str, done: Done) {
        let mut s = self.0.lock();
        s.started.push(why.clone());
        s.paths.push(path.into());
        s.running = Some(Some(why));
        s.pending = Some(done);
    }
    fn pause(&self) {
        let done = {
            let mut s = self.0.lock();
            let now = s.now;
            s.paused.push(now);
            s.running = None;
            s.pending.take()
        };
        if let Some(d) = done {
            d(Err(CANCELLED.into()));
        }
    }
    fn changed(&self, status: &Value) {
        self.0.lock().statuses.push(status.clone());
    }
    fn now(&self) -> f64 {
        self.0.lock().now
    }
}

struct Harness {
    fake: Arc<Fake>,
    auto: Arc<AutoEvaluation>,
}

fn summary(v: Value) -> EvalSummary {
    serde_json::from_value(v).unwrap()
}

fn harness(evaluated_at: Option<f64>) -> Harness {
    let now = 100.0 * H;
    let mut db = FakeDb { versions: vec![("16.19".into(), 0)], ..Default::default() };
    db.add(1000);
    let fake = Arc::new(Fake(Mutex::new(State {
        db,
        following: true,
        quiet: true,
        busy: false,
        summary: Some(summary(json!({
            "evaluatedAt": evaluated_at.unwrap_or(now - 7.0 * H), "gate": "failed", "bandwidth": 1, "k": 30, "lambda": 1,
            "dataVersion": "1000:0:0:0", "mark": { "row": 1000, "games": 1000, "patch": "16.19" },
        }))),
        running: None,
        // What plan_evaluation says: full until a test says otherwise, as for a summary before 0.16.0.
        plan: EvalPlan { path: "full".into(), reason: "no full evaluation saved by this version yet".into(), new_games: 0.0, since_full: 0.0, full_at: None },
        plan_error: None,
        started: vec![],
        paths: vec![],
        paused: vec![],
        plans: 0,
        statuses: vec![],
        now,
        pending: None,
    })));
    let auto = AutoEvaluation::new(fake.clone());
    Harness { fake, auto }
}

impl Harness {
    fn s(&self) -> parking_lot::MutexGuard<'_, State> {
        self.fake.0.lock()
    }
    fn advance(&self, ms: f64) {
        self.s().now += ms;
    }
    fn now(&self) -> f64 {
        self.s().now
    }
    fn check(&self) {
        self.auto.check();
    }
    fn started(&self) -> Vec<String> {
        self.s().started.clone()
    }
    fn paths(&self) -> Vec<String> {
        self.s().paths.clone()
    }
    fn reads(&self) -> usize {
        self.s().db.reads
    }
    fn plans(&self) -> usize {
        self.s().plans
    }
    fn statuses(&self) -> usize {
        self.s().statuses.len()
    }
    fn paused(&self) -> usize {
        self.s().paused.len()
    }
    fn running(&self) -> Option<Option<String>> {
        self.s().running.clone()
    }
    fn settle(&self, r: Result<(), String>) {
        let done = {
            let mut s = self.s();
            s.running = None;
            s.pending.take().expect("an evaluation running")
        };
        done(r);
    }
    fn finish(&self, patch: Value) {
        {
            let mut s = self.s();
            let mut v = serde_json::to_value(s.summary.clone().unwrap()).unwrap();
            for (k, x) in patch.as_object().unwrap() {
                v[k] = x.clone();
            }
            s.summary = Some(summary(v));
        }
        self.settle(Ok(()));
    }
    fn fail(&self, message: &str) {
        self.settle(Err(message.into()));
    }
}

#[test]
fn off_with_follow_off_on_it_starts_after_300_new_games_once_10_minutes_passed_without_champ_select_or_a_game() {
    let h = harness(None);
    h.s().following = false;
    h.s().db.add(500);
    h.advance(30.0 * MIN);
    h.check();
    assert_eq!((h.auto.status().on, h.started().len(), h.reads()), (false, 0, 0), "off: nothing read, nothing started");
    assert_eq!((h.plans(), h.auto.plan().map(|p| p.path), h.auto.status().path), (1, Some("full".into()), None), "the plan is still worked out");

    h.s().following = true;
    h.s().db.rows.truncate(1000); // back to the evaluation's 1,000 games
    h.s().db.add(299);
    h.check();
    assert_eq!((h.auto.status().new_games, h.auto.status().waiting, h.started().len()), (299.0, None, 0), "not due yet");
    h.s().db.add(1);
    h.s().quiet = false; // champ select
    h.check();
    assert_eq!((h.auto.status().waiting, h.started().len()), (Some("quiet"), 0));
    h.s().quiet = true;
    h.check();
    assert_eq!(h.auto.status().not_before, Some(h.now() + AUTO_EVAL.quiet_ms), "counts from the end of champ select");
    h.advance(9.0 * MIN);
    h.check();
    assert_eq!(h.started().len(), 0);
    h.advance(MIN);
    h.check();
    assert_eq!((h.started(), h.paths()), (vec!["300 new games".to_string()], vec!["full".to_string()]));

    // It ends: its mark is the new baseline.
    let now = h.now();
    h.finish(json!({ "evaluatedAt": now, "mark": { "row": 1300, "games": 1300, "patch": "16.19" } }));
    assert_eq!((h.auto.status().new_games, h.auto.status().waiting), (0.0, None));
    assert!(h.statuses() >= 3, "status changes are pushed");
}

#[test]
fn a_new_patch_starts_one_at_most_every_6_hours_after_the_last_evaluation() {
    let h = harness(Some(100.0 * H - H));
    h.advance(10.0 * MIN);
    let now = h.now() as i64;
    h.s().db.versions.push(("16.20".into(), now));
    h.check();
    let s = h.auto.status();
    assert_eq!((s.new_patch, s.waiting, s.not_before), (Some("16.20".into()), Some("spacing"), Some(100.0 * H + 5.0 * H)));
    h.advance(5.0 * H - 10.0 * MIN - MIN);
    h.check();
    assert_eq!(h.started().len(), 0);
    h.advance(MIN);
    h.check();
    assert_eq!(h.started(), ["new patch 16.20"]);
}

#[test]
fn champ_select_pauses_it_which_doesnt_count_as_a_try_a_failure_waits_6_hours_a_click_joining_it_is_never_paused() {
    let h = harness(None);
    h.s().db.add(400);
    h.advance(10.0 * MIN);
    h.check();
    assert_eq!(h.started().len(), 1);
    h.s().quiet = false;
    h.check();
    assert_eq!(h.paused(), 1, "paused at once");
    assert_eq!(h.running(), None);
    h.s().quiet = true;
    h.check();
    assert_eq!(h.auto.status().waiting, Some("quiet"));
    h.advance(10.0 * MIN);
    h.check();
    assert_eq!(h.started().len(), 2, "starts over after a quiet spell, no 6-hour wait");

    h.fail("The evaluation stopped");
    let failed_at = h.now();
    h.advance(10.0 * MIN);
    h.check();
    let s = h.auto.status();
    assert_eq!((s.waiting, s.not_before, h.started().len()), (Some("spacing"), Some(failed_at + 6.0 * H), 2));

    // A running evaluation you started (or joined with a click): champ select doesn't stop it.
    h.s().running = Some(None);
    h.s().quiet = false;
    h.check();
    assert_eq!(h.paused(), 1);
}

#[test]
fn while_the_data_folder_moves_nothing_is_read_or_started() {
    let h = harness(None);
    h.s().db.add(400);
    h.advance(10.0 * MIN);
    h.s().busy = true;
    let reads = h.reads();
    h.check();
    assert_eq!((h.reads() - reads, h.started().len()), (0, 0));
    h.s().busy = false;
    h.check();
    assert_eq!(h.started().len(), 1);
}

#[test]
fn never_evaluated_every_stored_game_counts_a_summary_from_before_0_8_4_still_gives_its_games_and_patch() {
    let mut db = FakeDb::default();
    db.add(250);
    let since = |db: &FakeDb, m: Option<&Mark>| new_games_since(db.mark(), &|r| db.after(r), m);
    assert_eq!(since(&db, mark_of(None, &[]).as_ref()), 250.0);
    db.versions = vec![("16.18".into(), 1), ("16.19".into(), 50), ("16.20".into(), 200)];
    let old = summary(json!({ "evaluatedAt": 100, "gate": "failed", "bandwidth": 1, "k": 30, "lambda": 1, "dataVersion": "200:99:180:200" }));
    let m = mark_of(Some(&old), &db.version_rows());
    assert_eq!(m, Some(Mark { row: None, games: 200.0, patch: Some("16.19".into()) }));
    assert_eq!(since(&db, m.as_ref()), 50.0);
    assert_eq!(newest_patch(&db.version_rows(), f64::INFINITY), Some("16.20".into()));
}

#[test]
fn new_games_are_counted_by_row_so_archiving_older_games_hides_none_after_compact_database_by_count() {
    let mut db = FakeDb::default();
    db.add(1100);
    db.rows.drain(0..400); // archived: 1..400
    let since = |db: &FakeDb, row: f64| new_games_since(db.mark(), &|r| db.after(r), Some(&Mark { row: Some(row), games: 1000.0, patch: None }));
    assert_eq!(since(&db, 1000.0), 100.0);
    db.rows = (1..=1100).collect(); // compacted: renumbered from 1, the mark's row was 5000
    assert_eq!(since(&db, 5000.0), 100.0);
}

fn renumber(games: Vec<SynthGame>, from: usize) -> Vec<SynthGame> {
    games.into_iter().enumerate().map(|(i, g)| SynthGame { id: format!("KR_{}", from + i), ..g }).collect()
}

#[test]
fn the_real_database_gives_the_mark_and_games_stored_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_default(&dir.path().join("irelia.db")).unwrap();
    store_games(&db, &renumber(ap_ad_games(20, 5, 1, ApAd::default()), 1000), Some("16.19"));
    let mark = eval_mark(&db);
    assert_eq!((mark.games, mark.patch.clone()), (20.0, Some("16.19".to_string())));
    store_games(&db, &renumber(ap_ad_games(6, 5, 2, ApAd::default()), 2000), Some("16.20"));
    db.conn.execute("DELETE FROM matches WHERE match_id LIKE 'KR_1%'", []).unwrap(); // older games go (archived, say)
    assert_eq!(new_games_since(db.game_mark(), &|r| db.games_after_row(r), Some(&mark)), 6.0);
    assert_eq!(newest_patch(&db.game_versions().unwrap(), f64::INFINITY), Some("16.20".into()));
}

#[test]
fn only_the_newest_20_automatic_reports_are_kept_yours_never_go() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("evals");
    std::fs::create_dir_all(&dir).unwrap();
    let stamp = |i: usize| format!("2026-10-{:02}-{:02}-00", 1 + i / 4, i % 4);
    for i in 0..23 {
        for ext in ["txt", "json"] {
            std::fs::write(dir.join(format!("eval-auto-{}.{ext}", stamp(i))), "").unwrap();
        }
    }
    for ext in ["txt", "json"] {
        std::fs::write(dir.join(format!("eval-2026-09-01-10-00.{ext}")), "").unwrap();
    }
    std::fs::write(dir.join("latest.json"), "{}").unwrap();
    prune_auto_reports(&dir, AUTO_EVAL.keep_reports);
    let left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(left.iter().filter(|f| f.starts_with("eval-auto-")).count(), 40);
    assert!(!left.contains(&format!("eval-auto-{}.txt", stamp(2))) && left.contains(&format!("eval-auto-{}.txt", stamp(3))), "the three oldest went");
    assert!(left.contains(&"eval-2026-09-01-10-00.txt".to_string()) && left.contains(&"latest.json".to_string()));
}

#[test]
fn while_the_plan_says_a_quick_update_one_starts_after_300_new_games_or_3_percent_at_most_an_hour_after_the_last() {
    let h = harness(Some(100.0 * H - 30.0 * MIN));
    let n = h.s().db.rows.len() as i64;
    h.s().db.add(100_000 - n);
    let full_at = 100.0 * H - 3.0 * H;
    {
        let mut s = h.s();
        let mut v = serde_json::to_value(s.summary.clone().unwrap()).unwrap();
        v["path"] = json!("update");
        v["mark"] = json!({ "row": 100_000, "games": 100_000, "patch": "16.19" });
        v["full"] = json!({ "evaluatedAt": full_at, "mark": { "row": 99_000, "games": 99_000, "patch": "16.19" } });
        s.summary = Some(summary(v));
    }
    let reason = "300 new games since the last evaluation (full evaluation of 21 Oct)";
    h.s().plan = EvalPlan { path: "update".into(), reason: reason.into(), new_games: 300.0, since_full: 1_300.0, full_at: Some(full_at) };
    h.s().db.add(299);
    h.advance(11.0 * MIN);
    h.check();
    let s = h.auto.status();
    assert_eq!(
        (s.path.as_deref(), s.reason.as_deref(), s.needed, s.update_games, s.new_games, s.since_full, h.started().len()),
        (Some("update"), Some(reason), 3_000.0, 3_000.0, 299.0, 1_299.0, 0),
        "3 % of the 100,000 games the last one saw"
    );
    h.s().db.add(2_701);
    h.check();
    let s = h.auto.status();
    assert_eq!((s.waiting, s.not_before), (Some("spacing"), Some(100.0 * H + 30.0 * MIN)), "an hour after the last evaluation");
    h.advance(19.0 * MIN);
    h.check();
    assert_eq!((h.started(), h.paths()), (vec!["3,000 new games".to_string()], vec!["update".to_string()]));

    // It fails: the next try an hour later, not six.
    h.fail("The evaluation stopped");
    let failed_at = h.now();
    h.advance(MIN);
    h.check();
    let s = h.auto.status();
    assert_eq!((s.waiting, s.not_before), (Some("spacing"), Some(failed_at + H)));
}

#[test]
fn when_the_plan_says_full_games_since_the_last_full_evaluation_count_and_it_runs_6_hours_after_that_one_at_the_earliest() {
    let h = harness(None);
    let n = h.s().db.rows.len() as i64;
    h.s().db.add(20_500 - n);
    let full_at = h.now() - 2.0 * H;
    {
        let now = h.now();
        let mut s = h.s();
        let mut v = serde_json::to_value(s.summary.clone().unwrap()).unwrap();
        v["evaluatedAt"] = json!(now - 30.0 * MIN);
        v["path"] = json!("update");
        v["mark"] = json!({ "row": 20_400, "games": 20_400, "patch": "16.19" });
        v["full"] = json!({ "evaluatedAt": full_at, "mark": { "row": 20_000, "games": 20_000, "patch": "16.19" } });
        s.summary = Some(summary(v));
    }
    h.s().plan = EvalPlan { path: "full".into(), reason: "settings the evaluation depends on changed".into(), new_games: 100.0, since_full: 500.0, full_at: Some(full_at) };
    h.advance(11.0 * MIN);
    h.check();
    let s = h.auto.status();
    assert_eq!((s.path.as_deref(), s.new_games, s.since_full, s.needed, s.waiting), (Some("full"), 100.0, 500.0, 600.0, None), "not due: 3 % of 20,000");
    h.s().db.add(100);
    h.check();
    let s = h.auto.status();
    assert_eq!((s.waiting, s.not_before, h.started().len()), (Some("spacing"), Some(full_at + 6.0 * H), 0), "from the full evaluation, not the quick update since");
    let now = h.now();
    h.advance(full_at + 6.0 * H - now);
    h.check();
    assert_eq!((h.started(), h.paths()), (vec!["600 games since the last full evaluation".to_string()], vec!["full".to_string()]));

    h.fail("The evaluation stopped");
    let failed_at = h.now();
    h.advance(2.0 * H);
    h.check();
    let s = h.auto.status();
    assert_eq!((s.waiting, s.not_before), (Some("spacing"), Some(failed_at + 6.0 * H)), "a failed full evaluation: 6 hours");
}

#[test]
fn the_plan_is_worked_out_at_each_check_not_while_an_evaluation_runs_or_the_folder_moves_up_to_date_nothing_starts_a_failing_plan_keeps_the_last() {
    let h = harness(None);
    h.s().plan = EvalPlan { path: "none".into(), reason: "up to date: no new games since the last evaluation".into(), new_games: 0.0, since_full: 0.0, full_at: Some(1.0) };
    h.s().db.add(400);
    h.advance(11.0 * MIN);
    h.check();
    let s = h.auto.status();
    assert_eq!(
        (s.path.as_deref(), s.reason.as_deref(), s.waiting, h.started().len()),
        (Some("none"), Some("up to date: no new games since the last evaluation"), None, 0)
    );

    // Off: the plan changing is still pushed (the Data tab shows it).
    h.s().following = false;
    h.check();
    let pushed = h.statuses();
    h.s().plan = EvalPlan { path: "update".into(), reason: "400 new games since the last evaluation".into(), new_games: 400.0, since_full: 400.0, full_at: Some(1.0) };
    h.check();
    assert_eq!((h.statuses(), h.auto.plan().map(|p| p.path), h.started().len()), (pushed + 1, Some("update".into()), 0));

    h.s().following = true;
    h.check();
    assert_eq!(h.paths(), ["update"]);
    let plans = h.plans();
    h.check();
    assert_eq!(h.plans(), plans, "not while it runs: its base is being written");
    let now = h.now();
    h.finish(json!({ "evaluatedAt": now, "path": "update", "mark": { "row": 1400, "games": 1400, "patch": "16.19" } }));
    assert_eq!(h.plans(), plans + 1, "once it ends");

    h.s().busy = true;
    h.check();
    assert_eq!(h.plans(), plans + 1, "not while the data folder moves");
    h.s().busy = false;
    h.s().plan_error = Some("scrape.config.json could not be read".into());
    h.check();
    assert_eq!((h.plans(), h.auto.plan().map(|p| p.path)), (plans + 2, Some("update".into())));
}

#[test]
fn on_a_large_database_3_percent_of_the_games_the_last_evaluation_saw() {
    let h = harness(None);
    let n = h.s().db.rows.len() as i64;
    h.s().db.add(100_000 - n);
    {
        let mut s = h.s();
        let mut v = serde_json::to_value(s.summary.clone().unwrap()).unwrap();
        v["mark"] = json!({ "row": 100_000, "games": 100_000, "patch": "16.19" });
        s.summary = Some(summary(v));
    }
    h.s().db.add(2_999);
    h.check();
    let s = h.auto.status();
    assert_eq!((s.needed, s.new_games, h.started().len()), (3_000.0, 2_999.0, 0), "not due at 2,999");
    h.s().db.add(1);
    h.advance(11.0 * MIN);
    h.check();
    assert_eq!(h.auto.status().new_games, 3_000.0);
    assert!(h.started().len() == 1 || h.auto.status().waiting.is_some(), "due at 3,000");
}

#[test]
fn quick_updates_reports_are_pruned_by_their_time_among_the_full_ones() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("evals-mixed");
    std::fs::create_dir_all(&dir).unwrap();
    let stamp = |i: usize| format!("2026-11-{:02}-10-00", 1 + i);
    // Old quick updates first, then newer full ones: the old quick updates go, not the newer full reports.
    for i in 0..22 {
        for ext in ["txt", "json"] {
            std::fs::write(dir.join(format!("eval-auto-{}{}.{ext}", if i < 5 { "update-" } else { "" }, stamp(i))), "").unwrap();
        }
    }
    prune_auto_reports(&dir, AUTO_EVAL.keep_reports);
    let left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(left.len(), 40);
    assert!(!left.contains(&format!("eval-auto-update-{}.txt", stamp(1))) && left.contains(&format!("eval-auto-update-{}.txt", stamp(2))), "the two oldest went");
    assert!(left.contains(&format!("eval-auto-{}.txt", stamp(5))), "the full reports after them stay");
}
