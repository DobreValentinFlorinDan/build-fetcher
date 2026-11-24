//! Port of server/model/evalUpdate.test.ts: quick updates (0.16.0, docs/EVAL-UPDATE.md). What a quick update adds to
//! its base is what a full evaluation measures for the new games, and with no new game it is the full evaluation again.

use irelia::db::Db;
use irelia::engine::exclusivity::build_exclusivity_index;
use irelia::engine::recommend::EngineSettings;
use irelia::model::eval_base::{keep_verdicts, plan_evaluation, regions_now, update_from, verdict_shift, BaseHeader, RegionPair, UPDATE_RULES};
use irelia::model::evaluate::{
    eval_defaults, eval_steps, evaluate, group_outcomes, merge_outcomes, EvalBaseData, EvalInput, EvalOptions, EvalProgress, EvalReport, Outcome,
};
use irelia::model::profiles::{ChampionProfiles, PROFILE_DEFAULTS};
use irelia::model::run_eval::{eval_mark, eval_reads};
use irelia::model::service::{EvalSummary, Mark};
use irelia::testkit::load_test_static_data;
use irelia::testkit_model::{ap_ad_games, model_inputs, store_games, ApAd, SynthGame};
use serde_json::Value;
use std::sync::{Arc, Mutex};

fn opts() -> EvalOptions {
    let engine = EngineSettings {
        min_matchup_games: 15.0,
        general_prior_weight: 0.15,
        min_lane_games: 30.0,
        other_lane_weight: 0.15,
        previous_patch_weight: 0.35,
        min_current_patch_games: 40.0,
        starting_item_window_ms: 60_000.0,
    };
    EvalOptions { lambdas: vec![1.0], bandwidths: vec![0.5, 0.7], ks: vec![30.0, 100.0], bootstrap: 300, live: false, ..eval_defaults(engine) }
}

fn header() -> BaseHeader {
    let m = Mark { row: Some(0.0), games: 0.0, patch: None };
    BaseHeader {
        logic: 1.0,
        full_at: 0.0,
        full_mark: m.clone(),
        full_held_out: 0.0,
        full_patch: None,
        full_current: 0.0,
        last_at: 0.0,
        last_mark: m,
        known: 0.0,
        patches: vec![],
        regions: vec![],
        key: String::new(),
        portable: None,
        imported_from: None,
    }
}

struct Run {
    report: EvalReport,
    base: EvalBaseData,
    opts: EvalOptions,
}

fn run(games: &[SynthGame], base: Option<&EvalBaseData>, progress: Option<&(dyn Fn(EvalProgress) + Sync)>) -> Run {
    let data = load_test_static_data();
    let (rows, participants, totals) = model_inputs(games);
    let mut o = opts();
    if let Some(b) = base {
        let h = BaseHeader { full_held_out: b.known.len() as f64, known: b.known.len() as f64, ..header() };
        o.update = Some(update_from(b, &h, o.max_grid_games));
    }
    let out = evaluate(
        EvalInput {
            rows,
            timelines_of: None,
            classic_patches: None,
            participants,
            excl: Arc::new(build_exclusivity_index(&data)),
            profiles: Arc::new(ChampionProfiles::new(&totals, &[], data.clone(), PROFILE_DEFAULTS)),
            data,
            region_patch: Arc::new(|_| None),
            rec_log: vec![],
            starting_item_window_ms: 60_000.0,
            log: Box::new(|_| {}),
            progress: Box::new(move |p| {
                if let Some(f) = progress {
                    f(p)
                }
            }),
            threads: 1,
            base: base.cloned(),
            salt: Some(base.map(|b| b.salt.clone()).unwrap_or_else(|| "test-salt".into())),
            control: None,
            prepared_hook: None,
        },
        &o,
    )
    .unwrap();
    Run { report: out.report, base: out.next.expect("a next base"), opts: o }
}

/// A report without what differs between two runs on the same data by design.
fn comparable(r: &EvalReport) -> Value {
    let mut v = serde_json::to_value(r).unwrap();
    let m = v.as_object_mut().unwrap();
    m.remove("evaluatedAt");
    m.remove("update");
    v
}

fn games() -> Vec<SynthGame> {
    ap_ad_games(800, 40, 21, ApAd { no_effect: false, random_opponent: true })
}

#[test]
fn with_no_new_game_it_is_the_full_evaluation_again_number_for_number() {
    let g = games();
    let full = run(&g, None, None);
    assert_eq!(full.report.gate, "passed", "{}", full.report.gate_reason);
    assert_eq!(full.base.known.len(), 800, "every held-out game measured, by its hash");
    assert!(full.base.final_.as_ref().unwrap().0.pilots.iter().all(|p| !p.starts_with("pilot")), "the base names pilots by a hash only");
    let again = run(&g, Some(&full.base), None);
    assert_eq!(again.report.update.as_ref().unwrap().new_games, 0.0);
    assert_eq!(comparable(&again.report), comparable(&full.report));
    assert_eq!(verdict_shift(&full.report, &again.report), (None, vec![]));
    assert_eq!(again.base.known.len(), 800);
}

