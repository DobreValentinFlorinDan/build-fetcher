//! Port of server/model/evaluate.test.ts: the evaluation (eval:builds) on synthetic games (B6). The gate passes when the
//! draft carries real information the lane baseline can't see, and fails when it carries none.
//!
//! 0.x's two memory tests ("keeps only the timelines the live replay reads", "read one game at a time") watched the
//! JavaScript rows being emptied in place; in Rust the rows are moved into the evaluation and dropped as it goes, so what
//! is left to check is the part that matters: reading the replay's timelines again gives the same report.

use irelia::engine::exclusivity::build_exclusivity_index;
use irelia::engine::recommend::EngineSettings;
use irelia::model::dataset::{prepare_games, score_games, PrepareOptions};
use irelia::model::evaluate::{
    eval_defaults, eval_steps, evaluate, group_outcomes, purchase_chances, within_pilot_of, EvalInput, EvalOptions, EvalProgress, EvalReport, Outcome,
    TuningOptions,
};
use irelia::model::profiles::{ChampionProfiles, PROFILE_DEFAULTS};
use irelia::model::run_eval::format_eval;
use irelia::model::state::{state_features_names, StateModel};
use irelia::testkit::load_test_static_data;
use irelia::testkit_model::{ap_ad_games, model_inputs, ApAd, SynthGame};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

fn engine() -> EngineSettings {
    EngineSettings {
        min_matchup_games: 15.0,
        general_prior_weight: 0.15,
        min_lane_games: 30.0,
        other_lane_weight: 0.15,
        previous_patch_weight: 0.35,
        min_current_patch_games: 40.0,
        starting_item_window_ms: 60_000.0,
    }
}

fn opts() -> EvalOptions {
    EvalOptions { lambdas: vec![1.0], bandwidths: vec![0.7], ks: vec![100.0], bootstrap: 300, live: true, ..eval_defaults(engine()) }
}

fn input<'a>(games: &[SynthGame], classic: Option<Vec<String>>, progress: Box<dyn Fn(EvalProgress) + Sync + 'a>) -> EvalInput<'a> {
    let data = load_test_static_data();
    let (rows, participants, totals) = model_inputs(games);
    EvalInput {
        rows,
        timelines_of: None,
        classic_patches: classic,
        participants,
        excl: Arc::new(build_exclusivity_index(&data)),
        profiles: Arc::new(ChampionProfiles::new(&totals, &[], data.clone(), PROFILE_DEFAULTS)),
        data,
        region_patch: Arc::new(|_| None),
        rec_log: vec![],
        starting_item_window_ms: 60_000.0,
        log: Box::new(|_| {}),
        progress,
        threads: 1,
        base: None,
        salt: None,
        control: None,
        prepared_hook: None,
    }
}

fn games(effect: bool) -> Vec<SynthGame> {
    ap_ad_games(800, 40, 21, ApAd { no_effect: !effect, random_opponent: true })
}

fn run(effect: bool, live: bool, o: impl FnOnce(&mut EvalOptions)) -> EvalReport {
    let mut opts = EvalOptions { live, ..opts() };
    o(&mut opts);
    evaluate(input(&games(effect), None, Box::new(|_| {})), &opts).unwrap().report
}

#[test]
fn the_draft_model_passes_the_gate_when_the_enemy_comp_decides_the_best_first_item() {
    let r = run(true, true, |_| {});
    assert_eq!(r.games, 800.0);
    assert_eq!(r.pilots, 40.0);
    let core1 = r.decisions.iter().find(|d| d.key == "core1").unwrap();
    assert!(core1.draft > 0.15, "draft lift on the first item {}", core1.draft);
    assert!(core1.baseline.abs() < 0.1, "the baseline can't see the comp ({})", core1.baseline);
    assert!(r.lift.low > 0.0, "CI {}..{}", r.lift.low, r.lift.high);
    assert!(r.brier.draft <= r.brier.baseline);
    assert_eq!(r.gate, "passed", "{}", r.gate_reason);
    let s = r.stability.as_ref().unwrap();
    assert!(s.games > 0.0 && s.flips_per_game < 1.0);
    assert!(format_eval(&r).contains("Gate: PASSED"));
}

#[test]
fn with_no_draft_effect_the_gate_fails() {
    let r = run(false, false, |_| {});
    assert_eq!(r.gate, "failed");
    assert!(r.gate_reason.contains("interval"), "{}", r.gate_reason);
}

