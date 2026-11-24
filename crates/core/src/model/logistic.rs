//! Port of `server/model/logistic.ts`: L2-regularised logistic regression by Newton's method (IRLS). The loops keep
//! the TS order of every floating-point sum, so fits are the same to the last bit.

use crate::jsmath;
use anyhow::{bail, Result};

#[inline]
pub fn sigmoid(z: f64) -> f64 {
    if z >= 0.0 {
        1.0 / (1.0 + jsmath::exp(-z))
    } else {
        jsmath::exp(z) / (1.0 + jsmath::exp(z))
    }
}

/// Σ a[i]·b[i] over a's length.
#[inline]
pub fn dot(a: &[f64], b: &[f64]) -> f64 {
    let mut s = 0.0;
    for i in 0..a.len() {
        s += a[i] * b.get(i).copied().unwrap_or(f64::NAN);
    }
    s
}

pub struct FitOptions<'a> {
    pub lambda: f64,
    pub weights: Option<&'a [f64]>,
    pub max_iterations: usize,
    pub tolerance: f64,
}

impl Default for FitOptions<'_> {
    fn default() -> Self {
        FitOptions { lambda: 1.0, weights: None, max_iterations: 30, tolerance: 1e-8 }
    }
}

#[derive(Clone, Debug)]
pub struct Fit {
    pub beta: Vec<f64>,
    pub iterations: usize,
    pub converged: bool,
}

/// Solves A x = b for a symmetric positive-definite A (Cholesky). A is d×d, row-major.
pub fn solve_spd(a: &[f64], b: &[f64], d: usize) -> Result<Vec<f64>> {
    let mut l = vec![0.0; d * d];
    for i in 0..d {
        for j in 0..=i {
            let mut s = a[i * d + j];
            for k in 0..j {
                s -= l[i * d + k] * l[j * d + k];
            }
            if i == j {
                if s <= 0.0 {
                    bail!("matrix is not positive definite");
                }
                l[i * d + i] = s.sqrt();
            } else {
                l[i * d + j] = s / l[j * d + j];
            }
        }
    }
    let mut y = vec![0.0; d];
    for i in 0..d {
        let mut s = b[i];
        for k in 0..i {
            s -= l[i * d + k] * y[k];
        }
        y[i] = s / l[i * d + i];
    }
    let mut x = vec![0.0; d];
    for i in (0..d).rev() {
        let mut s = y[i];
        for k in i + 1..d {
            s -= l[k * d + i] * x[k];
        }
        x[i] = s / l[i * d + i];
    }
    Ok(x)
}