#[test]
fn the_new_games_are_added_to_the_base_every_gate_decided_on_all_of_them() {
    let g = games();
    let older: Vec<SynthGame> = g.iter().enumerate().filter(|(i, _)| i % 5 != 0).map(|(_, x)| x.clone()).collect();
    let first = run(&older, None, None);
    let steps: Mutex<Vec<EvalProgress>> = Mutex::new(vec![]);
    let record = |p: EvalProgress| {
        if p.part.is_none() {
            steps.lock().unwrap().push(p)
        }
    };
    let quick = run(&g, Some(&first.base), Some(&record));
    let full = run(&g, None, None);
    let u = quick.report.update.as_ref().unwrap();
    assert_eq!(u.new_games, 160.0);
    assert_eq!(u.missing, Some(0.0));
    assert_eq!(quick.base.known.len(), 800);
    // Every game's outcomes once: the decisions' counts are the full evaluation's.
    let ns = |r: &EvalReport| r.decisions.iter().map(|d| d.n).collect::<Vec<_>>();
    assert_eq!(ns(&quick.report), ns(&full.report));
    assert_eq!(quick.report.brier.n, full.report.brier.n);
    assert_eq!(quick.report.pilots, full.report.pilots);
    // The base's grid and choice stand; the verdicts here are the full evaluation's.
    assert_eq!(quick.report.grid, first.report.grid);
    assert_eq!(quick.report.chosen, first.report.chosen);
    assert_eq!(verdict_shift(&full.report, &quick.report), (None, vec![]));
    assert!((quick.report.lift.delta - full.report.lift.delta).abs() < 0.02, "{} against {}", quick.report.lift.delta, full.report.lift.delta);
    // Its steps, as eval_steps counts them, in order.
    let steps = steps.into_inner().unwrap();
    assert_eq!(steps.len(), eval_steps(&quick.opts));
    assert_eq!(steps.iter().map(|s| s.step).collect::<Vec<_>>(), (1..=steps.len()).collect::<Vec<_>>());
    assert!(steps.iter().any(|s| s.label.contains("160 new held-out games")), "{}", steps.iter().map(|s| s.label.as_str()).collect::<Vec<_>>().join(" | "));
}

#[test]
fn games_the_base_measured_that_are_gone_still_count_and_are_reported() {
    let g = games();
    let first = run(&g, None, None);
    let fewer = run(&g[..760], Some(&first.base), None);
    let u = fewer.report.update.as_ref().unwrap();
    assert_eq!(u.new_games, 0.0);
    assert_eq!(u.missing, Some(40.0));
    let ns = |r: &EvalReport| r.decisions.iter().map(|d| d.n).collect::<Vec<_>>();
    assert_eq!(ns(&fewer.report), ns(&first.report));
}

#[test]
fn merged_outcomes_are_what_grouping_them_all_at_once_gives() {
    let o = |pilot: &str, key: &'static str, wpa: f64, d: bool, b: bool| Outcome { pilot: pilot.into(), key, wpa, followed_draft: d, followed_baseline: b, w: None };
    let a = vec![o("x", "keystone", 0.1, true, false), o("y", "core1", -0.2, false, true), o("x", "core1", 0.3, true, true)];
    let b = vec![o("z", "keystone", 0.05, false, false), o("x", "keystone", -0.1, true, true), o("z", "core1", 0.2, true, false)];
    let merged = merge_outcomes(&group_outcomes(&a, None), &group_outcomes(&b, None), &|p| p.to_string());
    let all: Vec<Outcome> = a.into_iter().chain(b).collect();
    assert_eq!(merged, group_outcomes(&all, None));
}

fn report(low: f64, high: f64, gate: &str) -> EvalReport {
    let mut r = run_tiny();
    r.gate = gate.into();
    r.gate_reason = "x".into();
    r.lift.draft = 0.0;
    r.lift.baseline = 0.0;
    r.lift.delta = (low + high) / 2.0;
    r.lift.low = low;
    r.lift.high = high;
    r.memory = None;
    r.scoring = None;
    r.tuning = None;
    r.swaps = None;
    r
}

/// Any report, for its shape.
fn run_tiny() -> EvalReport {
    static R: std::sync::OnceLock<EvalReport> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        let mut o = opts();
        o.memory = None;
        o.scoring = false;
        o.tuning = None;
        o.swaps = false;
        let g = ap_ad_games(200, 20, 5, ApAd::default());
        let data = load_test_static_data();
        let (rows, participants, totals) = model_inputs(&g);
        evaluate(
            EvalInput {
                rows,
                timelines_of: None,
                classic_patches: None,
                participants,
                excl: Arc::new(build_exclusivity_index(&data)),
                profiles: Arc::new(ChampionProfiles::new(&totals, &[], data.clone(), PROFILE_DEFAULTS)),
                data,
                region_patch: Arc::new(|_| None),
                rec_log: vec![],
                starting_item_window_ms: 60_000.0,
                log: Box::new(|_| {}),
                progress: Box::new(|_| {}),
                threads: 1,
                base: None,
                salt: None,
                control: None,
                prepared_hook: None,
            },
            &o,
        )
        .unwrap()
        .report
    })
    .clone()
}