#[test]
fn reports_every_step_in_order_and_replays_at_most_max_live_games_held_out_games() {
    let seen: Mutex<Vec<EvalProgress>> = Mutex::new(vec![]);
    let mut o = EvalOptions { lambdas: vec![0.3, 1.0], bandwidths: vec![0.7, f64::INFINITY], ks: vec![100.0], max_live_games: 120, ..opts() };
    o.live = true;
    let r = evaluate(input(&games(true), None, Box::new(|p| seen.lock().unwrap().push(p))), &o).unwrap().report;
    let folds = o.folds;
    let steps = eval_steps(&o);
    assert_eq!(
        steps,
        2 + folds + 2 + folds + 2 + 2 + folds + 1 + 1 + 1,
        "two penalties, one step per fold, the final comparison, the interval, patch memory (per fold, all games, early), scoring against the draft, fine-tuning (per fold, all games), late-game swaps, the replay"
    );
    let seen = seen.into_inner().unwrap();
    // Each step starts once, in order; between them, how far into the step, never back.
    let starts: Vec<&EvalProgress> = seen.iter().filter(|p| p.part.is_none()).collect();
    assert_eq!(starts.iter().map(|p| p.step).collect::<Vec<_>>(), (1..=steps).collect::<Vec<_>>());
    assert!(seen.iter().all(|p| p.steps == steps && !p.label.is_empty()));
    assert!(starts.last().unwrap().label.starts_with("Replaying "), "{}", starts.last().unwrap().label);
    let (mut step, mut part) = (0usize, 0.0f64);
    for p in &seen {
        match p.part {
            None => {
                step = p.step;
                part = 0.0;
            }
            Some(x) => {
                assert!(p.step == step && x >= part && x <= 1.0, "step {} ({}) at {x} after step {step} at {part}", p.step, p.label);
                part = x;
            }
        }
    }
    // An even subsample: every fold rounds up, so up to one game more per fold.
    let g = r.stability.as_ref().unwrap().games;
    assert!(g > 0.0 && g <= (120 + folds) as f64, "replayed {g}");
}

#[test]
fn the_replays_timelines_read_again_give_the_same_report() {
    let o = EvalOptions { lambdas: vec![1.0], bandwidths: vec![0.7], ks: vec![100.0], bootstrap: 50, live: true, max_live_games: 100, memory: None, ..opts() };
    let g = games(true);
    let kept = evaluate(input(&g, None, Box::new(|_| {})), &o).unwrap().report;
    // The same games; the replay reads its sample's timelines again instead of keeping them from the start.
    let mut streamed = input(&g, None, Box::new(|_| {}));
    let raw: HashMap<String, String> = streamed.rows.iter().map(|r| (r.match_id.clone(), r.timeline.clone().unwrap())).collect();
    let asked = Arc::new(Mutex::new(Vec::<String>::new()));
    let a2 = asked.clone();
    streamed.timelines_of = Some(Box::new(move |ids: &[String]| {
        a2.lock().unwrap().extend(ids.iter().cloned());
        ids.iter().map(|id| (id.clone(), raw[id].clone())).collect()
    }));
    let after = evaluate(streamed, &o).unwrap().report;
    let n = asked.lock().unwrap().len();
    assert!(n > 50 && n <= 100 + o.folds, "the replay asked for {n}");
    let mut x = serde_json::to_value(&kept).unwrap();
    let mut y = serde_json::to_value(&after).unwrap();
    x.as_object_mut().unwrap().remove("evaluatedAt");
    y.as_object_mut().unwrap().remove("evaluatedAt");
    assert_eq!(y, x);
}

/// Patch memory against the two-patch model: 300 games of today's and the previous patch, 900 of the three patches
/// before, from the same pilots.
fn run_memory(older: impl Fn(SynthGame) -> SynthGame) -> EvalReport {
    let recent: Vec<SynthGame> = ap_ad_games(300, 40, 31, ApAd { no_effect: false, random_opponent: true })
        .into_iter()
        .enumerate()
        .map(|(i, g)| SynthGame { version: Some(if i % 3 == 2 { "16.18.1.1" } else { "16.19.1.1" }.into()), ..g })
        .collect();
    let past: Vec<SynthGame> = ap_ad_games(900, 40, 32, ApAd { no_effect: false, random_opponent: true })
        .into_iter()
        .enumerate()
        .map(|(i, g)| older(SynthGame { id: format!("KR_{}", 30_000 + i), version: Some(format!("16.{}.1.1", 17 - (i % 3))), ..g }))
        .collect();
    let all: Vec<SynthGame> = recent.into_iter().chain(past).collect();
    let o = EvalOptions { live: false, min_games: 100, ..opts() };
    evaluate(input(&all, Some(vec!["16.19".into(), "16.18".into()]), Box::new(|_| {})), &o).unwrap().report
}

