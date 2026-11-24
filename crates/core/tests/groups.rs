//! Port of server/model/groups.test.ts: grouped drafts (0.20.0). The per-group sums are the per-game sums with each
//! game at its group's similarity; the grouping is deterministic and keeps games that know different dimensions apart;
//! an evaluation trial with grouping on reaches the exact evaluation's verdict on synthetic games, and a team of
//! threads reports what one does.

use irelia::engine::exclusivity::build_exclusivity_index;
use irelia::engine::recommend::EngineSettings;
use irelia::model::context::{Contexts, DRAFT_DIMS};
use irelia::model::dataset::PreparedGame;
use irelia::model::draft::similarities;
use irelia::model::estimate::{estimate_decision, DecisionEstimate, EstimateOptions, Obs, ObsRef, Observation, Query, Sims, Z};
use irelia::model::evaluate::{eval_defaults, evaluate, EvalInput, EvalOptions, EvalOutput, EvalReport};
use irelia::model::groups::{assign_draft_groups, auto_groups, draft_groups_of, group_similarities, kmeans};
use irelia::model::profiles::{ChampionProfiles, PROFILE_DEFAULTS};
use irelia::model::run_eval::format_eval;
use irelia::testkit::load_test_static_data;
use irelia::testkit_model::{ap_ad_games, blank_game, model_inputs, rng, ApAd};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const D: usize = DRAFT_DIMS.len();

struct Sample {
    r: Box<dyn FnMut() -> f64>,
    zs: Arc<Contexts>,
    obs: ObsRef,
}

impl Sample {
    fn query(&mut self) -> (Option<i64>, Vec<f64>) {
        let r = &mut self.r;
        let opponent = Some(100 + (r() * 8.0).floor() as i64);
        (opponent, (0..D).map(|_| (r() - 0.5) * 4.0).collect())
    }
}

/// `n` games of one lane with random drafts, options and opponents: their observations, sharing one flat context array.
fn sample(seed: u32, n: usize) -> Sample {
    let mut r = rng(seed);
    let z: Vec<Vec<f64>> = (0..n).map(|_| (0..D).map(|_| (r() - 0.5) * 4.0).collect()).collect();
    let zs = Arc::new(Contexts::from_rows(&z, Some(D)));
    let obs: Vec<Observation> = (0..n)
        .map(|i| {
            let option = format!("opt{}", (r() * 6.0).floor()).into();
            let wpa = r() - 0.5;
            let win = if r() < 0.5 { 1.0 } else { 0.0 };
            let w = if r() < 0.05 { 0.0 } else { 0.2 + r() };
            let lane = if r() < 0.9 { "TOP" } else { "MIDDLE" };
            let opponent = if r() < 0.1 { None } else { Some(100 + (r() * 8.0).floor() as i64) };
            Observation { option, wpa, win, start_win: f64::NAN, w, lane, opponent, pilot: format!("p{}", i % 40).into(), z: Z::Row(zs.clone(), i), row: Some(i), age: None }
        })
        .collect();
    Sample { r: Box::new(r), zs, obs: Obs::new(obs) }
}

/// The same estimates to within rounding (the per-group sums add in another order).
fn close(a: &DecisionEstimate, b: &DecisionEstimate, what: &str) {
    assert_eq!(a.options.len(), b.options.len(), "{what}");
    let near = |x: f64, y: f64, field: &str| assert!((x - y).abs() <= 1e-9 * (1.0 + y.abs()), "{what}: {field} {x} against {y}");
    near(a.n_eff, b.n_eff, "nEff");
    near(a.sigma, b.sigma, "sigma");
    let by: HashMap<&str, _> = b.options.iter().map(|o| (&*o.option, o)).collect();
    for x in &a.options {
        let y = by.get(&*x.option).unwrap_or_else(|| panic!("{what}: {}", x.option));
        for (f, xv, yv) in [
            ("theta", x.theta, y.theta),
            ("low", x.low, y.low),
            ("high", x.high, y.high),
            ("laneTheta", x.lane_theta, y.lane_theta),
            ("contextTheta", x.context_theta, y.context_theta),
            ("nEff", x.n_eff, y.n_eff),
            ("winRate", x.win_rate, y.win_rate),
        ] {
            near(xv, yv, &format!("{}.{f}", x.option));
        }
        assert_eq!(x.matchup_games, y.matchup_games, "{what}: {} matchup games", x.option);
        assert_eq!(x.games, y.games);
    }
}

