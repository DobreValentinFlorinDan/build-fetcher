//! Model step 1: logistic regression per champion-role with shrinkage toward a global model.
//!
//! One weight vector holds both parts. The global part (2^16 hashed slots) is shared by every champion; the
//! champion-role part (2^19 slots, hashed on champion, role and feature, with a random sign so collisions cancel on
//! average) holds each pair's deviation from it. A pair's model is global + its deviation, and the deviation's
//! stronger L2 pulls it toward the global model: a pair with few games predicts like the global model, one with many
//! learns its own effects. Training is warm-started mini-batch Adam: each update continues from the last weights and
//! moments, it never refits from scratch.

use crate::backend::{Backend, Csr};
use crate::features::{hash, key_id, sigmoid, Sample, Situation};
use bf_core::build::DamageProfile;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const GLOBAL_BITS: u32 = 16;
pub const PAIR_BITS: u32 = 19;
pub const GLOBAL: usize = 1 << GLOBAL_BITS;
pub const PAIR: usize = 1 << PAIR_BITS;
pub const DIM: usize = GLOBAL + PAIR;
/// Global slots below this are the dense game-state inputs and the bias, never hashed into.
const RESERVED: usize = 32;
const BIAS: usize = RESERVED - 1;

fn profile_id(p: Option<DamageProfile>) -> i64 {
    match p {
        None => 0,
        Some(DamageProfile::Physical) => 1,
        Some(DamageProfile::Magic) => 2,
        Some(DamageProfile::Mixed) => 3,
    }
}

