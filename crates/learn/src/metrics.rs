//! Held-out metrics and the guardrail that compares two models on them.

use bf_core::build::Metrics;
use serde::{Deserialize, Serialize};

/// Calibration bins per window (equal width over 0-1).
const BINS: usize = 10;
/// Predictions are clamped this far from 0 and 1 for the log loss (a confident miss costs ~13.8, not infinity).
const EPS: f64 = 1e-6;

/// Metrics of predictions `p` against labels `y` (0/1), each with its minute's window (0-3). Sums run in sample order.
pub fn metrics(p: &[f64], y: &[f64], window: &[usize]) -> Metrics {
    let n = p.len();
    let (mut ll, mut br) = (0.0, 0.0);
    // per window, per bin: count, Σp, Σy
    let mut bins = [[(0.0f64, 0.0f64, 0.0f64); BINS]; 4];
    let mut counts = [0.0f64; 4];
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
    let nf = n.max(1) as f64;
    Metrics { games: 0, log_loss: ll / nf, brier: br / nf, calibration }
}

/// What counts as worse. Log loss and Brier must not rise at all; calibration error per window may rise by
/// `calibration_slack` (it is noisy: ten bins over a few thousand examples move by about that between two equally
/// good models), and only in windows with at least `min_window_examples` examples.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Guardrail {
    pub log_loss_slack: f64,
    pub brier_slack: f64,
    pub calibration_slack: f64,
    pub min_window_examples: usize,
}

impl Default for Guardrail {
    fn default() -> Self {
        Guardrail { log_loss_slack: 0.0, brier_slack: 0.0, calibration_slack: 0.005, min_window_examples: 200 }
    }
}

impl Guardrail {
    /// None when `new` is not worse than `old` on any metric, else what got worse ("log loss 0.6712 > 0.6705").
    pub fn worse(&self, old: &Metrics, new: &Metrics, window_examples: &[usize; 4]) -> Option<String> {
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
            if !(n <= o + self.calibration_slack) {
                why.push(format!("calibration {} min {:.4} > {:.4}", NAMES[w], n, o));
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
        let better = metrics(&[0.6, 0.4, 0.6, 0.4], &y, &w);
        assert!(g.worse(&m, &better, &[1; 4]).is_none());
        assert!(g.worse(&better, &m, &[1; 4]).unwrap().contains("log loss"));
    }
}