fn q(opponent: Option<i64>, z: &[f64], sims: Sims) -> Query<'static> {
    Query { lane: "TOP", opponent, z: z.to_vec(), sims: Some(sims), memo: None }
}

const O: EstimateOptions = EstimateOptions { h: 0.7, k: 100.0, min_games: 5.0, min_share: 0.03 };

#[test]
fn per_group_sums_are_the_per_game_sums_with_each_game_at_its_groups_similarity() {
    let mut s = sample(3, 2_000);
    // Random groups and similarities: the sums depend on nothing else.
    let of: Arc<Vec<i32>> = Arc::new((0..s.zs.rows).map(|_| ((s.r)() * 37.0).floor() as i32).collect());
    for i in 0..8 {
        let (opponent, z) = s.query();
        let gs: Arc<Vec<f64>> = Arc::new((0..37).map(|_| if (s.r)() < 0.1 { 0.0 } else { (s.r)() }).collect());
        let values: Arc<Vec<f64>> = Arc::new(of.iter().map(|g| gs[*g as usize]).collect());
        let grouped = Sims { h: O.h, values: values.clone(), groups: Some((of.clone(), gs.clone())) };
        let per_game = Sims { h: O.h, values: values.clone(), groups: None };
        close(&estimate_decision(&s.obs, &q(opponent, &z, grouped.clone()), &O, None, None), &estimate_decision(&s.obs, &q(opponent, &z, per_game.clone()), &O, None, None), &format!("query {i}"));
        // No opponent: no matchup layer.
        close(&estimate_decision(&s.obs, &q(None, &z, grouped), &O, None, None), &estimate_decision(&s.obs, &q(None, &z, per_game), &O, None, None), &format!("query {i} without opponent"));
    }
}

#[test]
fn one_game_per_group_is_the_exact_estimate() {
    let mut s = sample(5, 600);
    let games: Vec<PreparedGame> = (0..s.zs.rows).map(|i| PreparedGame { dg: Some(i), ..blank_game() }).collect();
    let refs: Vec<&PreparedGame> = games.iter().collect();
    let groups = draft_groups_of(&refs, &s.zs).unwrap();
    assert_eq!(groups.centroids.rows, s.zs.rows);
    for i in 0..5 {
        let (opponent, z) = s.query();
        let gs = group_similarities(&groups, &z, O.h);
        let exact = similarities(&s.zs, &z, O.h);
        assert!(gs.iter().zip(&exact).all(|(a, b)| a.to_bits() == b.to_bits()), "each centre is its game");
        let exact = Arc::new(exact);
        let grouped = Sims { h: O.h, values: exact.clone(), groups: Some((groups.of.clone(), Arc::new(gs))) };
        close(
            &estimate_decision(&s.obs, &q(opponent, &z, grouped), &O, None, None),
            &estimate_decision(&s.obs, &q(opponent, &z, Sims { h: O.h, values: exact, groups: None }), &O, None, None),
            &format!("query {i}"),
        );
    }
}

#[test]
fn draft_groups_every_game_gets_one_the_same_each_time_and_only_games_knowing_the_same_dimensions_share_one() {
    let mut r = rng(11);
    let mut games: Vec<PreparedGame> = (0..3_000)
        .map(|i| {
            let mut draft = [0.0; 12];
            for (t, v) in draft.iter_mut().enumerate() {
                *v = if t >= 8 && i % 3 == 0 { f64::NAN } else { (r() - 0.5) * 10.0 + (i % 4) as f64 * 5.0 };
            }
            PreparedGame { draft, ..blank_game() }
        })
        .collect();
    let stats = assign_draft_groups(&mut games, 75, 1.0);
    let first: Vec<Option<usize>> = games.iter().map(|g| g.dg).collect();
    assert!(first.iter().all(|g| g.map(|g| g < stats.groups).unwrap_or(false)));
    assert!(stats.groups >= 50 && stats.groups <= 75, "{} groups for 75 asked", stats.groups);
    assert!(stats.spread > 0.0 && stats.spread < 1.0, "spread {}", stats.spread);
    assign_draft_groups(&mut games, 75, 1.0);
    assert_eq!(games.iter().map(|g| g.dg).collect::<Vec<_>>(), first, "deterministic");
    let mut pattern: HashMap<usize, bool> = HashMap::new();
    for (i, g) in games.iter().enumerate() {
        let unknown = i % 3 == 0;
        let dg = g.dg.unwrap();
        if let Some(seen) = pattern.get(&dg) {
            assert_eq!(*seen, unknown, "group {dg} mixes games that know different dimensions");
        }
        pattern.insert(dg, unknown);
    }
    // A group's centre doesn't know what its games don't.
    let rows: Vec<Vec<f64>> = games.iter().map(|g| g.draft.to_vec()).collect();
    let zs = Contexts::from_rows(&rows, Some(D));
    let refs: Vec<&PreparedGame> = games.iter().collect();
    let groups = draft_groups_of(&refs, &zs).unwrap();
    for i in (0..games.len()).step_by(3) {
        assert!(groups.centroids.flat[groups.of[i] as usize * D + 9].is_nan());
    }
    assert_eq!(auto_groups(1_000), 64);
    assert_eq!(auto_groups(40_000), 1_000);
    assert_eq!(auto_groups(1_000_000), 1_024);
}

