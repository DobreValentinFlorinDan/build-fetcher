//! Port of server/model/estimate.test.ts: estimate_decision indexes the observations once and loops over flat arrays.
//! It must give the same numbers as the plain version below (the one it replaced) on random decisions, to the last
//! bit: by its general loop, without context (from the index), with the similarities of a draft (option by option)
//! and with sums kept across priors. Also: similarities() are exactly kernel().
//!
//! Not ported: the weighted-sums accelerator case. 0.22.0 had the sums on the GPU only behind IRELIA_GPU_SUMS=1 (off:
//! slower than the processor, by its own measurement); 1.x accelerates the similarities only.

use indexmap::IndexMap;
use irelia::model::context::{kernel, Contexts};
use irelia::model::draft::similarities;
use irelia::model::estimate::{estimate_decision, DecisionEstimate, EstimateOptions, Memo, Obs, ObsRef, Observation, OptionEstimate, Query, Sims, Z};
use irelia::testkit_model::rng;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Default, Clone, Copy)]
struct Acc {
    sw: f64,
    sw2: f64,
    swx: f64,
    swin: f64,
    n: f64,
}

impl Acc {
    fn add(&mut self, w: f64, x: f64, win: f64, k: f64) {
        if w <= 0.0 || k <= 0.0 {
            return;
        }
        let wk = w * k;
        self.sw += wk;
        self.sw2 += w * wk;
        self.swx += wk * x;
        self.swin += wk * win;
        self.n += 1.0;
    }
    fn mean(&self) -> f64 {
        if self.sw > 0.0 {
            self.swx / self.sw
        } else {
            0.0
        }
    }
    fn n_eff(&self) -> f64 {
        if self.sw2 > 0.0 {
            (self.sw * self.sw) / self.sw2
        } else {
            0.0
        }
    }
}

fn z_of(o: &Observation) -> &[f64] {
    match &o.z {
        Z::Own(v) => v,
        _ => unreachable!(),
    }
}

/// The plain version: one pass over every observation, sums in maps.
fn reference(obs: &[Observation], lane_q: &str, opponent: Option<i64>, zq: &[f64], o: &EstimateOptions, only: Option<&HashSet<&str>>, dim: Option<usize>) -> DecisionEstimate {
    let towards = |a: &Acc, prior: f64, k: f64| (a.n_eff() * a.mean() + k * prior) / (a.n_eff() + k);
    let has_context = o.h.is_finite()
        && match dim {
            Some(d) => !zq[d].is_nan(),
            None => zq.iter().any(|v| !v.is_nan()),
        };
    let mut all: IndexMap<Arc<str>, Acc> = IndexMap::new();
    let mut lane: IndexMap<Arc<str>, Acc> = IndexMap::new();
    let mut ctx: IndexMap<Arc<str>, Acc> = IndexMap::new();
    let mut mu: IndexMap<Arc<str>, Acc> = IndexMap::new();
    let mut pilots: HashMap<Arc<str>, HashSet<Arc<str>>> = HashMap::new();
    let mut total = Acc::default();
    let (mut lane_weight, mut sq, mut sq_w) = (0.0, 0.0, 0.0);
    for ob in obs {
        if let Some(set) = only {
            if !set.contains(&*ob.option) {
                continue;
            }
        }
        all.entry(ob.option.clone()).or_default().add(ob.w, ob.wpa, ob.win, 1.0);
        if ob.lane != lane_q {
            continue;
        }
        lane.entry(ob.option.clone()).or_default().add(ob.w, ob.wpa, ob.win, 1.0);
        lane_weight += ob.w;
        sq += ob.w * ob.wpa * ob.wpa;
        sq_w += ob.w;
        let sim = if has_context { kernel(z_of(ob), zq, o.h, dim) } else { 1.0 };
        ctx.entry(ob.option.clone()).or_default().add(ob.w, ob.wpa, ob.win, sim);
        total.add(ob.w, 0.0, 0.0, sim);
        if opponent.is_some() && ob.opponent == opponent {
            mu.entry(ob.option.clone()).or_default().add(ob.w, ob.wpa, ob.win, sim);
        }
        pilots.entry(ob.option.clone()).or_default().insert(ob.pilot.clone());
    }
    let sigma = f64::max(0.3, if sq_w > 0.0 { (sq / sq_w).sqrt() } else { 0.5 });
    let mut options: Vec<OptionEstimate> = vec![];
    for (option, l) in &lane {
        if l.n < o.min_games || l.sw < o.min_share * lane_weight {
            continue;
        }
        let theta_all = towards(&all[option], 0.0, o.k);
        let lane_theta = towards(l, theta_all, o.k);
        let c = ctx[option];
        let context_theta = if has_context { towards(&c, lane_theta, o.k) } else { lane_theta };
        let m = mu.get(option);
        let theta = match m {
            Some(m) => towards(m, context_theta, o.k),
            None => context_theta,
        };
        let n = f64::max(if has_context { c.n_eff() } else { l.n_eff() }, m.map(|m| m.n_eff()).unwrap_or(0.0));
        let half = 1.96 * sigma / (n + o.k).sqrt();
        let wr = if has_context { c } else { *l };
        options.push(OptionEstimate {
            option: option.clone(),
            theta,
            low: theta - half,
            high: theta + half,
            lane_theta,
            context_theta,
            n_eff: if has_context { c.n_eff() } else { l.n_eff() },
            matchup_games: m.map(|m| m.n).unwrap_or(0.0),
            games: l.n,
            pilots: pilots.get(option).map(|p| p.len()).unwrap_or(0) as f64,
            win_rate: if wr.sw > 0.0 { wr.swin / wr.sw } else { 0.0 },
            start_win: None,
        });
    }
    // Array.prototype.sort: stable; NaN comparisons count as 0.
    options.sort_by(|a, b| {
        let d = b.theta - a.theta;
        let d = if d != 0.0 && !d.is_nan() { d } else { b.games - a.games };
        d.partial_cmp(&0.0).unwrap_or(std::cmp::Ordering::Equal)
    });
    let n_eff = if has_context {
        total.n_eff()
    } else {
        let sw: f64 = lane.values().fold(0.0, |s, a| s + a.sw);
        let sw2: f64 = lane.values().fold(0.0, |s, a| s + a.sw2);
        sw.powi(2) / f64::max(1e-9, sw2)
    };
    DecisionEstimate { options, n_eff, sigma }
}

