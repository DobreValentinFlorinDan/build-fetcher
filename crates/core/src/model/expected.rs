//! Port of `server/model/expected.ts`: each game's chance to win before it started (0.9.0), a ridge logistic
//! regression with the lane opponent as a random effect, cross-fitted by pilot.

use super::context::{scale_of, standardise, Scale, N_DRAFT};
use super::dataset::PreparedGame;
use super::logistic::{js_max, js_min, solve_spd};
use crate::js;
use crate::jsmath;
use anyhow::Result;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Penalty {
    pub lambda: f64,
    pub opponent_lambda: f64,
}

#[derive(Clone, Debug)]
pub struct ExpectedOptions {
    pub penalties: Vec<Penalty>,
    pub choice_lambda: f64,
    pub folds: usize,
    pub min_games: usize,
    pub min_choice_games: usize,
}

pub fn expected_defaults() -> ExpectedOptions {
    ExpectedOptions {
        penalties: vec![
            Penalty { lambda: 1.0, opponent_lambda: 10.0 },
            Penalty { lambda: 10.0, opponent_lambda: 30.0 },
            Penalty { lambda: 100.0, opponent_lambda: 100.0 },
            Penalty { lambda: 1000.0, opponent_lambda: 1000.0 },
        ],
        choice_lambda: 1.0,
        folds: 5,
        min_games: 200,
        min_choice_games: 30,
    }
}

fn choice_of(k: usize, g: &PreparedGame) -> Option<String> {
    match k {
        0 => Some(g.perks[0].to_string()),
        1 => Some(g.sub.to_string()),
        2 => Some(g.summoners.clone()),
        3 => (!g.start.is_empty()).then(|| g.start.clone()),
        _ => g.max_first.clone(),
    }
}
const N_CHOICES: usize = 5;

/// A game's patch age: 0 on its region's current patch.
pub type AgeOf<'a> = &'a (dyn Fn(&PreparedGame) -> f64 + Sync);

#[derive(Clone, Debug)]
pub struct Spec {
    pub scale: Scale,
    pub regions: Vec<String>,
    pub max_age: usize,
    pub choices: Vec<Vec<String>>,
}

#[derive(Clone, Debug)]
pub struct ExpectedModel {
    pub spec: Spec,
    pub beta: Vec<f64>,
    pub choice_means: Vec<f64>,
    pub opponents: HashMap<i64, f64>,
    pub games: usize,
    pub penalty: Penalty,
}

const MAX_AGE: f64 = 5.0;

fn is_apex(t: &str) -> bool {
    matches!(t, "CHALLENGER" | "GRANDMASTER" | "MASTER")
}

fn finite_age(a: f64) -> f64 {
    if a.is_finite() && a >= 0.0 {
        a
    } else {
        MAX_AGE
    }
}

fn spec_of(games: &[&PreparedGame], age_of: AgeOf, o: &ExpectedOptions) -> Spec {
    let choices: Vec<Vec<String>> = (0..N_CHOICES)
        .map(|c| {
            let mut n: indexmap::IndexMap<String, usize> = indexmap::IndexMap::new();
            for g in games {
                if let Some(v) = choice_of(c, g) {
                    *n.entry(v).or_insert(0) += 1;
                }
            }
            let mut v: Vec<String> = n.into_iter().filter(|(_, k)| *k >= o.min_choice_games).map(|(v, _)| v).collect();
            v.sort_by(|a, b| js::cmp_str(a, b));
            v
        })
        .collect();
    let mut counts: indexmap::IndexMap<String, usize> = indexmap::IndexMap::new();
    for g in games {
        *counts.entry(g.platform.clone()).or_insert(0) += 1;
    }
    let mut regions: Vec<(String, usize)> = counts.into_iter().collect();
    regions.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut max_age = 0.0f64;
    for g in games {
        max_age = js_max(max_age, js_min(MAX_AGE, finite_age(age_of(g))));
    }
    Spec {
        scale: scale_of(games.iter().map(|g| &g.draft[..]), N_DRAFT),
        regions: regions.into_iter().map(|r| r.0).collect(),
        max_age: max_age as usize,
        choices,
    }
}

