//! Model step 2: a small network shared by every champion, on top of step 1.
//!
//! Inputs are embeddings (champions, items, runes, summoners and skill orders, 8 numbers each), their products for
//! the interactions a linear model can't express (this champion with these items, with these allies and against
//! these enemies, this opponent with this keystone, these items against this damage profile), the role and profile,
//! and the game state. Two tanh layers (32, 16)
//! give one number that is added to step 1's logit. The last layer starts at zero, so a fresh network predicts
//! exactly like step 1 and only ships once it has learnt something that beats it on held-out games.
//!
//! Plain Rust: about 55 thousand parameters don't need a framework, and keeping every sum in a fixed order keeps it
//! deterministic. burn and candle were not adopted (not measured; see docs/MEASUREMENTS.md).

use crate::features::{hash, key_id, sigmoid, Rng, Sample, Situation, DENSE};
use bf_core::build::DamageProfile;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

pub const E: usize = 8;
const CH: usize = 1024;
const IT: usize = 4096;
const RU: usize = 512;
const MI: usize = 256;
const BLOCKS: usize = 16;
const EXTRA: usize = 5 + 3 + DENSE + 2;
pub const IN: usize = BLOCKS * E + EXTRA;
pub const H1: usize = 32;
pub const H2: usize = 16;

const O_CH: usize = 0;
const O_IT: usize = O_CH + CH * E;
const O_RU: usize = O_IT + IT * E;
const O_MI: usize = O_RU + RU * E;
const O_W1: usize = O_MI + MI * E;
const O_B1: usize = O_W1 + H1 * IN;
const O_W2: usize = O_B1 + H1;
const O_B2: usize = O_W2 + H2 * H1;
const O_W3: usize = O_B2 + H2;
const O_B3: usize = O_W3 + H2;
pub const PARAMS: usize = O_B3 + 1;

/// Samples per rayon task when computing a batch's gradient: fixed, so the partial sums (and their order) never
/// depend on the thread count.
const CHUNK: usize = 32;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NetOptions {
    pub learning_rate: f64,
    pub beta1: f64,
    pub beta2: f64,
    pub l2: f64,
    pub batch: usize,
    pub epochs: usize,
    /// Epochs of the first update of a fresh network.
    pub cold_epochs: usize,
    pub seed: u64,
}