#[test]
fn patch_memory_passes_when_older_patches_still_hold_and_leaves_the_two_patch_evaluation_as_it_was() {
    let r = run_memory(|g| g);
    assert_eq!(r.games, 300.0, "everything else is evaluated on the two-patch games");
    let m = r.memory.as_ref().unwrap();
    assert_eq!(m.older_games, 900.0);
    assert_eq!(m.grid.len(), opts().memory.unwrap().decays.len());
    assert!(m.early.delta > 0.0 && m.early.low > 0.0, "early in a patch {} ({}..{})", m.early.delta, m.early.low, m.early.high);
    assert!(m.full.delta >= 0.0, "all games {}", m.full.delta);
    assert_eq!(m.gate, "passed", "{}", m.gate_reason);
    assert!(format_eval(&r).contains("Patch memory: PASSED"));
}

#[test]
fn patch_memory_fails_when_older_patches_no_longer_hold() {
    // On the older patches Sterak's Gage and BotRK swapped roles: what won against AP lost against it.
    let r = run_memory(|mut g| {
        g.items[0] = if g.items[0] == 3053 { 3153 } else { 3053 };
        g
    });
    let m = r.memory.as_ref().unwrap();
    assert!(m.full.memory < m.full.classic, "all games: {} vs {}", m.full.memory, m.full.classic);
    assert!(m.full.brier > m.full.classic_brier);
    assert_eq!(m.gate, "failed");
}

#[test]
fn patch_memory_needs_games_from_older_patches_to_be_judged() {
    let r = run(true, false, |_| {});
    let m = r.memory.as_ref().unwrap();
    assert_eq!(m.older_games, 0.0);
    assert_eq!(m.gate, "failed");
    assert!(m.gate_reason.contains("older patches"), "{}", m.gate_reason);
    assert!(run(true, false, |o| o.memory = None).memory.is_none());
}

#[test]
fn the_purchases_chances_computed_once_are_what_scoring_a_game_reads_of_them() {
    let data = load_test_static_data();
    let (rows, participants, totals) = model_inputs(&ap_ad_games(200, 20, 5, ApAd { no_effect: false, random_opponent: true }));
    let rows: Vec<Arc<_>> = rows.into_iter().map(Arc::new).collect();
    let profiles = ChampionProfiles::new(&totals, &[], data.clone(), PROFILE_DEFAULTS);
    let games: Vec<Arc<_>> = prepare_games(&rows, &participants, &data, &profiles, &PrepareOptions { starting_item_window_ms: 60_000.0 }).into_iter().map(Arc::new).collect();
    assert!(games.iter().any(|g| g.items.iter().any(|c| c.x.is_some())) && games.iter().any(|g| g.boots.as_ref().map(|b| b.x.is_some()).unwrap_or(false)));
    // A fold with a state model and one without (its purchases score against the base rate).
    let features = state_features_names();
    let model = StateModel {
        version: 1,
        beta: (0..features.len()).map(|i| ((i % 5) as f64 - 2.0) * 0.013).collect(),
        features,
        lambda: 1.0,
        rows: 0.0,
        games: 0.0,
        base: 0.53,
    };
    let models = vec![Some(model), None];
    let refs: Vec<&_> = games.iter().map(|g| g.as_ref()).collect();
    let chances = purchase_chances(&refs, &models);
    for (f, m) in models.iter().enumerate() {
        for (k, s) in score_games(&games, m.as_ref(), None).iter().enumerate() {
            let at = (k * models.len() + f) * 7;
            for (slot, wpa) in s.item_wpa.iter().enumerate() {
                assert_eq!(*wpa, s.g.win - chances[at + slot]);
            }
            if let Some(b) = s.boots_wpa {
                assert_eq!(b, s.g.win - chances[at + 6]);
            }
        }
    }
}