fn choice_width(spec: &Spec) -> usize {
    spec.choices.iter().map(|c| c.len()).sum()
}
fn width_of(spec: &Spec) -> usize {
    1 + N_DRAFT + 9 + spec.max_age + spec.regions.len().saturating_sub(1) + choice_width(spec)
}

fn features_into(out: &mut [f64], g: &PreparedGame, spec: &Spec, age_of: AgeOf, choice_means: Option<&[f64]>) {
    let mut i = 0;
    out[i] = 1.0;
    i += 1;
    for z in standardise(&g.draft, &spec.scale) {
        out[i] = if z.is_finite() { z } else { 0.0 };
        i += 1;
    }
    out[i] = if g.opponent.is_none() { 1.0 } else { 0.0 };
    i += 1;
    out[i] = if g.lane == "MIDDLE" { 1.0 } else { 0.0 };
    i += 1;
    let apex = g.tier.as_deref().map(is_apex).unwrap_or(false);
    out[i] = if apex { 1.0 } else { 0.0 };
    i += 1;
    out[i] = match (apex, g.lp) {
        (true, Some(lp)) => jsmath::log1p(js_max(0.0, lp)) / 7.0,
        _ => 0.0,
    };
    i += 1;
    out[i] = if g.tier.is_none() { 1.0 } else { 0.0 };
    i += 1;
    out[i] = match g.mastery {
        Some(m) => jsmath::log1p(js_max(0.0, m)) / 14.0,
        None => 0.0,
    };
    i += 1;
    out[i] = if g.mastery.is_none() { 1.0 } else { 0.0 };
    i += 1;
    out[i] = if g.source == "main" { 1.0 } else { 0.0 };
    i += 1;
    out[i] = if g.source == "harvested" { 1.0 } else { 0.0 };
    i += 1;
    let age = js_min(spec.max_age as f64, finite_age(age_of(g)));
    for a in 1..=spec.max_age {
        out[i] = if age == a as f64 { 1.0 } else { 0.0 };
        i += 1;
    }
    for r in 1..spec.regions.len() {
        out[i] = if g.platform == spec.regions[r] { 1.0 } else { 0.0 };
        i += 1;
    }
    let mut c = 0;
    for k in 0..N_CHOICES {
        let v = if choice_means.is_some() { None } else { choice_of(k, g) };
        for option in &spec.choices[k] {
            out[i] = match choice_means {
                Some(m) => m[c],
                None => {
                    if v.as_deref() == Some(option.as_str()) {
                        1.0
                    } else {
                        0.0
                    }
                }
            };
            i += 1;
            c += 1;
        }
    }
}

struct Design {
    x: Vec<f64>,
    d: usize,
    group: Vec<i32>,
    opponent_ids: Vec<i64>,
    y: Vec<f64>,
}

fn design(games: &[&PreparedGame], spec: &Spec, age_of: AgeOf) -> Design {
    let d = width_of(spec);
    let mut x = vec![0.0; games.len() * d];
    let mut opponent_ids = Vec::new();
    let mut index: HashMap<i64, i32> = HashMap::new();
    let mut group = vec![0i32; games.len()];
    let mut y = vec![0.0; games.len()];
    for (r, g) in games.iter().enumerate() {
        features_into(&mut x[r * d..(r + 1) * d], g, spec, age_of, None);
        group[r] = match g.opponent {
            None => -1,
            Some(o) => *index.entry(o).or_insert_with(|| {
                opponent_ids.push(o);
                opponent_ids.len() as i32 - 1
            }),
        };
        y[r] = g.win;
    }
    Design { x, d, group, opponent_ids, y }
}

fn sigmoid(z: f64) -> f64 {
    super::logistic::sigmoid(z)
}

pub struct GroupedFit {
    pub beta: Vec<f64>,
    pub u: Vec<f64>,
    pub iterations: usize,
    pub converged: bool,
}

