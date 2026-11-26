//! Model step 3 (1.3.0): the team network. It looks at all ten players at once (champion, items, build stats, base
//! stats at their level) and adds a residual to steps 1 + 2.
//!
//! It is dormant until enough games with a timeline exist (`TeamOptions::threshold`). The learner then wakes it, it
//! learns every new game and the stored backlog, and each check judges it on top of the models in force. Like step 2,
//! its last layer starts at zero, so it changes nothing until it has learnt something that passes the guardrail.
//!
//! Shape: a player network shared by all ten (`PIN` → `P1` → `P2`, tanh), then the user's output, the mean of their
//! allies' and the mean of their enemies', with the game state and the user's combat terms, into a head (`HIN` → `H`
//! → 1). About 69,000 weights, plain Rust like step 2 (fixed summation order, deterministic).

use crate::combat::{combat_vector, PlayerIn, TERMS};
use crate::features::{hash, sigmoid, Rng, DENSE};
use crate::nn::NetOptions;
use bf_core::ddragon::{StaticData, ITEM_STATS};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

pub const E: usize = 12;
const CH: usize = 1024;
const IT: usize = 4096;
/// A player's own numbers: build stats per 1,000 gold, base stats at their level, level, has items.
pub const PF: usize = ITEM_STATS + 6 + 1;
pub const PIN: usize = 2 * E + PF;
pub const P1: usize = 32;
pub const P2: usize = 24;
pub const HIN: usize = 3 * P2 + DENSE + TERMS;
pub const H: usize = 48;

const O_CH: usize = 0;
const O_IT: usize = O_CH + CH * E;
const O_W1: usize = O_IT + IT * E;
const O_B1: usize = O_W1 + P1 * PIN;
const O_W2: usize = O_B1 + P1;
const O_B2: usize = O_W2 + P2 * P1;
const O_WH: usize = O_B2 + P2;
const O_BH: usize = O_WH + H * HIN;
const O_W3: usize = O_BH + H;
const O_B3: usize = O_W3 + H;
pub const TEAM_PARAMS: usize = O_B3 + 1;
const CHUNK: usize = 32;

/// When the team network wakes up and how it learns.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TeamOptions {
    /// Games with a timeline (stored or learnt) before it wakes up.
    pub threshold: u64,
    /// Stored games it works through per `absorb` once awake (the backlog).
    pub backlog_per_call: usize,
    /// Its own examples kept for replay.
    pub replay: usize,
    pub net: NetOptions,
}

impl Default for TeamOptions {
    fn default() -> Self {
        TeamOptions {
            threshold: 200_000,
            backlog_per_call: 150,
            replay: 10_000,
            net: NetOptions { learning_rate: 0.001, batch: 256, epochs: 1, cold_epochs: 1, l2: 1e-5, seed: 0x7ea3, ..NetOptions::default() },
        }
    }
}

/// One player as the network sees them.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerX {
    pub champ_row: u32,
    pub item_rows: Vec<u32>,
    pub feats: [f32; PF],
}

/// One example: the user (player `me`) at one minute, with the other nine.
#[derive(Clone, Debug, PartialEq)]
pub struct TeamSample {
    pub players: Vec<PlayerX>,
    /// Ally flags per player (the user's team), and the user's index.
    pub ally: Vec<bool>,
    pub me: usize,
    pub dense: [f64; DENSE],
    pub terms: [f32; TERMS],
}

fn champ_row(id: i64) -> u32 {
    (O_CH + (id.rem_euclid(CH as i64) as usize) * E) as u32
}
fn item_row(id: i64) -> u32 {
    (O_IT + (hash(&[9, id]) % IT as u64) as usize * E) as u32
}

/// A player's inputs from Data Dragon: build stats per 1,000 gold (the combat vector's first part), base stats at
/// their level, level.
pub fn player_x(data: &StaticData, champion: i64, level: f64, items: &[i64]) -> PlayerX {
    let v = combat_vector(data, &PlayerIn { champion, level, items }, &[]);
    let base = data.champion(champion).map(|c| c.base).unwrap_or_default();
    let mut feats = [0.0f32; PF];
    for k in 0..ITEM_STATS {
        feats[k] = v[k];
    }
    let b = [base.hp_at(level) / 2000.0, base.armor_at(level) / 100.0, base.mr_at(level) / 100.0, base.ad_at(level) / 150.0, base.attack_speed, level / 18.0];
    for (k, x) in b.iter().enumerate() {
        feats[ITEM_STATS + k] = *x as f32;
    }
    feats[PF - 1] = if items.is_empty() { 0.0 } else { 1.0 };
    PlayerX { champ_row: champ_row(champion), item_rows: items.iter().map(|i| item_row(*i)).collect(), feats }
}

