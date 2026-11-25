//! Held-out metrics and the guardrail that compares two models on them.

use bf_core::build::Metrics;
use serde::{Deserialize, Serialize};

/// Calibration bins per window (equal width over 0-1).
const BINS: usize = 10;
/// Predictions are clamped this far from 0 and 1 for the log loss (a confident miss costs ~13.8, not infinity).
const EPS: f64 = 1e-6;

/// Metrics and what the guardrail needs besides: examples per window, and each window's calibration noise floor: the
/// expected calibration error a perfectly calibrated model with these predictions would show from sampling alone,
/// Σ_bins n_b/n · √(2/π) · sd_b, where sd_b is the standard deviation of the bin's mean label. Teammates at the same
/// time share one label, so sd_b counts each (match, team) group as one draw: sd_b² = Σ_groups (Σ_i √(p_i(1−p_i)))² / n_b².
#[derive(Clone, Debug, PartialEq)]
pub struct Evaluation {
    pub metrics: Metrics,
    pub examples: [usize; 4],
    pub noise: [f64; 4],
}

/// Metrics of predictions `p` against labels `y` (0/1), each with its minute's window (0-3). Sums run in sample order.
pub fn metrics(p: &[f64], y: &[f64], window: &[usize]) -> Metrics {
    let groups: Vec<u64> = (0..p.len() as u64).collect();
    evaluation(p, y, window, &groups).metrics
}

/// `group[i]`: examples with the same group share their label (one match and team).
pub fn evaluation(p: &[f64], y: &[f64], window: &[usize], group: &[u64]) -> Evaluation {
    let n = p.len();
    let (mut ll, mut br) = (0.0, 0.0);
    // per window, per bin: count, Σp, Σy
    let mut bins = [[(0.0f64, 0.0f64, 0.0f64); BINS]; 4];
    let mut counts = [0.0f64; 4];
    // (window, bin, group) → Σ √(p(1−p)), in a BTreeMap so the sum's order is fixed.
    let mut sd: std::collections::BTreeMap<(usize, usize, u64), f64> = std::collections::BTreeMap::new();
    for i in 0..n {
        let q = p[i].clamp(EPS, 1.0 - EPS);
        ll -= if y[i] > 0.5 { bf_core::jsmath::log(q) } else { bf_core::jsmath::log(1.0 - q) };
        br += (p[i] - y[i]) * (p[i] - y[i]);
        let b = ((p[i] * BINS as f64) as usize).min(BINS - 1);
        let w = window[i].min(3);
        let e = &mut bins[w][b];
        e.0 += 1.0;
        e.1 += p[i];
        e.2 += y[i];
        counts[w] += 1.0;
        *sd.entry((w, b, group[i])).or_insert(0.0) += (p[i] * (1.0 - p[i])).max(0.0).sqrt();
    }
    let mut var = [[0.0f64; BINS]; 4];
    for ((w, b, _), s) in &sd {
        var[*w][*b] += s * s;
    }
    let calibration = (0..4)
        .map(|w| {
            if counts[w] == 0.0 {
                return 0.0;
            }
            let mut ece = 0.0;
            for b in &bins[w] {
                if b.0 > 0.0 {
                    ece += b.0 / counts[w] * ((b.1 - b.2) / b.0).abs();
                }
            }
            ece
        })
        .collect();
    let noise = std::array::from_fn(|w| {
        let mut f = 0.0;
        for (k, b) in bins[w].iter().enumerate() {
            if b.0 > 0.0 {
                f += b.0 / counts[w] * (2.0 / std::f64::consts::PI).sqrt() * var[w][k].sqrt() / b.0;
            }
        }
        f
    });
    let nf = n.max(1) as f64;
    Evaluation { metrics: Metrics { games: 0, matches: 0, log_loss: ll / nf, brier: br / nf, calibration }, examples: counts.map(|c| c as usize), noise }
}