#[test]
fn fine_tuning_tries_each_setting_against_the_chosen_model_and_reports_them() {
    let r = run(true, false, |o| {
        o.tuning = Some(TuningOptions { pilot_strengths: vec![0.0, 2.0], recency_days: vec![14.0] });
        o.ks = vec![30.0, 100.0];
        o.memory = None;
        o.scoring = false;
    });
    let t = r.tuning.as_ref().expect("reported");
    let other_k = if r.chosen.k == 30.0 { 100 } else { 30 };
    assert_eq!(
        t.grid.iter().map(|g| format!("{} {}", g.setting, irelia::js::num_to_string(g.value))).collect::<Vec<_>>(),
        ["pilotStrength 0".to_string(), "pilotStrength 2".into(), format!("itemK {other_k}"), "recencyDays 14".into()]
    );
    assert!(["passed", "failed"].contains(&t.gate.as_str()) && !t.gate_reason.is_empty());
    if t.gate == "passed" {
        assert!(t.full.as_ref().unwrap().low > 0.0);
    }
    assert!(format_eval(&r).contains("\nFine-tuning\n-----------\n"));
    assert!(r.lift.within_pilot.as_ref().map(|w| w.delta.is_finite()).unwrap_or(false));
    let off = run(true, false, |o| {
        o.tuning = None;
        o.memory = None;
        o.scoring = false;
    });
    assert!(off.tuning.is_none());
    assert_eq!(off.lift.delta, r.lift.delta, "the rest of the report as without it");
}

#[test]
fn the_within_pilot_lift_leaves_out_who_follows() {
    // Pilot A always follows the draft pick and wins more; pilot B never does. Across pilots following looks good;
    // within each pilot there is nothing to compare. Pilot C follows half the time, and following does better by 0.2.
    let o = |pilot: &str, followed: bool, wpa: f64| Outcome { pilot: pilot.into(), key: "core1", wpa, followed_draft: followed, followed_baseline: !followed, w: None };
    let mut out = vec![];
    out.extend((0..20).map(|_| o("A", true, 0.3)));
    out.extend((0..20).map(|_| o("B", false, -0.3)));
    out.extend((0..10).map(|_| o("C", true, 0.1)));
    out.extend((0..10).map(|_| o("C", false, -0.1)));
    let w = within_pilot_of(&group_outcomes(&out, None));
    assert!((w.draft - 0.2).abs() < 1e-9, "within C only: {}", w.draft);
    assert!((w.baseline + 0.2).abs() < 1e-9);
}

#[test]
fn the_state_model_report_carries_calibration_overall_and_by_ten_minute_window() {
    let o = EvalOptions { lambdas: vec![1.0], bandwidths: vec![0.7], ks: vec![100.0], bootstrap: 50, live: false, memory: None, ..opts() };
    let r = evaluate(input(&games(true), None, Box::new(|_| {})), &o).unwrap().report;
    let s = r.state.expect("a state model was fitted");
    let c = s.reliability.expect("reliability is reported");
    assert_eq!(c.overall.bins.len(), 20);
    assert_eq!(c.overall.frames, s.frames);
    assert_eq!(c.overall.bins.iter().map(|b| b.count).sum::<f64>(), s.frames);
    let names: Vec<&str> = c.by_window.iter().map(|w| w.window.as_str()).collect();
    assert_eq!(names, ["0-10", "10-20", "20-30", "30+"]);
    assert_eq!(c.by_window.iter().map(|w| w.frames).sum::<f64>(), s.frames, "every frame is in exactly one window");
    for w in std::iter::once(&c.overall).chain(c.by_window.iter()) {
        assert_eq!(w.bins.iter().map(|b| b.count).sum::<f64>(), w.frames);
        // ECE from the table: Σ count/N · |win rate − mean prediction| over the non-empty bins.
        let ece: f64 = w.bins.iter().filter(|b| b.count > 0.0).map(|b| b.count / w.frames * (b.win_rate.unwrap() - b.mean_prediction.unwrap()).abs()).sum();
        assert!((ece - w.ece).abs() < 1e-12, "{}: {ece} vs {}", w.window, w.ece);
        assert!((0.0..=1.0).contains(&w.ece));
        assert!(w.bins.iter().all(|b| (b.count == 0.0) == b.win_rate.is_none()));
    }
    // The existing 10-bin table is as it was.
    assert_eq!(s.calibration.iter().map(|b| b.n).sum::<f64>(), s.frames);
}