/// Rows of a design matrix: anything that gives row r as a slice.
pub trait Rows {
    fn len(&self) -> usize;
    fn row(&self, r: usize) -> &[f64];
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Rows for [Vec<f64>] {
    fn len(&self) -> usize {
        <[Vec<f64>]>::len(self)
    }
    fn row(&self, r: usize) -> &[f64] {
        &self[r]
    }
}
impl Rows for Vec<Vec<f64>> {
    fn len(&self) -> usize {
        Vec::len(self)
    }
    fn row(&self, r: usize) -> &[f64] {
        &self[r]
    }
}
impl Rows for [&[f64]] {
    fn len(&self) -> usize {
        <[&[f64]]>::len(self)
    }
    fn row(&self, r: usize) -> &[f64] {
        self[r]
    }
}
impl<'a> Rows for Vec<&'a [f64]> {
    fn len(&self) -> usize {
        Vec::len(self)
    }
    fn row(&self, r: usize) -> &[f64] {
        self[r]
    }
}

/// Maximises Σ wᵢ·loglik(yᵢ | xᵢ·β) − λ/2·‖β₁..‖². Rows must start with a 1 (intercept).
pub fn fit_logistic<R: Rows + ?Sized>(x: &R, y: &[f64], o: &FitOptions) -> Result<Fit> {
    let n = x.len();
    if n == 0 {
        bail!("no rows to fit");
    }
    let d = x.row(0).len();
    let mut beta = vec![0.0; d];
    let max_it = o.max_iterations;
    let tol = o.tolerance;
    let mut h = vec![0.0; d * d];
    let mut g = vec![0.0; d];
    for it in 1..=max_it {
        h.iter_mut().for_each(|v| *v = 0.0);
        g.iter_mut().for_each(|v| *v = 0.0);
        for r in 0..n {
            let xr = x.row(r);
            let sw = match o.weights {
                Some(w) => w[r],
                None => 1.0,
            };
            if sw == 0.0 || sw.is_nan() {
                continue;
            }
            let p = sigmoid(dot(xr, &beta));
            let resid = sw * (y[r] - p);
            let w = sw * p * (1.0 - p);
            for i in 0..d {
                let xi = xr[i];
                if xi == 0.0 {
                    continue;
                }
                g[i] += resid * xi;
                let wxi = w * xi;
                let hrow = &mut h[i * d..i * d + i + 1];
                for j in 0..=i {
                    hrow[j] += wxi * xr[j];
                }
            }
        }
        for i in 0..d {
            for j in 0..i {
                h[j * d + i] = h[i * d + j];
            }
        }
        for i in 1..d {
            h[i * d + i] += o.lambda;
            g[i] -= o.lambda * beta[i];
        }
        h[0] += 1e-9;
        let step = solve_spd(&h, &g, d)?;
        let mut max: f64 = 0.0;
        for i in 0..d {
            beta[i] += step[i];
            max = js_max(max, step[i].abs());
        }
        if max < tol {
            return Ok(Fit { beta, iterations: it, converged: true });
        }
    }
    Ok(Fit { beta, iterations: max_it, converged: false })
}

/// `Math.max(a, b)` (NaN wins).
#[inline]
pub fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a > b {
        a
    } else if b > a {
        b
    } else if a == 0.0 && b == 0.0 {
        if a.is_sign_positive() || b.is_sign_positive() { 0.0 } else { -0.0 }
    } else {
        a
    }
}

/// `Math.min(a, b)` (NaN wins).
#[inline]
pub fn js_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a < b {
        a
    } else if b < a {
        b
    } else if a == 0.0 && b == 0.0 {
        if a.is_sign_negative() || b.is_sign_negative() { -0.0 } else { 0.0 }
    } else {
        a
    }
}

