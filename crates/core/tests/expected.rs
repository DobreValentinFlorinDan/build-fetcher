//! Port of server/model/expected.test.ts: scoring pre-game decisions against each game's expected win rate before it
//! started (0.9.0). The fit is a plain ridge logistic regression with the lane opponent as a shrunk effect, no game's
//! result reaches its own expected win rate, and a summoner spell taken mostly by strong pilots stops looking better
//! than it is.
//!
//! Not ported: "frames from shared blocks" (frameSlabs), a V8 memory layout of 0.9.1; 1.x keeps each game's frames in
//! a vector of its own.

use irelia::engine::exclusivity::build_exclusivity_index;
use irelia::engine::recommend::EngineSettings;
use irelia::model::dataset::{prepare_games, score_games, PrepareOptions, PreparedGame, ScoredGame};
use irelia::model::evaluate::{eval_defaults, evaluate, EvalInput, EvalOptions, EvalReport};
use irelia::model::expected::{cross_fitted, expected_defaults, fit_grouped, ExpectedOptions, Penalty};
use irelia::model::logistic::{fit_logistic, FitOptions};
use irelia::model::profiles::{ChampionProfiles, PROFILE_DEFAULTS};
use irelia::model::run_eval::format_eval;
use irelia::testkit::load_test_static_data;
use irelia::testkit_model::{ap_ad_games, model_inputs, pilot_skill_games, rng, ApAd, SynthGame};
use std::collections::HashMap;
use std::sync::Arc;

fn age_of(_: &PreparedGame) -> f64 {
    0.0
}

/// One penalty: with several, a game's result takes part in choosing it (as any cross-validated choice).
fn one() -> ExpectedOptions {
    ExpectedOptions { penalties: vec![Penalty { lambda: 10.0, opponent_lambda: 30.0 }], ..expected_defaults() }
}

fn prepared(games: &[SynthGame]) -> Vec<PreparedGame> {
    let data = load_test_static_data();
    let (rows, participants, totals) = model_inputs(games);
    let rows: Vec<Arc<_>> = rows.into_iter().map(Arc::new).collect();
    let profiles = ChampionProfiles::new(&totals, &[], data.clone(), PROFILE_DEFAULTS);
    prepare_games(&rows, &participants, &data, &profiles, &PrepareOptions { starting_item_window_ms: 60_000.0 })
}

fn values(games: &[PreparedGame], o: &ExpectedOptions) -> Vec<f64> {
    let refs: Vec<&PreparedGame> = games.iter().collect();
    cross_fitted(&refs, &age_of, o).unwrap().0
}

#[test]
fn expected_win_rate_the_grouped_fit_is_a_ridge_logistic_regression_with_one_column_per_group() {
    let mut r = rng(3);
    let (n, d, groups) = (400usize, 4usize, 5usize);
    let mut x = vec![0.0; n * d];
    let mut group = vec![0i32; n];
    let mut y = vec![0.0; n];
    let mut rows: Vec<Vec<f64>> = vec![];
    for i in 0..n {
        let row = [1.0, r() * 2.0 - 1.0, r() * 2.0 - 1.0, if r() < 0.3 { 1.0 } else { 0.0 }];
        x[i * d..(i + 1) * d].copy_from_slice(&row);
        group[i] = if i % 7 == 0 { -1 } else { (r() * groups as f64).floor() as i32 };
        let effect = if group[i] >= 0 { group[i] as f64 * 0.2 - 0.4 } else { 0.0 };
        y[i] = if r() < 1.0 / (1.0 + (-(0.3 + row[1] - 0.5 * row[2] + effect)).exp()) { 1.0 } else { 0.0 };
        rows.push(row.iter().copied().chain((0..groups).map(|k| if group[i] == k as i32 { 1.0 } else { 0.0 })).collect());
    }
    let grouped = fit_grouped(&x, d, &group, groups, &y, &vec![1.0; d], 1.0, None, 30, 1e-8).unwrap();
    let plain = fit_logistic(&rows, &y, &FitOptions { lambda: 1.0, weights: None, max_iterations: 50, tolerance: 1e-12 }).unwrap();
    assert!(grouped.converged);
    for j in 0..d {
        assert!((grouped.beta[j] - plain.beta[j]).abs() < 1e-7, "β{j}: {} vs {}", grouped.beta[j], plain.beta[j]);
    }
    for k in 0..groups {
        assert!((grouped.u[k] - plain.beta[d + k]).abs() < 1e-7, "group {k}: {} vs {}", grouped.u[k], plain.beta[d + k]);
    }
}

