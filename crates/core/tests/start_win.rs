//! W̄: the weighted mean win chance just before the purchase, beside each option's score. It never moves a score.

use irelia::model::context::kernel;
use irelia::model::estimate::{estimate_decision, EstimateOptions, Obs, Observation, OptionEstimate, Query, Z};
use std::sync::Arc;

fn ob(option: &str, wpa: f64, win: f64, start_win: f64, w: f64, z: Vec<f64>) -> Observation {
    Observation { option: option.into(), wpa, win, start_win, w, lane: "TOP", opponent: None, pilot: "p".into(), z: Z::Own(Arc::new(z)), row: None, age: None }
}

fn query(z: Vec<f64>) -> Query<'static> {
    Query { lane: "TOP", opponent: None, z, sims: None, memo: None }
}

fn find<'a>(o: &'a [OptionEstimate], name: &str) -> &'a OptionEstimate {
    o.iter().find(|x| &*x.option == name).unwrap()
}

#[test]
fn without_context_it_is_the_plain_weighted_mean_of_the_starting_chance() {
    let o = EstimateOptions { h: f64::INFINITY, k: 10.0, min_games: 1.0, min_share: 0.0 };
    // weights 1, 1, 2 with chances .4, .6, .8: (.4 + .6 + 1.6) / 4 = .65. A second option with a different mean.
    let list = vec![
        ob("a", 0.1, 1.0, 0.4, 1.0, vec![]),
        ob("a", 0.0, 0.0, 0.6, 1.0, vec![]),
        ob("a", -0.1, 1.0, 0.8, 2.0, vec![]),
        ob("b", 0.0, 1.0, 0.3, 1.0, vec![]),
    ];
    let est = estimate_decision(&Obs::new(list), &query(vec![]), &o, None, None);
    let a = find(&est.options, "a");
    assert!((a.start_win.unwrap() - 0.65).abs() < 1e-12);
    assert_eq!(a.games, 3.0);
    assert!((find(&est.options, "b").start_win.unwrap() - 0.3).abs() < 1e-12);
}

#[test]
fn with_context_it_uses_the_same_similarity_weights_as_the_win_rate() {
    let o = EstimateOptions { h: 0.7, k: 10.0, min_games: 1.0, min_share: 0.0 };
    let zs = [vec![0.0], vec![0.5], vec![2.0]];
    let (ws, starts, wins) = ([1.0, 2.0, 1.5], [0.3, 0.5, 0.9], [1.0, 0.0, 1.0]);
    let list: Vec<Observation> = (0..3).map(|i| ob("a", 0.0, wins[i], starts[i], ws[i], zs[i].clone())).collect();
    let q = vec![0.2];
    let est = estimate_decision(&Obs::new(list), &query(q.clone()), &o, None, None);
    let a = find(&est.options, "a");
    let (mut sw, mut sw0, mut swin) = (0.0, 0.0, 0.0);
    for i in 0..3 {
        let wk = ws[i] * kernel(&zs[i], &q, 0.7, None);
        sw += wk;
        sw0 += wk * starts[i];
        swin += wk * wins[i];
    }
    assert!((a.start_win.unwrap() - sw0 / sw).abs() < 1e-12);
    assert!((a.win_rate - swin / sw).abs() < 1e-12);
}

#[test]
fn unknown_starting_chances_give_none_and_no_starting_chance_moves_any_score() {
    let o = EstimateOptions { h: 0.7, k: 10.0, min_games: 1.0, min_share: 0.0 };
    let make = |start: &dyn Fn(usize) -> f64| -> Vec<Observation> {
        (0..30).map(|i| ob(if i % 3 == 0 { "a" } else { "b" }, (i as f64 % 7.0 - 3.0) / 20.0, (i % 2) as f64, start(i), 1.0 + (i % 4) as f64 / 4.0, vec![(i % 5) as f64 / 2.0])).collect()
    };
    let q = vec![0.5];
    let unknown = estimate_decision(&Obs::new(make(&|_| f64::NAN)), &query(q.clone()), &o, None, None);
    let known = estimate_decision(&Obs::new(make(&|i| 0.2 + (i % 6) as f64 / 10.0)), &query(q), &o, None, None);
    assert!(unknown.options.iter().all(|x| x.start_win.is_none()));
    assert!(known.options.iter().all(|x| x.start_win.is_some()));
    assert_eq!(unknown.options.len(), known.options.len());
    for (u, k) in unknown.options.iter().zip(&known.options) {
        for (x, y) in [(u.theta, k.theta), (u.low, k.low), (u.high, k.high), (u.lane_theta, k.lane_theta), (u.context_theta, k.context_theta), (u.n_eff, k.n_eff), (u.win_rate, k.win_rate)] {
            assert_eq!(x.to_bits(), y.to_bits());
        }
    }
}