fn bits_eq(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
}

fn same(got: &DecisionEstimate, want: &DecisionEstimate, what: &str) {
    assert!(bits_eq(got.n_eff, want.n_eff) && bits_eq(got.sigma, want.sigma), "{what}: n_eff {} vs {}, sigma {} vs {}", got.n_eff, want.n_eff, got.sigma, want.sigma);
    assert_eq!(got.options.len(), want.options.len(), "{what}: options");
    for (g, w) in got.options.iter().zip(&want.options) {
        let fields = [
            (g.theta, w.theta),
            (g.low, w.low),
            (g.high, w.high),
            (g.lane_theta, w.lane_theta),
            (g.context_theta, w.context_theta),
            (g.n_eff, w.n_eff),
            (g.matchup_games, w.matchup_games),
            (g.games, w.games),
            (g.pilots, w.pilots),
            (g.win_rate, w.win_rate),
        ];
        assert!(g.option == w.option && fields.iter().all(|(a, b)| bits_eq(*a, *b)), "{what}:\n got {g:?}\nwant {w:?}");
    }
}

const DRAFT: usize = 6;
const LIVE: usize = 3;

struct Sample {
    rows: Vec<(Vec<f64>, Vec<f64>)>,
    pregame: Vec<Observation>,
    items: Vec<Observation>,
    r: Box<dyn FnMut() -> f64>,
}

struct Q {
    lane: &'static str,
    opponent: Option<i64>,
    z: Vec<f64>,
}

impl Sample {
    fn query(&mut self) -> Q {
        let r = &mut self.r;
        let lane = if r() < 0.5 { "TOP" } else { "MIDDLE" };
        let opponent = if r() < 0.7 { Some(1 + (r() * 6.0).floor() as i64) } else { None };
        let z = (0..DRAFT).map(|_| if r() < 0.08 { f64::NAN } else { (r() - 0.5) * 4.0 }).collect();
        Q { lane, opponent, z }
    }
}