#[test]
fn expected_win_rate_cross_fitted_so_a_games_own_result_never_moves_it_the_users_own_games_get_none() {
    let games = prepared(&pilot_skill_games(600, 30, 5));
    let before = values(&games, &expected_defaults());
    assert!(before.iter().all(|p| *p > 0.0 && *p < 1.0), "every game has one");
    // Flipping a game's result changes the models of the other folds, never its own fold's.
    let mut flipped = games.clone();
    flipped[0].win = 1.0 - flipped[0].win;
    assert_eq!(values(&flipped, &one())[0].to_bits(), values(&games, &one())[0].to_bits());
    let mut with_mine = games.clone();
    with_mine[1].source = "me".into();
    assert!(values(&with_mine, &expected_defaults())[1].is_nan());
    // Too few games: none (scored against the overall win rate, as before 0.9.0).
    assert!(values(&games[..100], &expected_defaults()).iter().all(|v| v.is_nan()));
}

#[test]
fn expected_win_rate_a_summoner_spell_taken_mostly_by_strong_pilots_stops_looking_better_than_it_is() {
    let games = prepared(&pilot_skill_games(1600, 40, 5));
    let mean_by = |scored: &[ScoredGame]| {
        let mut sums: HashMap<String, (f64, f64)> = HashMap::new();
        for x in scored {
            let e = sums.entry(x.g.summoners.clone()).or_insert((0.0, 0.0));
            e.0 += x.wpa0;
            e.1 += 1.0;
        }
        let of = |k: &str| sums[k].0 / sums[k].1;
        of("4,14") - of("4,12") // Flash + Ignite minus Flash + Teleport
    };
    let expected = values(&games, &expected_defaults());
    let arcs: Vec<Arc<PreparedGame>> = games.into_iter().map(Arc::new).collect();
    let overall = mean_by(&score_games(&arcs, None, None));
    let fair = mean_by(&score_games(&arcs, None, Some(&expected)));
    assert!(overall > 0.02, "against the overall win rate Ignite looks {:.1} pts better", overall * 100.0);
    assert!(fair < -0.03 && fair > -0.14, "against the expected win rate it costs {:.1} pts (truly 8)", fair * 100.0);
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

fn run(games: &[SynthGame]) -> EvalReport {
    let data = load_test_static_data();
    let (rows, participants, totals) = model_inputs(games);
    let o = EvalOptions { lambdas: vec![1.0], bandwidths: vec![0.7], ks: vec![100.0], bootstrap: 300, live: false, memory: None, ..eval_defaults(engine()) };
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
}

#[test]
fn eval_scoring_against_the_draft_passes_when_pilot_strength_hides_what_a_summoner_spell_costs() {
    let r = run(&pilot_skill_games(1600, 40, 5));
    let c = r.scoring.as_ref().unwrap();
    let e = c.expected.as_ref().expect("an expected win rate");
    assert!(e.log_loss < e.base_log_loss, "the expected win rate predicts held-out results");
    let summoners = c.decisions.iter().find(|d| d.key == "summoners").unwrap();
    assert!(summoners.changed > 0.5, "the summoner pick changes in most games ({})", summoners.changed);
    assert!(summoners.fair > summoners.classic, "{} > {}", summoners.fair, summoners.classic);
    assert!(c.adjusted.low > 0.0, "CI {}..{}", c.adjusted.low, c.adjusted.high);
    assert!(c.brier.fair <= c.brier.classic);
    assert_eq!(c.gate, "passed", "{}", c.gate_reason);
    assert!(format_eval(&r).contains("Runes and summoners against the draft: PASSED"));
}

#[test]
fn eval_scoring_against_the_draft_stays_off_when_the_draft_and_the_pilot_dont_predict_who_wins() {
    let r = run(&ap_ad_games(800, 40, 21, ApAd { no_effect: false, random_opponent: true }));
    let c = r.scoring.as_ref().unwrap();
    assert_eq!(c.gate, "failed");
    assert!(c.gate_reason.contains("no better than the overall win rate"), "{}", c.gate_reason);
}