/// The intermediate values of a forward pass, for the backward pass.
struct Pass {
    x: Vec<[f64; PIN]>,
    h1: Vec<[f64; P1]>,
    u: Vec<[f64; P2]>,
    hin: [f64; HIN],
    hh: [f64; H],
    out: f64,
    n_ally: usize,
    n_enemy: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TeamNet {
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

impl TeamNet {
    pub fn new(seed: u64) -> TeamNet {
        let mut r = Rng(seed);
        let mut p = vec![0.0; TEAM_PARAMS];
        for x in p[O_CH..O_W1].iter_mut() {
            *x = r.normal() * 0.05;
        }
        for (lo, hi, fan) in [(O_W1, O_B1, PIN), (O_W2, O_B2, P1), (O_WH, O_BH, HIN)] {
            let s = 1.0 / (fan as f64).sqrt();
            for x in p[lo..hi].iter_mut() {
                *x = r.normal() * s;
            }
        }
        // The output layer starts at zero: a fresh network adds nothing.
        TeamNet { p, m: vec![0.0; TEAM_PARAMS], v: vec![0.0; TEAM_PARAMS], t: 0 }
    }

    pub fn bytes(&self) -> usize {
        TEAM_PARAMS * 8 * 3
    }

    fn player_in(&self, q: &PlayerX) -> [f64; PIN] {
        let p = &self.p;
        let mut x = [0.0; PIN];
        let c = q.champ_row as usize;
        x[..E].copy_from_slice(&p[c..c + E]);
        if !q.item_rows.is_empty() {
            let s = 1.0 / q.item_rows.len() as f64;
            for r in &q.item_rows {
                let r = *r as usize;
                for k in 0..E {
                    x[E + k] += p[r + k] * s;
                }
            }
        }
        for k in 0..PF {
            x[2 * E + k] = q.feats[k] as f64;
        }
        x
    }

    fn forward(&self, s: &TeamSample) -> Pass {
        let p = &self.p;
        let n = s.players.len();
        let mut x = Vec::with_capacity(n);
        let mut h1 = Vec::with_capacity(n);
        let mut u = Vec::with_capacity(n);
        for q in &s.players {
            let xi = self.player_in(q);
            let mut a = [0.0; P1];
            for (j, a) in a.iter_mut().enumerate() {
                let w = &p[O_W1 + j * PIN..O_W1 + (j + 1) * PIN];
                let mut z = p[O_B1 + j];
                for k in 0..PIN {
                    z += w[k] * xi[k];
                }
                *a = tanh(z);
            }
            let mut b = [0.0; P2];
            for (j, b) in b.iter_mut().enumerate() {
                let w = &p[O_W2 + j * P1..O_W2 + (j + 1) * P1];
                let mut z = p[O_B2 + j];
                for k in 0..P1 {
                    z += w[k] * a[k];
                }
                *b = tanh(z);
            }
            x.push(xi);
            h1.push(a);
            u.push(b);
        }
        let mut hin = [0.0; HIN];
        let (mut n_ally, mut n_enemy) = (0usize, 0usize);
        for i in 0..n {
            if i == s.me {
                hin[..P2].copy_from_slice(&u[i]);
            } else if s.ally[i] {
                n_ally += 1;
            } else {
                n_enemy += 1;
            }
        }
        for i in 0..n {
            if i == s.me {
                continue;
            }
            let (off, cnt) = if s.ally[i] { (P2, n_ally) } else { (2 * P2, n_enemy) };
            for k in 0..P2 {
                hin[off + k] += u[i][k] / cnt as f64;
            }
        }
        hin[3 * P2..3 * P2 + DENSE].copy_from_slice(&s.dense);
        for k in 0..TERMS {
            hin[3 * P2 + DENSE + k] = s.terms[k] as f64;
        }
        let mut hh = [0.0; H];
        for (j, h) in hh.iter_mut().enumerate() {
            let w = &p[O_WH + j * HIN..O_WH + (j + 1) * HIN];
            let mut z = p[O_BH + j];
            for k in 0..HIN {
                z += w[k] * hin[k];
            }
            *h = tanh(z);
        }
        let mut out = p[O_B3];
        for k in 0..H {
            out += p[O_W3 + k] * hh[k];
        }
        Pass { x, h1, u, hin, hh, out, n_ally, n_enemy }
    }

    /// What the network adds to steps 1 + 2.
    pub fn residual(&self, s: &TeamSample) -> f64 {
        self.forward(s).out
    }

    fn backward(&self, s: &TeamSample, f: &Pass, e: f64, g: &mut [f64]) {
        let p = &self.p;
        g[O_B3] += e;
        let mut dzh = [0.0; H];
        for k in 0..H {
            g[O_W3 + k] += e * f.hh[k];
            dzh[k] = e * p[O_W3 + k] * (1.0 - f.hh[k] * f.hh[k]);
        }
        let mut dhin = [0.0; HIN];
        for j in 0..H {
            if dzh[j] == 0.0 {
                continue;
            }
            g[O_BH + j] += dzh[j];
            let w = O_WH + j * HIN;
            for k in 0..HIN {
                g[w + k] += dzh[j] * f.hin[k];
                dhin[k] += dzh[j] * p[w + k];
            }
        }
        for i in 0..s.players.len() {
            let du: Vec<f64> = if i == s.me {
                dhin[..P2].to_vec()
            } else if s.ally[i] {
                dhin[P2..2 * P2].iter().map(|d| d / f.n_ally as f64).collect()
            } else {
                dhin[2 * P2..3 * P2].iter().map(|d| d / f.n_enemy as f64).collect()
            };
            let (x, h1, u) = (&f.x[i], &f.h1[i], &f.u[i]);
            let mut dh1 = [0.0; P1];
            for j in 0..P2 {
                let dz = du[j] * (1.0 - u[j] * u[j]);
                if dz == 0.0 {
                    continue;
                }
                g[O_B2 + j] += dz;
                let w = O_W2 + j * P1;
                for k in 0..P1 {
                    g[w + k] += dz * h1[k];
                    dh1[k] += dz * p[w + k];
                }
            }
            let mut dx = [0.0; PIN];
            for j in 0..P1 {
                let dz = dh1[j] * (1.0 - h1[j] * h1[j]);
                if dz == 0.0 {
                    continue;
                }
                g[O_B1 + j] += dz;
                let w = O_W1 + j * PIN;
                for k in 0..PIN {
                    g[w + k] += dz * x[k];
                    dx[k] += dz * p[w + k];
                }
            }
            let q = &s.players[i];
            let c = q.champ_row as usize;
            for k in 0..E {
                g[c + k] += dx[k];
            }
            if !q.item_rows.is_empty() {
                let sc = 1.0 / q.item_rows.len() as f64;
                for r in &q.item_rows {
                    let r = *r as usize;
                    for k in 0..E {
                        g[r + k] += dx[E + k] * sc;
                    }
                }
            }
        }
    }

    /// Mini-batch Adam on top of steps 1 + 2's logits `base`. Ok(batches), or Err(batches) when stopped.
    #[allow(clippy::too_many_arguments)]
    pub fn train(&mut self, samples: &[TeamSample], labels: &[f64], base: &[f64], weights: &[f64], order: &[usize], o: &NetOptions, stop: &(dyn Fn() -> bool + Sync)) -> Result<usize, usize> {
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
                        let mut g = vec![0.0; TEAM_PARAMS];
                        for &i in c {
                            let f = net.forward(&samples[i]);
                            let e = weights[i] * (sigmoid(base[i] + f.out) - labels[i]) / wsum;
                            net.backward(&samples[i], &f, e, &mut g);
                        }
                        g
                    })
                    .collect();
                let mut g = vec![0.0; TEAM_PARAMS];
                for part in &parts {
                    for (a, b) in g.iter_mut().zip(part) {
                        *a += *b;
                    }
                }
                self.t += 1;
                let b1t = 1.0 - bf_core::jsmath::pow(o.beta1, self.t as f64);
                let b2t = 1.0 - bf_core::jsmath::pow(o.beta2, self.t as f64);
                for j in 0..TEAM_PARAMS {
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

    pub fn residuals(&self, xs: &[TeamSample]) -> Vec<f64> {
        xs.par_iter().map(|s| self.residual(s)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(r: &mut Rng) -> TeamSample {
        let players = (0..10)
            .map(|i| {
                let mut feats = [0.0f32; PF];
                for f in feats.iter_mut() {
                    *f = r.normal() as f32 * 0.5;
                }
                PlayerX { champ_row: champ_row(i * 37 + 1), item_rows: (0..(i % 4) as i64).map(|k| item_row(3000 + k * 31 + i as i64)).collect(), feats }
            })
            .collect();
        let mut dense = [0.0; DENSE];
        for d in dense.iter_mut() {
            *d = r.normal() * 0.5;
        }
        let mut terms = [0.0f32; TERMS];
        for t in terms.iter_mut() {
            *t = r.normal() as f32 * 0.5;
        }
        TeamSample { players, ally: (0..10).map(|i| i < 5).collect(), me: 2, dense, terms }
    }

    /// The hand-written backward pass agrees with finite differences on every kind of weight.
    #[test]
    fn gradient_matches_finite_differences() {
        let mut net = TeamNet::new(5);
        let mut r = Rng(11);
        for x in net.p[O_W3..O_B3].iter_mut() {
            *x = r.normal() * 0.5;
        }
        let s = sample(&mut r);
        let mut g = vec![0.0; TEAM_PARAMS];
        let f = net.forward(&s);
        net.backward(&s, &f, 1.0, &mut g);
        let probes = [O_B3, O_W3 + 3, O_BH + 5, O_WH + 7 * HIN + 2, O_WH + 2 * HIN + 30, O_B2 + 4, O_W2 + 3 * P1 + 1, O_B1 + 2, O_W1 + 5 * PIN + 30, s.players[2].champ_row as usize + 1, s.players[7].champ_row as usize + 4, s.players[3].item_rows[0] as usize + 2];
        for &j in &probes {
            let h = 1e-6;
            let mut a = net.clone();
            a.p[j] += h;
            let mut b = net.clone();
            b.p[j] -= h;
            let num = (a.residual(&s) - b.residual(&s)) / (2.0 * h);
            assert!((num - g[j]).abs() < 1e-6 * (1.0 + num.abs()), "param {j}: numeric {num} vs backprop {}", g[j]);
        }
        // A fresh network adds nothing.
        assert_eq!(TeamNet::new(5).residual(&s), 0.0);
    }
}