/// The sparse features of a situation, in a fixed order (the order sums run in).
pub fn featurize(s: &Situation, out: &mut Vec<(u32, f64)>) {
    out.clear();
    out.push((BIAS as u32, 1.0));
    for (i, x) in s.state.dense().iter().enumerate() {
        if *x != 0.0 {
            out.push((i as u32, *x));
        }
    }
    let role = s.role.map(|r| r.index() as i64).unwrap_or(5);
    let prof = profile_id(s.profile);
    let opp = s.opponent.unwrap_or(0);
    let page = s.rune_page.as_deref().map(key_id).unwrap_or(0);
    let g = |parts: &[i64]| -> (u32, f64) { ((RESERVED as u64 + hash(parts) % (GLOBAL - RESERVED) as u64) as u32, 1.0) };
    // Per pair: the slot and a ±1 sign from the same hash.
    let p = |parts: &[i64], v: f64| -> (u32, f64) {
        let mut all = vec![1000, s.champion, role];
        all.extend_from_slice(parts);
        let h = hash(&all);
        ((GLOBAL as u64 + (h >> 1) % PAIR as u64) as u32, if h & 1 == 1 { v } else { -v })
    };
    out.push(g(&[1, role]));
    out.push(g(&[2, s.champion, role]));
    out.push(g(&[20, prof]));
    out.push(p(&[0], 1.0));
    out.push(p(&[1], s.state.minute / 10.0));
    out.push(p(&[20, prof], 1.0));
    if opp != 0 {
        out.push(g(&[3, opp, role]));
        out.push(p(&[3, opp], 1.0));
    }
    for e in &s.enemies {
        out.push(g(&[4, *e]));
        out.push(p(&[4, *e], 1.0));
    }
    for a in &s.allies {
        out.push(g(&[5, *a]));
    }
    if s.keystone != 0 {
        out.push(g(&[6, s.keystone]));
        out.push(p(&[6, s.keystone], 1.0));
        if opp != 0 {
            out.push(g(&[17, s.keystone, opp]));
            out.push(p(&[17, s.keystone, opp], 1.0));
        }
    }
    if page != 0 {
        out.push(g(&[7, page]));
        out.push(p(&[7, page], 1.0));
        if opp != 0 {
            out.push(p(&[18, page, opp], 1.0));
        }
    }
    if s.sub_style != 0 {
        out.push(g(&[8, s.sub_style]));
    }
    if let Some(k) = s.summoners.as_deref() {
        out.push(g(&[9, key_id(k)]));
        out.push(p(&[9, key_id(k)], 1.0));
    }
    if let Some(k) = s.skill_max.as_deref() {
        out.push(g(&[10, key_id(k)]));
        out.push(p(&[10, key_id(k)], 1.0));
    }
    if let Some(k) = s.starting.as_deref() {
        out.push(g(&[11, key_id(k)]));
        out.push(p(&[11, key_id(k)], 1.0));
    }
    for (pos, it) in s.items.iter().enumerate() {
        out.push(g(&[12, *it]));
        out.push(p(&[12, *it], 1.0));
        out.push(g(&[16, *it, prof]));
        out.push(p(&[16, *it, prof], 1.0));
        if pos < 3 {
            out.push(g(&[14, pos as i64, *it]));
            out.push(p(&[14, pos as i64, *it], 1.0));
        }
    }
    if let Some(b) = s.boots {
        out.push(g(&[15, b]));
        out.push(p(&[15, b], 1.0));
        out.push(g(&[19, b, prof]));
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrainOptions {
    pub learning_rate: f64,
    pub beta1: f64,
    pub beta2: f64,
    /// L2 on the global part, and on each pair's deviation (larger: pairs stay closer to the global model).
    pub l2_global: f64,
    pub l2_pair: f64,
    pub batch: usize,
    pub epochs: usize,
}

impl Default for TrainOptions {
    fn default() -> Self {
        TrainOptions { learning_rate: 0.01, beta1: 0.9, beta2: 0.999, l2_global: 1e-4, l2_pair: 3e-3, batch: 512, epochs: 2 }
    }
}

/// Weights and Adam's moments (kept in checkpoints so the next update continues where this one stopped).
#[derive(Clone, Debug, PartialEq)]
pub struct LogisticModel {
    pub w: Vec<f64>,
    pub m: Vec<f64>,
    pub v: Vec<f64>,
    /// Adam steps taken.
    pub t: u64,
    /// Training examples seen per (champion, role index).
    pub pair_samples: BTreeMap<(i64, u8), u64>,
}

impl Default for LogisticModel {
    fn default() -> Self {
        LogisticModel { w: vec![0.0; DIM], m: vec![0.0; DIM], v: vec![0.0; DIM], t: 0, pair_samples: BTreeMap::new() }
    }
}

/// Stops training between mini-batches when it returns true.
pub type StopCheck<'a> = &'a (dyn Fn() -> bool + Sync);

impl LogisticModel {
    pub fn logit(&self, s: &Situation) -> f64 {
        let mut f = Vec::with_capacity(96);
        featurize(s, &mut f);
        let mut z = 0.0;
        for (i, v) in &f {
            z += self.w[*i as usize] * v;
        }
        z
    }

    pub fn predict(&self, s: &Situation) -> f64 {
        sigmoid(self.logit(s))
    }

    /// Logits of many situations through `backend` (the evaluation's forward pass).
    pub fn logits(&self, xs: &[&Situation], backend: &dyn Backend) -> Vec<f64> {
        let csr = to_csr(xs);
        let mut out = vec![0.0; xs.len()];
        backend.logits(&csr, &self.w, &mut out);
        out
    }

    pub fn bytes(&self) -> usize {
        DIM * 8 * 3
    }

    /// Mini-batch Adam over `order` (indices into `samples`), `epochs` times. Ok(batches) or Err(batches) when
    /// `stop` said to stop (the model is then half-updated: the caller drops it).
    pub fn train(&mut self, samples: &[Sample], weights: &[f64], order: &[usize], o: &TrainOptions, backend: &dyn Backend, stop: StopCheck) -> Result<usize, usize> {
        let mut grad = vec![0.0f64; DIM];
        let mut mark = vec![false; DIM];
        let mut touched: Vec<u32> = Vec::new();
        let mut batches = 0;
        for &i in order {
            let s = &samples[i].situation;
            if let Some(r) = s.role {
                *self.pair_samples.entry((s.champion, r.index() as u8)).or_insert(0) += 1;
            }
        }
        for _ in 0..o.epochs {
            for chunk in order.chunks(o.batch.max(1)) {
                if stop() {
                    return Err(batches);
                }
                let xs: Vec<&Situation> = chunk.iter().map(|i| &samples[*i].situation).collect();
                let csr = to_csr(&xs);
                let mut z = vec![0.0; chunk.len()];
                backend.logits(&csr, &self.w, &mut z);
                let wsum: f64 = chunk.iter().map(|i| weights[*i]).sum();
                if wsum <= 0.0 {
                    continue;
                }
                // The scatter runs in sample order on one thread: the gradient's sums never depend on threads.
                for (r, i) in chunk.iter().enumerate() {
                    let e = weights[*i] * (sigmoid(z[r]) - samples[*i].label()) / wsum;
                    for k in csr.ptr[r]..csr.ptr[r + 1] {
                        let j = csr.idx[k];
                        if !mark[j as usize] {
                            mark[j as usize] = true;
                            touched.push(j);
                        }
                        grad[j as usize] += e * csr.val[k];
                    }
                }
                touched.sort_unstable();
                self.t += 1;
                let b1t = 1.0 - bf_core::jsmath::pow(o.beta1, self.t as f64);
                let b2t = 1.0 - bf_core::jsmath::pow(o.beta2, self.t as f64);
                for &j in &touched {
                    let j = j as usize;
                    let l2 = if j < RESERVED { 0.0 } else if j < GLOBAL { o.l2_global } else { o.l2_pair };
                    let g = grad[j] + l2 * self.w[j];
                    self.m[j] = o.beta1 * self.m[j] + (1.0 - o.beta1) * g;
                    self.v[j] = o.beta2 * self.v[j] + (1.0 - o.beta2) * g * g;
                    let mh = self.m[j] / b1t;
                    let vh = self.v[j] / b2t;
                    self.w[j] -= o.learning_rate * mh / (vh.sqrt() + 1e-8);
                    grad[j] = 0.0;
                    mark[j] = false;
                }
                touched.clear();
                batches += 1;
            }
        }
        Ok(batches)
    }
}

/// Situations as sparse rows (featurized in parallel; `collect` keeps the order).
pub fn to_csr(xs: &[&Situation]) -> Csr {
    let rows: Vec<Vec<(u32, f64)>> = xs
        .par_iter()
        .map(|s| {
            let mut f = Vec::with_capacity(96);
            featurize(s, &mut f);
            f
        })
        .collect();
    let mut csr = Csr::new();
    for r in &rows {
        csr.push_row(r);
    }
    csr
}