impl Default for NetOptions {
    fn default() -> Self {
        NetOptions { learning_rate: 0.003, beta1: 0.9, beta2: 0.999, l2: 1e-5, batch: 256, epochs: 2, cold_epochs: 4, seed: 0x6e6e }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SharedNet {
    pub p: Vec<f64>,
    pub m: Vec<f64>,
    pub v: Vec<f64>,
    pub t: u64,
}

#[inline]
fn tanh(x: f64) -> f64 {
    let e = bf_core::jsmath::exp(-2.0 * x.abs());
    let t = (1.0 - e) / (1.0 + e);
    if x < 0.0 {
        -t
    } else {
        t
    }
}

/// Where a situation's embeddings are (rows of the tables), worked out once per situation.
struct Lookup {
    me: usize,
    opp: Option<usize>,
    enemies: Vec<usize>,
    allies: Vec<usize>,
    items: Vec<usize>,
    boots: Option<usize>,
    key: Option<usize>,
    page: Option<usize>,
    summ: Option<usize>,
    skill: Option<usize>,
    magic: f64,
    physical: f64,
}

fn champ_row(id: i64) -> usize {
    O_CH + (id.rem_euclid(CH as i64) as usize) * E
}
fn item_row(id: i64) -> usize {
    O_IT + (hash(&[7, id]) % IT as u64) as usize * E
}
fn rune_row(parts: &[i64]) -> usize {
    O_RU + (hash(parts) % RU as u64) as usize * E
}
fn misc_row(parts: &[i64]) -> usize {
    O_MI + (hash(parts) % MI as u64) as usize * E
}

fn lookup(s: &Situation) -> Lookup {
    Lookup {
        me: champ_row(s.champion),
        opp: s.opponent.map(champ_row),
        enemies: s.enemies.iter().map(|c| champ_row(*c)).collect(),
        allies: s.allies.iter().map(|c| champ_row(*c)).collect(),
        items: s.items.iter().map(|i| item_row(*i)).collect(),
        boots: s.boots.map(item_row),
        key: (s.keystone != 0).then(|| rune_row(&[8, s.keystone])),
        page: s.rune_page.as_deref().map(|k| rune_row(&[9, key_id(k)])),
        summ: s.summoners.as_deref().map(|k| misc_row(&[10, key_id(k)])),
        skill: s.skill_max.as_deref().map(|k| misc_row(&[11, key_id(k)])),
        magic: if s.profile == Some(DamageProfile::Magic) { 1.0 } else { 0.0 },
        physical: if s.profile == Some(DamageProfile::Physical) { 1.0 } else { 0.0 },
    }
}

fn add_row(p: &[f64], row: usize, scale: f64, out: &mut [f64]) {
    for k in 0..E {
        out[k] += p[row + k] * scale;
    }
}

fn mean_rows(p: &[f64], rows: &[usize], out: &mut [f64]) {
    if rows.is_empty() {
        return;
    }
    let s = 1.0 / rows.len() as f64;
    for r in rows {
        add_row(p, *r, s, out);
    }
}

/// A forward pass's intermediate values (kept for the backward pass).
struct Pass {
    x: [f64; IN],
    h1: [f64; H1],
    h2: [f64; H2],
    out: f64,
}

impl SharedNet {
    pub fn new(seed: u64) -> SharedNet {
        let mut r = Rng(seed);
        let mut p = vec![0.0; PARAMS];
        for x in p[O_CH..O_W1].iter_mut() {
            *x = r.normal() * 0.05;
        }
        let s1 = 1.0 / (IN as f64).sqrt();
        for x in p[O_W1..O_B1].iter_mut() {
            *x = r.normal() * s1;
        }
        let s2 = 1.0 / (H1 as f64).sqrt();
        for x in p[O_W2..O_B2].iter_mut() {
            *x = r.normal() * s2;
        }
        // W3 and every bias start at zero: the network adds nothing until trained.
        SharedNet { p, m: vec![0.0; PARAMS], v: vec![0.0; PARAMS], t: 0 }
    }

    pub fn bytes(&self) -> usize {
        PARAMS * 8 * 3
    }

    fn inputs(&self, s: &Situation, l: &Lookup) -> [f64; IN] {
        let p = &self.p;
        let mut x = [0.0; IN];
        let b = |k: usize| k * E..(k + 1) * E;
        add_row(p, l.me, 1.0, &mut x[b(0)]);
        if let Some(o) = l.opp {
            add_row(p, o, 1.0, &mut x[b(1)]);
        }
        mean_rows(p, &l.enemies, &mut x[b(2)]);
        mean_rows(p, &l.allies, &mut x[b(3)]);
        mean_rows(p, &l.items, &mut x[b(4)]);
        if let Some(r) = l.boots {
            add_row(p, r, 1.0, &mut x[b(5)]);
        }
        if let Some(r) = l.key {
            add_row(p, r, 1.0, &mut x[b(6)]);
        }
        if let Some(r) = l.page {
            add_row(p, r, 1.0, &mut x[b(7)]);
        }
        if let Some(r) = l.summ {
            add_row(p, r, 1.0, &mut x[b(8)]);
        }
        if let Some(r) = l.skill {
            add_row(p, r, 1.0, &mut x[b(9)]);
        }
        for k in 0..E {
            x[10 * E + k] = x[k] * x[4 * E + k];
            x[11 * E + k] = x[E + k] * x[6 * E + k];
            x[12 * E + k] = x[4 * E + k] * l.magic;
            x[13 * E + k] = x[4 * E + k] * l.physical;
            x[14 * E + k] = x[k] * x[3 * E + k];
            x[15 * E + k] = x[k] * x[2 * E + k];
        }
        let mut at = BLOCKS * E;
        if let Some(r) = s.role {
            x[at + r.index()] = 1.0;
        }
        at += 5;
        if let Some(pr) = s.profile {
            x[at + pr as usize] = 1.0;
        }
        at += 3;
        for (k, d) in s.state.dense().iter().enumerate() {
            x[at + k] = *d;
        }
        at += DENSE;
        x[at] = if l.opp.is_some() { 1.0 } else { 0.0 };
        x[at + 1] = l.items.len() as f64 / 3.0;
        x
    }

    fn forward(&self, s: &Situation, l: &Lookup) -> Pass {
        let p = &self.p;
        let x = self.inputs(s, l);
        let mut h1 = [0.0; H1];
        for (j, h) in h1.iter_mut().enumerate() {
            let w = &p[O_W1 + j * IN..O_W1 + (j + 1) * IN];
            let mut z = p[O_B1 + j];
            for k in 0..IN {
                z += w[k] * x[k];
            }
            *h = tanh(z);
        }
        let mut h2 = [0.0; H2];
        for (j, h) in h2.iter_mut().enumerate() {
            let w = &p[O_W2 + j * H1..O_W2 + (j + 1) * H1];
            let mut z = p[O_B2 + j];
            for k in 0..H1 {
                z += w[k] * h1[k];
            }
            *h = tanh(z);
        }
        let mut out = p[O_B3];
        for k in 0..H2 {
            out += p[O_W3 + k] * h2[k];
        }
        Pass { x, h1, h2, out }
    }

    /// What the network adds to step 1's logit.
    pub fn residual(&self, s: &Situation) -> f64 {
        self.forward(s, &lookup(s)).out
    }

    /// Adds d(loss)/d(params) for one example with output gradient `e` into `g`.
    fn backward(&self, l: &Lookup, f: &Pass, e: f64, g: &mut [f64]) {
        let p = &self.p;
        g[O_B3] += e;
        let mut d2 = [0.0; H2];
        for k in 0..H2 {
            g[O_W3 + k] += e * f.h2[k];
            d2[k] = e * p[O_W3 + k] * (1.0 - f.h2[k] * f.h2[k]);
        }
        let mut d1 = [0.0; H1];
        for j in 0..H2 {
            if d2[j] == 0.0 {
                continue;
            }
            g[O_B2 + j] += d2[j];
            let w = O_W2 + j * H1;
            for k in 0..H1 {
                g[w + k] += d2[j] * f.h1[k];
                d1[k] += d2[j] * p[w + k];
            }
        }
        let mut dx = [0.0; IN];
        for j in 0..H1 {
            let dz = d1[j] * (1.0 - f.h1[j] * f.h1[j]);
            if dz == 0.0 {
                continue;
            }
            g[O_B1 + j] += dz;
            let w = O_W1 + j * IN;
            for k in 0..IN {
                g[w + k] += dz * f.x[k];
                dx[k] += dz * p[w + k];
            }
        }
        // Back into the embeddings, through the means and the products.
        let x = &f.x;
        let mut d_me = [0.0; E];
        let mut d_opp = [0.0; E];
        let mut d_items = [0.0; E];
        let mut d_key = [0.0; E];
        for k in 0..E {
            d_me[k] = dx[k] + dx[10 * E + k] * x[4 * E + k] + dx[14 * E + k] * x[3 * E + k] + dx[15 * E + k] * x[2 * E + k];
            d_items[k] = dx[4 * E + k] + dx[10 * E + k] * x[k] + dx[12 * E + k] * l.magic + dx[13 * E + k] * l.physical;
            d_opp[k] = dx[E + k] + dx[11 * E + k] * x[6 * E + k];
            d_key[k] = dx[6 * E + k] + dx[11 * E + k] * x[E + k];
        }
        let put = |g: &mut [f64], row: usize, d: &[f64], scale: f64| {
            for k in 0..E {
                g[row + k] += d[k] * scale;
            }
        };
        put(g, l.me, &d_me, 1.0);
        if let Some(o) = l.opp {
            put(g, o, &d_opp, 1.0);
        }
        for (rows, blk, prod) in [(&l.enemies, 2usize, 15usize), (&l.allies, 3, 14)] {
            if !rows.is_empty() {
                let s = 1.0 / rows.len() as f64;
                let mut d = [0.0; E];
                for k in 0..E {
                    d[k] = dx[blk * E + k] + dx[prod * E + k] * x[k];
                }
                for r in rows.iter() {
                    put(g, *r, &d, s);
                }
            }
        }
        if !l.items.is_empty() {
            let s = 1.0 / l.items.len() as f64;
            for r in &l.items {
                put(g, *r, &d_items, s);
            }
        }
        if let Some(r) = l.boots {
            put(g, r, &dx[5 * E..6 * E], 1.0);
        }
        if let Some(r) = l.key {
            put(g, r, &d_key, 1.0);
        }
        for (r, blk) in [(l.page, 7usize), (l.summ, 8), (l.skill, 9)] {
            if let Some(r) = r {
                put(g, r, &dx[blk * E..(blk + 1) * E], 1.0);
            }
        }
    }

    /// Mini-batch Adam on top of step 1's logits `base` (one per sample). Ok(batches), or Err(batches) when stopped.
    #[allow(clippy::too_many_arguments)]
    pub fn train(&mut self, samples: &[Sample], base: &[f64], weights: &[f64], order: &[usize], o: &NetOptions, stop: &(dyn Fn() -> bool + Sync)) -> Result<usize, usize> {
        let mut batches = 0;
        let epochs = if self.t == 0 { o.cold_epochs.max(o.epochs) } else { o.epochs };
        for _ in 0..epochs {
            for chunk in order.chunks(o.batch.max(1)) {
                if stop() {
                    return Err(batches);
                }
                let wsum: f64 = chunk.iter().map(|i| weights[*i]).sum();
                if wsum <= 0.0 {
                    continue;
                }
                let net = &*self;
                let parts: Vec<Vec<f64>> = chunk
                    .par_chunks(CHUNK)
                    .map(|c| {
                        let mut g = vec![0.0; PARAMS];
                        for &i in c {
                            let s = &samples[i].situation;
                            let l = lookup(s);
                            let f = net.forward(s, &l);
                            let e = weights[i] * (sigmoid(base[i] + f.out) - samples[i].label()) / wsum;
                            net.backward(&l, &f, e, &mut g);
                        }
                        g
                    })
                    .collect();
                let mut g = vec![0.0; PARAMS];
                for part in &parts {
                    for (a, b) in g.iter_mut().zip(part) {
                        *a += *b;
                    }
                }
                self.t += 1;
                let b1t = 1.0 - bf_core::jsmath::pow(o.beta1, self.t as f64);
                let b2t = 1.0 - bf_core::jsmath::pow(o.beta2, self.t as f64);
                for j in 0..PARAMS {
                    let gj = g[j] + o.l2 * self.p[j];
                    if gj == 0.0 && self.m[j] == 0.0 {
                        continue;
                    }
                    self.m[j] = o.beta1 * self.m[j] + (1.0 - o.beta1) * gj;
                    self.v[j] = o.beta2 * self.v[j] + (1.0 - o.beta2) * gj * gj;
                    self.p[j] -= o.learning_rate * (self.m[j] / b1t) / ((self.v[j] / b2t).sqrt() + 1e-8);
                }
                batches += 1;
            }
        }
        Ok(batches)
    }

    /// Residuals for many situations (parallel per situation; each is independent).
    pub fn residuals(&self, xs: &[&Situation]) -> Vec<f64> {
        xs.par_iter().map(|s| self.residual(s)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::GameState;

    #[test]
    fn gradient_matches_finite_differences() {
        let mut net = SharedNet::new(7);
        let mut r = Rng(3);
        for x in net.p[O_W3..O_B3].iter_mut() {
            *x = r.normal() * 0.5;
        }
        let s = Situation {
            champion: 39,
            role: Some(bf_core::build::Role::Top),
            opponent: Some(122),
            enemies: vec![122, 64, 103],
            allies: vec![1, 51],
            profile: Some(DamageProfile::Magic),
            keystone: 8010,
            rune_page: Some("x".into()),
            summoners: Some("4,12".into()),
            items: vec![3153, 3078],
            boots: Some(3047),
            state: GameState { minute: 20.0, gold_diff: 800.0, ..Default::default() },
            ..Default::default()
        };
        let l = lookup(&s);
        let mut g = vec![0.0; PARAMS];
        let f = net.forward(&s, &l);
        net.backward(&l, &f, 1.0, &mut g);
        for &j in &[O_B3, O_W3 + 3, O_W2 + 5, O_W1 + 17, l.me + 2, l.items[1] + 4, l.key.unwrap() + 1, l.opp.unwrap() + 6, l.allies[0] + 3, l.enemies[1] + 5] {
            let h = 1e-6;
            let old = net.p[j];
            net.p[j] = old + h;
            let a = net.residual(&s);
            net.p[j] = old - h;
            let b = net.residual(&s);
            net.p[j] = old;
            let num = (a - b) / (2.0 * h);
            assert!((num - g[j]).abs() < 1e-6 * (1.0 + num.abs()), "param {j}: {num} vs {}", g[j]);
        }
    }
}