/// Random games: draft context (some unknown), per-game live context, options, weights (some zero), opponents.
fn sample(seed: u32, games: usize) -> Sample {
    let mut r = rng(seed);
    let z = |r: &mut dyn FnMut() -> f64, n: usize| -> Vec<f64> { (0..n).map(|_| if r() < 0.08 { f64::NAN } else { (r() - 0.5) * 4.0 }).collect() };
    let rows: Vec<(Vec<f64>, Vec<f64>)> = (0..games).map(|_| (z(&mut r, DRAFT), z(&mut r, LIVE))).collect();
    let obs = |r: &mut dyn FnMut() -> f64, with_live: bool| -> Vec<Observation> {
        rows.iter()
            .enumerate()
            .map(|(i, (draft, live))| {
                let option: Arc<str> = (3000 + (r().powi(2) * 9.0).floor() as i64).to_string().into();
                let wpa = (r() - 0.5) * 0.8;
                let win = if r() < 0.5 { 1.0 } else { 0.0 };
                let w = if r() < 0.05 { 0.0 } else { 0.2 + r() };
                let lane = if r() < 0.6 { "TOP" } else { "MIDDLE" };
                let opponent = if r() < 0.7 { Some(1 + (r() * 6.0).floor() as i64) } else { None };
                let pilot: Arc<str> = format!("p{}", (r() * games as f64 / 4.0).floor()).into();
                let zz: Vec<f64> = if with_live { draft.iter().chain(live.iter()).copied().collect() } else { draft.clone() };
                Observation { option, wpa, win, w, lane, opponent, pilot, z: Z::Own(Arc::new(zz)), row: Some(i), age: None, start_win: f64::NAN }
            })
            .collect()
    };
    let pregame = obs(&mut r, false);
    let items = obs(&mut r, true);
    Sample { rows, pregame, items, r: Box::new(r) }
}

fn opts(h: f64, k: f64, min_games: f64, min_share: f64) -> EstimateOptions {
    EstimateOptions { h, k, min_games, min_share }
}

fn query<'a>(q: &Q, z: Vec<f64>, sims: Option<Sims>, memo: Option<&'a RefCell<Memo>>) -> Query<'a> {
    Query { lane: q.lane, opponent: q.opponent, z, sims, memo }
}

fn sims_of(rows: &[(Vec<f64>, Vec<f64>)], zq: &[f64], h: f64) -> Sims {
    Sims { h, values: Arc::new(rows.iter().map(|g| kernel(&g.0, zq, h, None)).collect()), groups: None }
}

fn with_live(z: &[f64]) -> Vec<f64> {
    z.iter().copied().chain(std::iter::repeat(f64::NAN).take(LIVE)).collect()
}

#[test]
fn estimate_decision_gives_exactly_what_the_plain_version_gives() {
    let settings = [opts(0.7, 100.0, 5.0, 0.03), opts(0.3, 30.0, 1.0, 0.0), opts(f64::INFINITY, 300.0, 5.0, 0.03)];
    for seed in [1, 2, 3] {
        let mut s = sample(seed, 600);
        let pregame: ObsRef = Obs::new(s.pregame.clone());
        for o in &settings {
            for i in 0..8 {
                let q = s.query();
                let what = format!("seed {seed}, h {}, query {i}", o.h);
                same(&estimate_decision(&pregame, &query(&q, q.z.clone(), None, None), o, None, None), &reference(&s.pregame, q.lane, q.opponent, &q.z, o, None, None), &what);
                let pair: HashSet<&str> = ["3000", "3002"].into_iter().collect();
                for dim in [None, Some(2)] {
                    same(
                        &estimate_decision(&pregame, &query(&q, q.z.clone(), None, None), o, Some(&["3000", "3002"]), dim),
                        &reference(&s.pregame, q.lane, q.opponent, &q.z, o, Some(&pair), dim),
                        &format!("{what}: only, dim {dim:?}"),
                    );
                }
            }
        }
    }
}