/// Ridge logistic regression with one random effect per row's group, by Newton's method (Schur complement).
#[allow(clippy::too_many_arguments)]
pub fn fit_grouped(x: &[f64], d: usize, group: &[i32], groups: usize, y: &[f64], lambda: &[f64], group_lambda: f64, use_: Option<&[u8]>, max_iterations: usize, tolerance: f64) -> Result<GroupedFit> {
    let n = y.len();
    let mut beta = vec![0.0; d];
    let mut u = vec![0.0; groups];
    let (mut iterations, mut converged) = (0, false);
    for it in 1..=max_iterations {
        iterations = it;
        let mut a = vec![0.0; d * d];
        let mut gb = vec![0.0; d];
        let mut b = vec![0.0; d * groups];
        let mut dd = vec![0.0; groups];
        let mut gu = vec![0.0; groups];
        for r in 0..n {
            if let Some(us) = use_ {
                if us[r] == 0 {
                    continue;
                }
            }
            let off = r * d;
            let k = group[r];
            let mut eta = if k >= 0 { u[k as usize] } else { 0.0 };
            for i in 0..d {
                eta += x[off + i] * beta[i];
            }
            let p = sigmoid(eta);
            let w = p * (1.0 - p);
            let res = y[r] - p;
            for i in 0..d {
                let xi = x[off + i];
                if xi == 0.0 {
                    continue;
                }
                gb[i] += res * xi;
                let wxi = w * xi;
                for j in 0..=i {
                    a[i * d + j] += wxi * x[off + j];
                }
                if k >= 0 {
                    b[i * groups + k as usize] += wxi;
                }
            }
            if k >= 0 {
                dd[k as usize] += w;
                gu[k as usize] += res;
            }
        }
        for i in 0..d {
            for j in 0..i {
                a[j * d + i] = a[i * d + j];
            }
        }
        for i in 1..d {
            let l = lambda[i];
            a[i * d + i] += l;
            gb[i] -= l * beta[i];
        }
        a[0] += 1e-9;
        for k in 0..groups {
            dd[k] += group_lambda;
            gu[k] -= group_lambda * u[k];
        }
        for k in 0..groups {
            let inv = 1.0 / dd[k];
            for i in 0..d {
                let bik = b[i * groups + k];
                if bik == 0.0 {
                    continue;
                }
                gb[i] -= bik * inv * gu[k];
                for j in 0..d {
                    a[i * d + j] -= bik * inv * b[j * groups + k];
                }
            }
        }
        let db = solve_spd(&a, &gb, d)?;
        let mut biggest = 0.0f64;
        for i in 0..d {
            beta[i] += db[i];
            biggest = js_max(biggest, db[i].abs());
        }
        for k in 0..groups {
            let mut s = gu[k];
            for i in 0..d {
                s -= b[i * groups + k] * db[i];
            }
            let du = s / dd[k];
            u[k] += du;
            biggest = js_max(biggest, du.abs());
        }
        if biggest < tolerance {
            converged = true;
            break;
        }
    }
    Ok(GroupedFit { beta, u, iterations, converged })
}

fn fit_rows(rows: &Design, spec: &Spec, o: &ExpectedOptions, penalty: Penalty, use_: Option<&[u8]>, games: usize) -> Result<ExpectedModel> {
    let width = choice_width(spec);
    let from = rows.d - width;
    let penalties: Vec<f64> = (0..rows.d).map(|i| if i >= from { o.choice_lambda } else { penalty.lambda }).collect();
    let f = fit_grouped(&rows.x, rows.d, &rows.group, rows.opponent_ids.len(), &rows.y, &penalties, penalty.opponent_lambda, use_, 30, 1e-8)?;
    let mut choice_means = vec![0.0; width];
    let mut n = 0.0;
    for r in 0..rows.y.len() {
        if let Some(us) = use_ {
            if us[r] == 0 {
                continue;
            }
        }
        n += 1.0;
        for c in 0..width {
            choice_means[c] += rows.x[r * rows.d + from + c];
        }
    }
    for c in 0..width {
        choice_means[c] /= js_max(1.0, n);
    }
    Ok(ExpectedModel {
        spec: spec.clone(),
        beta: f.beta,
        choice_means,
        opponents: rows.opponent_ids.iter().enumerate().map(|(k, id)| (*id, f.u[k])).collect(),
        games,
        penalty,
    })
}