#[test]
fn a_verdict_the_new_games_only_move_onto_the_line_stands_one_moved_clearly_across_goes_to_a_full_evaluation() {
    let passed = report(0.001, 0.009, "passed");
    let name = "the draft-adjusted builds".to_string();
    // Half-width 0.004: the margin is a quarter of it, 0.001.
    assert_eq!(verdict_shift(&passed, &report(-0.0005, 0.0075, "failed")), (None, vec![name.clone()]));
    assert_eq!(verdict_shift(&passed, &report(-0.004, 0.004, "failed")), (Some(name.clone()), vec![]));
    assert_eq!(verdict_shift(&passed, &report(0.002, 0.01, "passed")), (None, vec![]));
    let kept = keep_verdicts(&passed, &report(-0.0005, 0.0075, "failed"), &[name]);
    assert_eq!(kept.gate, "passed");
    assert!(kept.gate_reason.contains("stands"), "{}", kept.gate_reason);
    assert_eq!(UPDATE_RULES.margin, 0.25);
}

fn renumber(games: Vec<SynthGame>, from: usize) -> Vec<SynthGame> {
    games.into_iter().enumerate().map(|(i, g)| SynthGame { id: format!("KR_{}", from + i), ..g }).collect()
}

#[test]
fn plan_quick_while_the_full_evaluation_still_describes_the_games_full_on_a_new_patch_a_region_moving_growth_or_age() {
    let cfg = tempfile::tempdir().unwrap();
    std::env::set_var("IRELIA_CONFIG_DIR", cfg.path());
    let data = load_test_static_data();
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_default(&dir.path().join("irelia.db")).unwrap();
    // 900 games of the previous patch and 100 of today's.
    store_games(&db, &renumber(ap_ad_games(900, 40, 1, ApAd::default()), 100_000), Some(&data.previous_patch));
    store_games(&db, &renumber(ap_ad_games(100, 40, 2, ApAd::default()), 200_000), Some(&data.patch));
    let day = 86_400_000.0;
    let now = irelia::util::now_ms() as f64;
    let mark = eval_mark(&db);
    let h = BaseHeader {
        full_at: now - day,
        full_mark: mark.clone(),
        full_held_out: 1000.0,
        full_patch: Some(data.patch.clone()),
        full_current: 100.0,
        last_at: now - day,
        last_mark: mark.clone(),
        known: 1000.0,
        patches: eval_reads(&db, &data, false).unwrap().read,
        regions: regions_now(&db, &data).unwrap(),
        key: "k".into(),
        ..header()
    };
    let summary: EvalSummary = serde_json::from_value(serde_json::json!({
        "evaluatedAt": now - day, "gate": "passed", "bandwidth": 0.7, "k": 100, "lambda": 1, "dataVersion": "x", "mark": mark,
    }))
    .unwrap();
    let plan = |h: &BaseHeader, key: &str, at: f64| plan_evaluation(&db, &data, at, Some(Some(h.clone())), Some(Some(summary.clone())), Some(key.into())).unwrap();
    assert_eq!(plan(&h, "k", now).path, "none", "{}", plan(&h, "k", now).reason);
    // 40 games of the previous patch and 10 of today's.
    store_games(&db, &renumber(ap_ad_games(40, 40, 3, ApAd::default()), 300_000), Some(&data.previous_patch));
    store_games(&db, &renumber(ap_ad_games(10, 40, 4, ApAd::default()), 400_000), Some(&data.patch));
    let p = plan(&h, "k", now);
    assert_eq!((p.path.as_str(), p.new_games), ("update", 50.0), "{}", p.reason);
    assert_eq!(plan(&h, "other", now).path, "full");
    assert!(plan(&h, "k", now + 7.0 * day).reason.contains("over 7 days old"));
    let fewer = BaseHeader { patches: h.patches[1..].to_vec(), ..h.clone() };
    let r = plan(&fewer, "k", now).reason;
    assert!(r.contains("new patch") || r.contains("patches"), "{r}");
    let moved = BaseHeader { regions: vec![("KR".into(), RegionPair { patch: "1.1".into(), previous_patch: "1.0".into() })], ..h.clone() };
    let r = plan(&moved, "k", now).reason;
    assert!(r.contains("is on patch") || r.contains("regions"), "{r}");
    // Today's patch: over 30 % more than the full evaluation had.
    store_games(&db, &renumber(ap_ad_games(21, 40, 5, ApAd::default()), 500_000), Some(&data.patch));
    let r = plan(&h, "k", now).reason;
    assert!(r.contains(&format!("31 more games of patch {}", data.patch)), "{r}");
    // Over 20 % more games in all (checked first).
    store_games(&db, &renumber(ap_ad_games(130, 40, 6, ApAd::default()), 600_000), Some(&data.previous_patch));
    let r = plan(&h, "k", now).reason;
    assert!(r.contains("over 20 % more"), "{r}");
    assert_eq!(plan_evaluation(&db, &data, now, Some(None), Some(Some(summary.clone())), Some("k".into())).unwrap().path, "full");
}