#[test]
fn similarities_computed_once_per_draft_give_the_same_estimates_for_pre_game_and_item_decisions() {
    let mut s = sample(7, 500);
    let pregame = Obs::new(s.pregame.clone());
    let items = Obs::new(s.items.clone());
    let o = opts(0.7, 100.0, 5.0, 0.03);
    for _ in 0..10 {
        let q = s.query();
        // As recommend_draft does: each game's similarity on the draft dimensions; item queries leave the live ones unknown.
        let sims = sims_of(&s.rows, &q.z, o.h);
        let iz = with_live(&q.z);
        same(&estimate_decision(&pregame, &query(&q, q.z.clone(), Some(sims.clone()), None), &o, None, None), &reference(&s.pregame, q.lane, q.opponent, &q.z, &o, None, None), "pregame");
        same(&estimate_decision(&items, &query(&q, iz.clone(), Some(sims.clone()), None), &o, None, None), &reference(&s.items, q.lane, q.opponent, &iz, &o, None, None), "items");
        // Another bandwidth, or the one-dimension reason search, ignores them.
        let other = EstimateOptions { h: 1.5, ..o };
        same(&estimate_decision(&pregame, &query(&q, q.z.clone(), Some(sims.clone()), None), &other, None, None), &reference(&s.pregame, q.lane, q.opponent, &q.z, &other, None, None), "other h");
        same(&estimate_decision(&pregame, &query(&q, q.z.clone(), Some(sims), None), &o, None, Some(1)), &reference(&s.pregame, q.lane, q.opponent, &q.z, &o, None, Some(1)), "dim 1");
    }
}

#[test]
fn a_grown_observation_array_is_indexed_again() {
    let mut s = sample(11, 300);
    let o = opts(0.7, 100.0, 5.0, 0.03);
    let q = s.query();
    let first = &s.pregame[..200];
    same(&estimate_decision(&Obs::new(first.to_vec()), &query(&q, q.z.clone(), None, None), &o, None, None), &reference(first, q.lane, q.opponent, &q.z, &o, None, None), "200");
    same(&estimate_decision(&Obs::new(s.pregame.clone()), &query(&q, q.z.clone(), None, None), &o, None, None), &reference(&s.pregame, q.lane, q.opponent, &q.z, &o, None, None), "300");
}

#[test]
fn sums_kept_for_a_draft_give_each_prior_exactly_what_a_fresh_estimate_gives() {
    let mut s = sample(13, 500);
    let pregame = Obs::new(s.pregame.clone());
    let items = Obs::new(s.items.clone());
    for h in [0.5, 1.0, f64::INFINITY] {
        for _ in 0..6 {
            let q = s.query();
            let sims = if h.is_finite() { Some(sims_of(&s.rows, &q.z, h)) } else { None };
            let memo = RefCell::new(Memo::default());
            for k in [30.0, 100.0, 300.0] {
                let o = opts(h, k, 5.0, 0.03);
                same(&estimate_decision(&pregame, &query(&q, q.z.clone(), sims.clone(), Some(&memo)), &o, None, None), &reference(&s.pregame, q.lane, q.opponent, &q.z, &o, None, None), &format!("h {h}, k {k}"));
                let iz = with_live(&q.z);
                same(&estimate_decision(&items, &query(&q, iz.clone(), sims.clone(), Some(&memo)), &o, None, None), &reference(&s.items, q.lane, q.opponent, &iz, &o, None, None), &format!("items h {h}, k {k}"));
            }
            assert_eq!(memo.borrow().sums.len(), 2, "one entry per set of observations");
        }
    }
}

#[test]
fn similarities_are_exactly_kernel_with_and_without_games_missing_a_dimension() {
    let mut s = sample(17, 400);
    let with_gaps: Vec<Vec<f64>> = s.rows.iter().map(|g| g.0.clone()).collect();
    let complete: Vec<Vec<f64>> = with_gaps.iter().map(|z| z.iter().map(|v| if v.is_nan() { 0.25 } else { *v }).collect()).collect();
    for z_draft in [&with_gaps, &complete] {
        let contexts = Contexts::from_rows(z_draft, None);
        for h in [0.5, 1.0] {
            for _ in 0..5 {
                let zq = s.query().z;
                let got = similarities(&contexts, &zq, h);
                let want: Vec<f64> = z_draft.iter().map(|z| kernel(z, &zq, h, None)).collect();
                assert!(got.iter().zip(&want).all(|(a, b)| bits_eq(*a, *b)), "h {h}");
            }
        }
    }
}