/// Fits the expected win rate on `games` with `penalty`; None when too few.
pub fn fit_expected(games: &[&PreparedGame], age_of: AgeOf, penalty: Penalty, o: &ExpectedOptions) -> Result<Option<ExpectedModel>> {
    let fit: Vec<&PreparedGame> = games.iter().copied().filter(|g| g.source != "me").collect();
    if fit.len() < o.min_games {
        return Ok(None);
    }
    let spec = spec_of(&fit, age_of, o);
    Ok(Some(fit_rows(&design(&fit, &spec, age_of), &spec, o, penalty, None, fit.len())?))
}

/// A game's expected win rate before it started, with the average mix of pre-game choices.
pub fn expected_of(m: &ExpectedModel, g: &PreparedGame, age_of: AgeOf) -> f64 {
    let mut x = vec![0.0; m.beta.len()];
    features_into(&mut x, g, &m.spec, age_of, Some(&m.choice_means));
    let mut eta = g.opponent.map(|o| m.opponents.get(&o).copied().unwrap_or(0.0)).unwrap_or(0.0);
    for i in 0..x.len() {
        eta += x[i] * m.beta[i];
    }
    sigmoid(eta)
}

/// FNV-1a over UTF-16 code units, as the evaluation's folds.
pub fn fnv(s: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for c in s.encode_utf16() {
        h ^= c as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

/// Every game's expected win rate from a model fitted without its pilot's games, and the penalty used.
pub fn cross_fitted(games: &[&PreparedGame], age_of: AgeOf, o: &ExpectedOptions) -> Result<(Vec<f64>, Option<Penalty>)> {
    let empty = (vec![f64::NAN; games.len()], None);
    let at: Vec<usize> = games.iter().enumerate().filter(|(_, g)| g.source != "me").map(|(i, _)| i).collect();
    if at.len() < o.min_games {
        return Ok(empty);
    }
    let fit: Vec<&PreparedGame> = at.iter().map(|i| games[*i]).collect();
    let spec = spec_of(&fit, age_of, o);
    let rows = design(&fit, &spec, age_of);
    let fold_of: Vec<usize> = fit.iter().map(|g| fnv(&format!("expected:{}", g.pilot)) as usize % o.folds).collect();
    let mut x = vec![0.0; rows.d];
    let mut best: Option<(Vec<f64>, Penalty, f64)> = None;
    for penalty in &o.penalties {
        let mut values = vec![f64::NAN; games.len()];
        let (mut loss, mut n) = (0.0, 0.0);
        for f in 0..o.folds {
            let use_: Vec<u8> = fold_of.iter().map(|k| if *k == f { 0 } else { 1 }).collect();
            let trained: usize = use_.iter().map(|k| *k as usize).sum();
            if trained < o.min_games || trained == fit.len() {
                continue;
            }
            let m = fit_rows(&rows, &spec, o, *penalty, Some(&use_), trained)?;
            for (j, g) in fit.iter().enumerate() {
                if fold_of[j] != f {
                    continue;
                }
                values[at[j]] = expected_of(&m, g, age_of);
                features_into(&mut x, g, &spec, age_of, None);
                let mut eta = g.opponent.map(|o| m.opponents.get(&o).copied().unwrap_or(0.0)).unwrap_or(0.0);
                for i in 0..x.len() {
                    eta += x[i] * m.beta[i];
                }
                let p = js_min(1.0 - 1e-6, js_max(1e-6, sigmoid(eta)));
                loss -= if g.win != 0.0 { jsmath::log(p) } else { jsmath::log(1.0 - p) };
                n += 1.0;
            }
        }
        if n != 0.0 && best.as_ref().map(|b| loss / n < b.2).unwrap_or(true) {
            best = Some((values, *penalty, loss / n));
        }
    }
    Ok(match best {
        Some((v, p, _)) => (v, Some(p)),
        None => empty,
    })
}