#[inline]
pub fn predict_logistic(beta: &[f64], x: &[f64]) -> f64 {
    sigmoid(dot(beta, x))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Score {
    pub brier: f64,
    pub log_loss: f64,
    pub n: usize,
}

/// Brier score and log loss of probabilities `p` against outcomes `y`.
pub fn score_probabilities(p: &[f64], y: &[f64]) -> Score {
    let mut brier = 0.0;
    let mut log_loss = 0.0;
    for i in 0..p.len() {
        let q = js_min(1.0 - 1e-12, js_max(1e-12, p[i]));
        brier += (q - y[i]) * (q - y[i]);
        log_loss -= y[i] * jsmath::log(q) + (1.0 - y[i]) * jsmath::log(1.0 - q);
    }
    let n = p.len() as f64;
    Score { brier: brier / n, log_loss: log_loss / n, n: p.len() }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CalibrationRow {
    pub from: f64,
    pub to: f64,
    pub n: f64,
    pub predicted: f64,
    pub observed: f64,
}

/// Calibration table: predictions in `bins` equal-width buckets.
pub fn calibration(p: &[f64], y: &[f64], bins: usize) -> Vec<CalibrationRow> {
    let mut rows: Vec<CalibrationRow> = (0..bins)
        .map(|i| CalibrationRow { from: i as f64 / bins as f64, to: (i + 1) as f64 / bins as f64, n: 0.0, predicted: 0.0, observed: 0.0 })
        .collect();
    for i in 0..p.len() {
        let b = ((p[i] * bins as f64).floor() as usize).min(bins - 1);
        rows[b].n += 1.0;
        rows[b].predicted += p[i];
        rows[b].observed += y[i];
    }
    rows.into_iter()
        .filter(|r| r.n > 0.0)
        .map(|r| CalibrationRow { predicted: r.predicted / r.n, observed: r.observed / r.n, ..r })
        .collect()
}

/// One bin of a reliability table: how many predictions fell in `[from, to)` (the last bin also holds 1.0), their mean
/// prediction and the share that won. Both means are `None` for an empty bin.
#[derive(Clone, Debug, PartialEq)]
pub struct ReliabilityBin {
    pub from: f64,
    pub to: f64,
    pub n: f64,
    pub predicted: Option<f64>,
    pub observed: Option<f64>,
}

/// A reliability table over `bins` equal-width bins of [0, 1] (every bin listed, empty ones too) and the expected
/// calibration error ECE = Σ |B_m|/N · |mean outcome in B_m − mean prediction in B_m| (0 for no predictions).
#[derive(Clone, Debug, PartialEq)]
pub struct Reliability {
    pub n: f64,
    pub ece: f64,
    pub bins: Vec<ReliabilityBin>,
}

/// Reliability of probabilities `p` against outcomes `y` in `bins` equal-width bins. Pairs with a non-finite
/// prediction are left out.
pub fn reliability(p: &[f64], y: &[f64], bins: usize) -> Reliability {
    let bins = bins.max(1);
    let mut n = vec![0.0f64; bins];
    let mut sp = vec![0.0f64; bins];
    let mut sy = vec![0.0f64; bins];
    for i in 0..p.len().min(y.len()) {
        if !p[i].is_finite() {
            continue;
        }
        let b = ((p[i].clamp(0.0, 1.0) * bins as f64).floor() as usize).min(bins - 1);
        n[b] += 1.0;
        sp[b] += p[i];
        sy[b] += y[i];
    }
    let total: f64 = n.iter().sum();
    let mut ece = 0.0;
    let rows = (0..bins)
        .map(|b| {
            let (predicted, observed) = if n[b] > 0.0 { (Some(sp[b] / n[b]), Some(sy[b] / n[b])) } else { (None, None) };
            if let (Some(pm), Some(om)) = (predicted, observed) {
                ece += n[b] / total * (om - pm).abs();
            }
            ReliabilityBin { from: b as f64 / bins as f64, to: (b + 1) as f64 / bins as f64, n: n[b], predicted, observed }
        })
        .collect();
    Reliability { n: total, ece, bins: rows }
}

#[cfg(test)]
mod reliability_tests {
    use super::*;

    #[test]
    fn no_predictions_is_zero_with_every_bin_empty() {
        let r = reliability(&[], &[], 20);
        assert_eq!((r.n, r.ece, r.bins.len()), (0.0, 0.0, 20));
        assert!(r.bins.iter().all(|b| b.n == 0.0 && b.predicted.is_none() && b.observed.is_none()));
    }

    #[test]
    fn all_in_one_bin() {
        // 0.62 falls in bin 12 of 20 ([0.60, 0.65)); mean prediction 0.62, win rate 0.75: ECE = 0.13.
        let r = reliability(&[0.62; 4], &[1.0, 1.0, 1.0, 0.0], 20);
        assert_eq!(r.n, 4.0);
        assert!((r.ece - 0.13).abs() < 1e-12);
        let full: Vec<_> = r.bins.iter().enumerate().filter(|(_, b)| b.n > 0.0).collect();
        assert_eq!(full.len(), 1);
        assert_eq!(full[0].0, 12);
        assert_eq!((full[0].1.n, full[0].1.predicted, full[0].1.observed), (4.0, Some(0.62), Some(0.75)));
    }

    #[test]
    fn perfect_calibration_is_zero() {
        // 0.5 twice with one win (mean 0.5 = 0.5), 1.0 won, 0.0 lost: every bin matches.
        let r = reliability(&[0.5, 0.5, 1.0, 0.0], &[1.0, 0.0, 1.0, 0.0], 20);
        assert_eq!(r.ece, 0.0);
    }

    #[test]
    fn two_bins_weighted_by_their_share_and_edges_land_right() {
        // 0.0 -> bin 0; 1.0 -> the last bin; 0.05 starts bin 1.
        let r = reliability(&[0.0, 1.0, 0.05], &[1.0, 0.0, 1.0], 20);
        assert_eq!((r.bins[0].n, r.bins[1].n, r.bins[19].n), (1.0, 1.0, 1.0));
        // |1 − 0|/3 + |0 − 1|/3 + |1 − 0.05|/3 = (1 + 1 + 0.95)/3
        assert!((r.ece - 2.95 / 3.0).abs() < 1e-12);
    }
}