#[test]
fn k_means_finds_well_separated_clusters() {
    let mut r = rng(2);
    let (n, d) = (900usize, 3usize);
    let mut x = vec![0.0; n * d];
    for i in 0..n {
        for t in 0..d {
            x[i * d + t] = (i % 3) as f64 * 100.0 + r();
        }
    }
    let a = kmeans(&x, n, d, 3, 1.0);
    for i in 3..n {
        assert_eq!(a[i], a[i % 3], "point {i}");
    }
    assert_eq!(a.iter().collect::<HashSet<_>>().len(), 3);
}

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
    EvalOptions {
        lambdas: vec![1.0],
        bandwidths: vec![0.7, f64::INFINITY],
        ks: vec![100.0],
        bootstrap: 200,
        live: true,
        max_live_games: 60,
        memory: None,
        min_games: 100,
        ..eval_defaults(engine())
    }
}

fn run_with(games: usize, pilots: usize, seed: u32, o: &EvalOptions, threads: usize) -> EvalOutput {
    let data = load_test_static_data();
    let (rows, participants, totals) = model_inputs(&ap_ad_games(games, pilots, seed, ApAd { no_effect: false, random_opponent: true }));
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
            threads,
            base: None,
            salt: None,
            control: None,
            prepared_hook: None,
        },
        o,
    )
    .unwrap()
}

fn run(grouping: Option<usize>, threads: usize) -> EvalReport {
    run_with(800, 40, 21, &EvalOptions { grouping, ..opts() }, threads).report
}

/// The report as JSON without what differs between two runs by design (when it ran, how long grouping took).
fn comparable(r: &EvalReport) -> serde_json::Value {
    let mut v = serde_json::to_value(r).unwrap();
    v.as_object_mut().unwrap().remove("evaluatedAt");
    if let Some(g) = v.get_mut("grouping").and_then(|g| g.as_object_mut()) {
        g.insert("ms".into(), 0.into());
    }
    v
}

#[test]
fn evaluation_trial_grouped_drafts_reach_the_exact_verdict_say_so_and_three_threads_report_what_one_does() {
    let exact = run(None, 1);
    let grouped = run(Some(0), 1);
    assert!(exact.grouping.is_none());
    let g = grouped.grouping.as_ref().unwrap();
    assert!(g.groups > 1.0 && g.games == 800.0);
    assert_eq!(grouped.gate, exact.gate);
    assert_eq!(grouped.gate, "passed");
    let core = |r: &EvalReport| r.decisions.iter().find(|d| d.key == "core1").unwrap().draft;
    assert!((core(&grouped) - core(&exact)).abs() < 0.05, "core1 lift {} against {}", core(&grouped), core(&exact));
    assert!(format_eval(&grouped).contains("Grouped drafts (a trial"));
    assert!(!format_eval(&exact).contains("Grouped drafts"));
    let team = run(Some(0), 3);
    assert_eq!(comparable(&team), comparable(&grouped));
}

#[test]
fn an_evaluation_reports_how_long_each_step_took_the_reading_first() {
    let o = EvalOptions { live: false, bandwidths: vec![0.7], scoring: false, tuning: None, swaps: false, min_games: 50, ..opts() };
    let timings = run_with(300, 30, 4, &o, 1).timings;
    assert_eq!(timings[0].label, "Reading the stored games");
    assert!(timings.len() > 5 && timings.iter().all(|t| t.ms >= 0.0 && !t.label.is_empty()));
    assert!(timings.iter().any(|t| t.label.starts_with("Comparing builds")));
}