/// What counts as worse. Log loss and Brier must not rise at all. A window's calibration error is worse when it is
/// above both the old model's and the new model's noise floor (what sampling alone gives a perfectly calibrated model
/// with the same predictions: a sharper model spreads its examples over more bins and so shows more error by
/// chance; comparing raw errors would reject every model that beats a constant), plus `noise_allowance` times that
/// floor (the floor is an expected value: a calibrated model's error scatters around it) and `calibration_slack`.
/// Windows with fewer than `min_window_examples` examples are not judged.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Guardrail {
    pub log_loss_slack: f64,
    pub brier_slack: f64,
    pub calibration_slack: f64,
    pub noise_allowance: f64,
    pub min_window_examples: usize,
}

impl Default for Guardrail {
    fn default() -> Self {
        Guardrail { log_loss_slack: 0.0, brier_slack: 0.0, calibration_slack: 0.002, noise_allowance: 0.5, min_window_examples: 200 }
    }
}

impl Guardrail {
    /// None when `new` is not worse than `old` on any metric, else what got worse ("log loss 0.6712 > 0.6705").
    pub fn worse(&self, old: &Metrics, new: &Evaluation) -> Option<String> {
        let window_examples = &new.examples;
        let floor = &new.noise;
        let new = &new.metrics;
        let mut why = vec![];
        if !(new.log_loss <= old.log_loss + self.log_loss_slack) {
            why.push(format!("log loss {:.5} > {:.5}", new.log_loss, old.log_loss));
        }
        if !(new.brier <= old.brier + self.brier_slack) {
            why.push(format!("Brier {:.5} > {:.5}", new.brier, old.brier));
        }
        const NAMES: [&str; 4] = ["0-10", "10-20", "20-30", "30+"];
        for w in 0..4 {
            if window_examples[w] < self.min_window_examples {
                continue;
            }
            let (o, n) = (old.calibration.get(w).copied().unwrap_or(0.0), new.calibration.get(w).copied().unwrap_or(0.0));
            if !(n <= o.max(floor[w]) + self.noise_allowance * floor[w] + self.calibration_slack) {
                why.push(format!("calibration {} min {:.4} > {:.4} (noise floor {:.4})", NAMES[w], n, o, floor[w]));
            }
        }
        (!why.is_empty()).then(|| why.join(", "))
    }
}

pub fn fmt_metrics(m: &Metrics) -> String {
    format!(
        "log loss {:.5}, Brier {:.5}, calibration {}",
        m.log_loss,
        m.brier,
        m.calibration.iter().map(|c| format!("{c:.4}")).collect::<Vec<_>>().join("/")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfect_and_coin_flip() {
        let y = [1.0, 0.0, 1.0, 0.0];
        let w = [0, 1, 2, 3];
        let m = metrics(&[0.5; 4], &y, &w);
        assert!((m.log_loss - std::f64::consts::LN_2).abs() < 1e-12);
        assert!((m.brier - 0.25).abs() < 1e-12);
        assert!(m.calibration.iter().all(|c| (*c - 0.5).abs() < 1e-12));
        let g = Guardrail { min_window_examples: 1, ..Default::default() };
        let gr = [0, 1, 2, 3];
        let better = evaluation(&[0.6, 0.4, 0.6, 0.4], &y, &w, &gr);
        assert!(g.worse(&m, &better).is_none());
        assert!(g.worse(&better.metrics, &evaluation(&[0.5; 4], &y, &w, &gr)).unwrap().contains("log loss"));
        // A miscalibrated model: right ranking, far too confident.
        let y: Vec<f64> = (0..4000).map(|i| (i % 2) as f64).collect();
        let w = vec![1; 4000];
        let gr: Vec<u64> = (0..4000).collect();
        let fair = evaluation(&y.iter().map(|v| if *v > 0.5 { 0.6 } else { 0.4 }).collect::<Vec<_>>(), &y, &w, &gr);
        let wild: Vec<f64> = (0..4000).map(|i| if i % 2 == 1 { 0.99 } else if i % 4 == 0 { 0.01 } else { 0.98 }).collect();
        let wild = evaluation(&wild, &y, &w, &gr);
        assert!(g.worse(&fair.metrics, &wild).is_some());
    }
}
