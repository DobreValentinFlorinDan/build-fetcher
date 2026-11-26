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
/// The enemy-build region (1.2.0), after the pair region so earlier checkpoints keep their slots and load with it at
/// zero (they then predict exactly as before). Its first half is shared like the global part, its second half holds
/// champion-role deviations (signed, like the pair part).
pub const ENEMY_BITS: u32 = 16;
pub const ENEMY: usize = 1 << ENEMY_BITS;
const ENEMY_START: usize = GLOBAL + PAIR;
const ENEMY_HALF: usize = ENEMY / 2;
/// The enemy tags that meet the player's items one by one: armor, magic resist, healing, anti-heal, health.
const INTERACT_TAGS: [usize; 5] = [0, 1, 2, 3, 6];
/// The stat region (1.3.0), after the enemy region, starting at zero like it: one shared weight per combat number,
/// then each champion-role's own weight per combat number (signed, like the pair part).
pub const STAT_BITS: u32 = 16;
pub const STAT: usize = 1 << STAT_BITS;
const STAT_START: usize = GLOBAL + PAIR + ENEMY;
const STAT_HALF: usize = STAT / 2;
pub const DIM: usize = GLOBAL + PAIR + ENEMY + STAT;
/// Global slots below this are the dense game-state inputs and the bias, never hashed into.
const RESERVED: usize = 40;
const KNOT_FEATURES: usize = 8;
/// Rows featurized at once when scoring many situations.
const EVAL_ROWS: usize = 8192;
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
    // The game state's coefficients vary with time: each input feeds the two nearest of four time knots (5, 15, 25
    // and 35 minutes), weighted linearly between them, so a gold lead can mean more at 25 minutes than at 8 and each
    // calibration window gets slopes of its own without a jump between windows.
    let st = &s.state;
    let base = [1.0, st.gold_diff / 1000.0, st.cs_diff / 20.0, st.level_diff / 2.0, st.kill_diff / 5.0, st.tower_diff / 2.0, st.dragon_diff / 2.0, st.baron_diff];
    let pos = ((st.minute - 5.0) / 10.0).clamp(0.0, 3.0);
    let k0 = (pos.floor() as usize).min(2);
    let a1 = pos - k0 as f64;
    for (k, a) in [(k0, 1.0 - a1), (k0 + 1, a1)] {
        if a == 0.0 {
            continue;
        }
        for (f, x) in base.iter().enumerate() {
            if *x != 0.0 {
                out.push(((k * KNOT_FEATURES + f) as u32, x * a));
            }
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
    // Decisions made before the game (and the draft itself) also get an early-game copy that fades out by minute 20.
    // Their effect works mostly through the leads they build, which the game state then shows: the plain copy learns
    // what they add given the state, the early copy their whole effect (what the recommender ranks them by, at minute 0).
    let early = (1.0 - s.state.minute / 20.0).max(0.0);
    let pre = |out: &mut Vec<(u32, f64)>, f: (u32, f64), e: (u32, f64)| {
        out.push(f);
        if early > 0.0 {
            out.push((e.0, e.1 * early));
        }
    };
    out.push(g(&[1, role]));
    pre(out, g(&[2, s.champion, role]), g(&[102, s.champion, role]));
    pre(out, g(&[20, prof]), g(&[120, prof]));
    out.push(p(&[0], 1.0));
    out.push(p(&[1], s.state.minute / 10.0));
    out.push(p(&[20, prof], 1.0));
    if opp != 0 {
        pre(out, g(&[3, opp, role]), g(&[103, opp, role]));
        pre(out, p(&[3, opp], 1.0), p(&[103, opp], 1.0));
    }
    for e in &s.enemies {
        pre(out, g(&[4, *e]), g(&[104, *e]));
        out.push(p(&[4, *e], 1.0));
    }
    for a in &s.allies {
        pre(out, g(&[5, *a]), g(&[105, *a]));
    }
    if s.keystone != 0 {
        pre(out, g(&[6, s.keystone]), g(&[106, s.keystone]));
        pre(out, p(&[6, s.keystone], 1.0), p(&[106, s.keystone], 1.0));
        if opp != 0 {
            pre(out, g(&[17, s.keystone, opp]), g(&[117, s.keystone, opp]));
            pre(out, p(&[17, s.keystone, opp], 1.0), p(&[117, s.keystone, opp], 1.0));
        }
    }
    if page != 0 {
        pre(out, g(&[7, page]), g(&[107, page]));
        pre(out, p(&[7, page], 1.0), p(&[107, page], 1.0));
        if opp != 0 {
            pre(out, p(&[18, page, opp], 1.0), p(&[118, page, opp], 1.0));
        }
    }
    if s.sub_style != 0 {
        out.push(g(&[8, s.sub_style]));
    }
    if let Some(k) = s.summoners.as_deref() {
        pre(out, g(&[9, key_id(k)]), g(&[109, key_id(k)]));
        pre(out, p(&[9, key_id(k)], 1.0), p(&[109, key_id(k)], 1.0));
    }
    if let Some(k) = s.skill_max.as_deref() {
        pre(out, g(&[10, key_id(k)]), g(&[110, key_id(k)]));
        pre(out, p(&[10, key_id(k)], 1.0), p(&[110, key_id(k)], 1.0));
    }
    if let Some(k) = s.starting.as_deref() {
        pre(out, g(&[11, key_id(k)]), g(&[111, key_id(k)]));
        pre(out, p(&[11, key_id(k)], 1.0), p(&[111, key_id(k)], 1.0));
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
    enemy_features(s, role, out);
    stat_features(s, role, out);
}

/// The combat vector (1.3.0): a shared weight per number, and the champion-role's own (its taste for each stat).
fn stat_features(s: &Situation, role: i64, out: &mut Vec<(u32, f64)>) {
    for (k, x) in s.combat.iter().enumerate() {
        let x = *x as f64;
        if x == 0.0 {
            continue;
        }
        let k = k as i64;
        out.push(((STAT_START as u64 + hash(&[40, k]) % STAT_HALF as u64) as u32, x));
        // Each champion-role's taste: for the build's stats only (the combat terms are shared).
        if (k as usize) < bf_core::ddragon::ITEM_STATS {
            let h = hash(&[3000, s.champion, role, 41, k]);
            out.push(((STAT_START as u64 + STAT_HALF as u64 + (h >> 1) % STAT_HALF as u64) as u32, if h & 1 == 1 { x } else { -x }));
        }
    }
}

/// The enemy's build (1.2.0): each enemy item, each tag level, the champion-role's response to each tag, and each of
/// the player's items against each tag ("this item does well against armor stacking"), in the enemy region.
fn enemy_features(s: &Situation, role: i64, out: &mut Vec<(u32, f64)>) {
    if s.enemy_items.is_empty() {
        return;
    }
    let shared = |parts: &[i64], v: f64| -> (u32, f64) { ((ENEMY_START as u64 + hash(parts) % ENEMY_HALF as u64) as u32, v) };
    let own = |parts: &[i64], v: f64| -> (u32, f64) {
        let mut all = vec![2000, s.champion, role];
        all.extend_from_slice(parts);
        let h = hash(&all);
        ((ENEMY_START as u64 + ENEMY_HALF as u64 + (h >> 1) % ENEMY_HALF as u64) as u32, if h & 1 == 1 { v } else { -v })
    };
    let mine: Vec<i64> = s.items.iter().copied().chain(s.boots).collect();
    for (t, lv) in s.enemy_tags.iter().enumerate() {
        if *lv == 0.0 {
            continue;
        }
        let t = t as i64;
        out.push(shared(&[31, t], *lv));
        out.push(own(&[32, t], *lv));
        // Ability power and attack damage already meet the items through the damage profile ([16]).
        if INTERACT_TAGS.contains(&(t as usize)) {
            for it in &mine {
                out.push(shared(&[33, *it, t], *lv));
            }
        }
    }
}

fn default_l2_enemy() -> f64 {
    3e-3
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrainOptions {
    pub learning_rate: f64,
    pub beta1: f64,
    pub beta2: f64,
    /// L2 on the global part, and on each pair's deviation (larger: pairs stay closer to the global model).
    pub l2_global: f64,
    pub l2_pair: f64,
    /// L2 on the enemy region's shared half (item × enemy tag): weaker, because each item × tag pair is seen in few
    /// rows and a pull as strong as the global part's kept its weight near zero (planted effects were found at a
    /// tenth of their size on synthetic data). Its champion-role half takes `l2_pair`.
    #[serde(default = "default_l2_enemy")]
    pub l2_enemy: f64,
    pub batch: usize,
    pub epochs: usize,
    /// Epochs of the very first update (no model yet): it has to beat the baseline from one batch of records.
    pub cold_epochs: usize,
}

impl Default for TrainOptions {
    fn default() -> Self {
        TrainOptions { learning_rate: 0.003, beta1: 0.9, beta2: 0.999, l2_global: 1e-2, l2_pair: 3e-2, l2_enemy: default_l2_enemy(), batch: 512, epochs: 2, cold_epochs: 6 }
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
    /// Post-hoc calibration: the served logit is `(a0 + a1 * t) * z` of the raw one, a temperature that varies linearly
    /// with game time (`t` = minutes / 10, at most 4). Fitted after training on records held out from it
    /// (`fit_calibration`), never on the validation sample. Why: one update's mini-batches can move the few game-state
    /// weights too little or too much for how informative the state is at each point of a game, which leaves the
    /// model under- or over-confident; seen on synthetic data, the right temperature rises with game time (0.9 to 1.35
    /// on one set, 1.7 to 2.8 on another). A temperature never changes what the weights rank.
    pub calib: [f64; 2],
    /// A factor per calibration window (0-10, 10-20, 20-30, 30+ min) on top of the line (1.3.3), each changed only
    /// when its own held-out matches show a significant need: a line can't follow a model that needs ×2 mid-game
    /// and ×1 late (seen on a user's games, where the line overshot the 30+ window to ×2.8 and every check failed
    /// there).
    pub calib_win: [f64; 4],
}

/// No calibration: a temperature of 1 at every minute.
pub const IDENTITY_CALIB: [f64; 2] = [1.0, 0.0];
/// Game time in the temperature, in tens of minutes, capped where the examples stop (minute 40).
fn calib_time(minute: f64) -> f64 {
    (minute / 10.0).clamp(0.0, 4.0)
}

impl Default for LogisticModel {
    fn default() -> Self {
        LogisticModel { w: vec![0.0; DIM], m: vec![0.0; DIM], v: vec![0.0; DIM], t: 0, pair_samples: BTreeMap::new(), calib: IDENTITY_CALIB, calib_win: [1.0; 4] }
    }
}

/// A window's temperature changes only when the new fit differs from the one in force by more than this many standard
/// Fewer held-out matches than this: the calibration in force is kept. A cluster-robust covariance needs a few hundred
/// clusters before its own uncertainty is small; with about 200 a significant-looking line still misjudged the late
/// game on synthetic data. An update of 2,000 records holds out about 285.
pub const CALIB_MIN_GAMES: usize = 250;
/// The Wald statistic a change of the temperature line must exceed: the 95% point of χ² with 2 degrees of freedom.
const CALIB_WALD: f64 = 5.991;
/// The same for one window's factor: the 95% point of χ² with 1 degree of freedom.
const CALIB_WALD_1: f64 = 3.841;
/// Matches a window needs before its own factor can change.
pub const CALIB_WINDOW_GAMES: usize = 100;

fn inverse2(m: [[f64; 2]; 2]) -> [[f64; 2]; 2] {
    let det = m[0][0] * m[1][1] - m[0][1] * m[1][0];
    [[m[1][1] / det, -m[0][1] / det], [-m[1][0] / det, m[0][0] / det]]
}

fn mul2(a: [[f64; 2]; 2], b: [[f64; 2]; 2]) -> [[f64; 2]; 2] {
    let mut out = [[0.0; 2]; 2];
    for i in 0..2 {
        for j in 0..2 {
            out[i][j] = a[i][0] * b[0][j] + a[i][1] * b[1][j];
        }
    }
    out
}

/// Stops training between mini-batches when it returns true.
pub type StopCheck<'a> = &'a (dyn Fn() -> bool + Sync);

impl LogisticModel {
    /// The served (calibrated) logit.
    pub fn logit(&self, s: &Situation) -> f64 {
        self.calibrated(self.raw_logit(s), s.state.minute)
    }

    /// The weights' own logit (what training optimises).
    pub fn raw_logit(&self, s: &Situation) -> f64 {
        let mut f = Vec::with_capacity(96);
        featurize(s, &mut f);
        let mut z = 0.0;
        for (i, v) in &f {
            z += self.w[*i as usize] * v;
        }
        z
    }

    pub fn calibrated(&self, z: f64, minute: f64) -> f64 {
        (self.calib[0] + self.calib[1] * calib_time(minute)) * self.calib_win[crate::features::window_of(minute)] * z
    }

    /// Refits the calibration on held-out examples: the temperature line (a0, a1) that maximises their weighted log
    /// likelihood, by Newton's method from the line in force. It replaces the line in force only when a Wald test
    /// with a cluster-robust covariance says the two differ (`CALIB_WALD`, 95% for two parameters): no move on noise.
    ///
    /// - Matches are the clusters: the examples of one match (10 players at several minutes, both teams) share one
    ///   outcome, so they are one piece of evidence, not dozens. Counting them one by one overfitted small windows
    ///   on synthetic data (a = 1.27 to 1.6 from 50-100 matches of a model already calibrated).
    /// - One line over the whole game, not a temperature per window: a late window often has too few matches to fit
    ///   alone, and one temperature pooled over every window made some worse (they need different corrections).
    /// - No intercept: every match gives examples for both teams, so the true intercept is zero by symmetry.
    ///
    /// Sequential sums in example order: deterministic.
    pub fn fit_calibration(&mut self, held_out: &[Sample], weights: &[f64], backend: &dyn Backend) {
        let refs: Vec<&Situation> = held_out.iter().map(|s| &s.situation).collect();
        let mut z = vec![0.0; refs.len()];
        for (c, o) in refs.chunks(EVAL_ROWS).zip(z.chunks_mut(EVAL_ROWS)) {
            backend.logits(&to_csr(c), &self.w, o);
        }
        self.fit_calibration_on(held_out, weights, &z);
    }

    /// `fit_calibration` given the raw logits `z` of the held-out examples: the line, then the per-window factors.
    pub(crate) fn fit_calibration_on(&mut self, held_out: &[Sample], weights: &[f64], z: &[f64]) {
        let games = held_out.iter().map(|s| s.game).collect::<std::collections::BTreeSet<u64>>().len();
        if games < CALIB_MIN_GAMES {
            return;
        }
        let line = self.calib;
        self.fit_line(held_out, weights, z);
        if self.calib != line {
            // The factors were relative to the old line.
            self.calib_win = [1.0; 4];
        }
        self.fit_windows(held_out, weights, z);
    }

    /// Per window, the factor on the line's logit that maximises the window's weighted log likelihood (Newton, from
    /// the factor in force). It replaces the one in force only when a cluster-robust Wald test with one degree of
    /// freedom passes (95%, 3.841), and the window has at least `CALIB_WINDOW_GAMES` matches.
    fn fit_windows(&mut self, held_out: &[Sample], weights: &[f64], z: &[f64]) {
        for k in 0..4 {
            let idx: Vec<usize> = (0..held_out.len()).filter(|i| crate::features::window_of(held_out[*i].situation.state.minute) == k).collect();
            let games = idx.iter().map(|i| held_out[*i].game).collect::<std::collections::BTreeSet<u64>>().len();
            if games < CALIB_WINDOW_GAMES {
                continue;
            }
            // The line's logit for each example of the window.
            let b: Vec<f64> = idx.iter().map(|i| (self.calib[0] + self.calib[1] * calib_time(held_out[*i].situation.state.minute)) * z[*i]).collect();
            let old = self.calib_win[k];
            let mut r = old;
            let mut h = 0.0;
            for _ in 0..50 {
                let (mut g, mut hh) = (0.0, 0.0);
                for (n, i) in idx.iter().enumerate() {
                    let p = sigmoid(r * b[n]);
                    g += weights[*i] * (p - held_out[*i].label()) * b[n];
                    hh += weights[*i] * p * (1.0 - p) * b[n] * b[n];
                }
                if !(hh.is_finite() && hh > 0.0) {
                    break;
                }
                let d = g / hh;
                r -= d;
                h = hh;
                if d.abs() < 1e-12 {
                    break;
                }
            }
            if !(r.is_finite() && r > 0.05 && h > 0.0) {
                continue;
            }
            let mut per_game: BTreeMap<u64, f64> = BTreeMap::new();
            for (n, i) in idx.iter().enumerate() {
                let p = sigmoid(r * b[n]);
                *per_game.entry(held_out[*i].game).or_insert(0.0) += weights[*i] * (p - held_out[*i].label()) * b[n];
            }
            let meat: f64 = per_game.values().map(|x| x * x).sum();
            let var = meat / (h * h);
            let wald = (r - old) * (r - old) / var;
            if wald.is_finite() && wald > CALIB_WALD_1 {
                self.calib_win[k] = r;
            }
        }
    }

    /// The temperature line (see `fit_calibration`).
    fn fit_line(&mut self, held_out: &[Sample], weights: &[f64], z: &[f64]) {
        // The two inputs of the line for each example: z and z * t.
        let x: Vec<[f64; 2]> = held_out.iter().zip(z).map(|(s, z)| [*z, *z * calib_time(s.situation.state.minute)]).collect();
        let old = self.calib;
        let mut th = old;
        let mut hess = [[0.0; 2]; 2];
        for _ in 0..50 {
            let (mut g, mut h) = ([0.0; 2], [[0.0; 2]; 2]);
            for (i, s) in held_out.iter().enumerate() {
                let p = sigmoid(th[0] * x[i][0] + th[1] * x[i][1]);
                let (r, c) = (weights[i] * (p - s.label()), weights[i] * p * (1.0 - p));
                for j in 0..2 {
                    g[j] += r * x[i][j];
                    for k in 0..2 {
                        h[j][k] += c * x[i][j] * x[i][k];
                    }
                }
            }
            let det = h[0][0] * h[1][1] - h[0][1] * h[1][0];
            if !(det.is_finite() && det > 0.0) {
                return;
            }
            let d0 = (h[1][1] * g[0] - h[0][1] * g[1]) / det;
            let d1 = (h[0][0] * g[1] - h[1][0] * g[0]) / det;
            th = [th[0] - d0, th[1] - d1];
            hess = h;
            if d0.abs() < 1e-12 && d1.abs() < 1e-12 {
                break;
            }
        }
        // The temperature must stay positive over the whole game, or the ranking would flip.
        if !(th[0].is_finite() && th[1].is_finite() && th[0] > 0.0 && th[0] + 4.0 * th[1] > 0.0) {
            return;
        }
        // Cluster-robust covariance H⁻¹ (Σ_match s sᵀ) H⁻¹, then the Wald statistic of the change.
        let mut per_game: BTreeMap<u64, [f64; 2]> = BTreeMap::new();
        for (i, s) in held_out.iter().enumerate() {
            let p = sigmoid(th[0] * x[i][0] + th[1] * x[i][1]);
            let r = weights[i] * (p - s.label());
            let e = per_game.entry(s.game).or_insert([0.0; 2]);
            e[0] += r * x[i][0];
            e[1] += r * x[i][1];
        }
        let mut meat = [[0.0; 2]; 2];
        for sc in per_game.values() {
            for j in 0..2 {
                for k in 0..2 {
                    meat[j][k] += sc[j] * sc[k];
                }
            }
        }
        let inv = inverse2(hess);
        let cov = mul2(mul2(inv, meat), inv);
        let covi = inverse2(cov);
        let d = [th[0] - old[0], th[1] - old[1]];
        let wald = d[0] * (covi[0][0] * d[0] + covi[0][1] * d[1]) + d[1] * (covi[1][0] * d[0] + covi[1][1] * d[1]);
        if wald.is_finite() && wald > CALIB_WALD {
            self.calib = th;
        }
    }

    pub fn predict(&self, s: &Situation) -> f64 {
        sigmoid(self.logit(s))
    }

    /// Logits of many situations through `backend` (the evaluation's forward pass).
    pub fn logits(&self, xs: &[&Situation], backend: &dyn Backend) -> Vec<f64> {
        // In slices, so the sparse rows of a whole training set never sit in memory at once.
        let mut out = vec![0.0; xs.len()];
        for (c, o) in xs.chunks(EVAL_ROWS).zip(out.chunks_mut(EVAL_ROWS)) {
            backend.logits(&to_csr(c), &self.w, o);
        }
        for (z, s) in out.iter_mut().zip(xs) {
            *z = self.calibrated(*z, s.state.minute);
        }
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
        let epochs = if self.t == 0 { o.cold_epochs.max(o.epochs) } else { o.epochs };
        for _ in 0..epochs {
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
                    let l2 = if j < RESERVED {
                        0.0
                    } else if j < GLOBAL {
                        o.l2_global
                    } else if (ENEMY_START..ENEMY_START + ENEMY_HALF).contains(&j) {
                        o.l2_enemy
                    } else if (STAT_START..STAT_START + STAT_HALF).contains(&j) {
                        o.l2_pair
                    } else {
                        o.l2_pair
                    };
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
    // Featurized in parallel per slice of rows (each slice in order), then concatenated in order.
    let parts: Vec<Csr> = xs
        .par_chunks(256)
        .map(|c| {
            let mut csr = Csr::new();
            let mut f = Vec::with_capacity(128);
            for s in c {
                featurize(s, &mut f);
                csr.push_row(&f);
            }
            csr
        })
        .collect();
    let mut csr = Csr::new();
    for p in parts {
        let at = csr.idx.len();
        csr.idx.extend_from_slice(&p.idx);
        csr.val.extend_from_slice(&p.val);
        csr.ptr.extend(p.ptr[1..].iter().map(|x| x + at));
    }
    csr
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::{GameState, Rng};

    /// Held-out examples whose labels follow σ(a(t)·z) for a known temperature line; two teams per match, mirrored.
    fn held_out(n_games: usize, truth: [f64; 2], seed: u64) -> (Vec<Sample>, Vec<f64>) {
        let mut r = Rng(seed);
        let (mut out, mut z) = (vec![], vec![]);
        for g in 0..n_games {
            for minute in [0.0, 8.0, 14.0, 20.0, 26.0, 32.0] {
                let zi = r.normal() * (0.5 + minute / 15.0);
                let p = sigmoid((truth[0] + truth[1] * calib_time(minute)) * zi);
                let won = r.uniform() < p;
                for (team, w, zz) in [(0u64, won, zi), (1, !won, -zi)] {
                    let situation = Situation { state: GameState { minute, ..Default::default() }, ..Default::default() };
                    out.push(Sample { situation, won: w, is_me: false, version: None, rank: None, group: g as u64 * 2 + team, game: g as u64, player: 0 });
                    z.push(zz);
                }
            }
        }
        (out, z)
    }

    #[test]
    fn calibration_recovers_a_temperature_line_and_ignores_noise() {
        // Under-confident late (the model's logits are too small): found and applied.
        // The two parameters trade off (the start of a game, with small logits, says little), so the check is the
        // temperature the line gives mid-game.
        let (s, z) = held_out(2000, [1.4, 0.5], 7);
        let w = vec![1.0; s.len()];
        let mut m = LogisticModel::default();
        m.fit_calibration_on(&s, &w, &z);
        for minute in [14.0, 26.0] {
            let (got, want) = (m.calibrated(1.0, minute), 1.4 + 0.5 * minute / 10.0);
            assert!((got - want).abs() < 0.1 * want, "{minute} min: {got} vs {want} ({:?})", m.calib);
        }
        // Already calibrated: no significant change, so the identity stays.
        let (s, z) = held_out(2000, IDENTITY_CALIB, 8);
        let mut m = LogisticModel::default();
        m.fit_calibration_on(&s, &w, &z);
        assert_eq!(m.calib, IDENTITY_CALIB);
        // Too few matches: nothing is fitted, whatever the data say.
        let (s, z) = held_out(CALIB_MIN_GAMES - 1, [3.0, 1.0], 9);
        let mut m = LogisticModel::default();
        m.fit_calibration_on(&s, &vec![1.0; s.len()], &z);
        assert_eq!(m.calib, IDENTITY_CALIB);
        // The served logit uses it: (a0 + a1 * t) * z, t capped at 40 minutes.
        let m = LogisticModel { calib: [1.5, 0.25], ..Default::default() };
        assert_eq!(m.calibrated(2.0, 20.0), 4.0);
        assert_eq!(m.calibrated(2.0, 60.0), m.calibrated(2.0, 40.0));
        // Under a pure line, the window factors stay at 1 (nothing significant left over).
        let (s, z) = held_out(2000, [1.4, 0.5], 7);
        let mut m = LogisticModel::default();
        m.fit_calibration_on(&s, &vec![1.0; s.len()], &z);
        assert!(m.calib_win.iter().all(|f| (f - 1.0).abs() < 0.15), "{:?}", m.calib_win);
    }

    /// 1.3.3, a user's pattern: the model needs ×2 from 10 to 30 minutes and ×1 after 30. The line alone overshoots
    /// the late window (it can only keep rising); the window factors bring each window to what it needs.
    #[test]
    fn window_factors_fix_what_a_line_cannot() {
        let mut r = Rng(21);
        let need = |minute: f64| if (10.0..30.0).contains(&minute) { 2.0 } else { 1.0 };
        let (mut s, mut z) = (vec![], vec![]);
        for g in 0..1500u64 {
            for minute in [2.0, 8.0, 14.0, 20.0, 26.0, 32.0, 38.0] {
                let zi = r.normal() * (0.4 + minute / 25.0);
                let won = r.uniform() < sigmoid(need(minute) * zi);
                for (team, w, zz) in [(0u64, won, zi), (1, !won, -zi)] {
                    let situation = Situation { state: GameState { minute, ..Default::default() }, ..Default::default() };
                    s.push(Sample { situation, won: w, is_me: false, version: None, rank: None, group: g * 2 + team, game: g, player: 0 });
                    z.push(zz);
                }
            }
        }
        let w = vec![1.0; s.len()];
        let mut line_only = LogisticModel::default();
        line_only.fit_line(&s, &w, &z);
        let mut m = LogisticModel::default();
        m.fit_calibration_on(&s, &w, &z);
        for (minute, want) in [(14.0, 2.0), (26.0, 2.0), (35.0, 1.0)] {
            let got = m.calibrated(1.0, minute);
            assert!((got - want).abs() < 0.25 * want, "{minute} min: {got} vs {want} (line {:?}, windows {:?})", m.calib, m.calib_win);
        }
        // The line alone overshoots the late window; the factors land at least twice as close to what it needs.
        let (line_err, err) = ((line_only.calibrated(1.0, 35.0) - 1.0).abs(), (m.calibrated(1.0, 35.0) - 1.0).abs());
        assert!(line_err > 0.2 && err < line_err / 2.0, "at 35 min: line alone off by {line_err:.3}, with factors {err:.3}");
        eprintln!("35 min: line alone ×{:.3}, with window factors ×{:.3} (needs ×1); windows {:?}", line_only.calibrated(1.0, 35.0), m.calibrated(1.0, 35.0), m.calib_win);
    }
}
