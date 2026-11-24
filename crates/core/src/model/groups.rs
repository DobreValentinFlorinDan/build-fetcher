//! Port of `server/model/groups.ts`: grouped drafts (0.20.0, an evaluation trial): k-means on the standardised draft
//! contexts, a group's centre standing for its games.

use super::context::{kernel_at, scale_of, standardise, Contexts, N_DRAFT};
use super::dataset::PreparedGame;
use crate::jsmath::imul;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct DraftGroups {
    pub of: Arc<Vec<i32>>,
    pub centroids: Contexts,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GroupingStats {
    pub games: usize,
    pub groups: usize,
    pub spread: f64,
    pub ms: f64,
}

pub fn auto_groups(games: usize) -> usize {
    (crate::js::round(games as f64 / 40.0) as i64).clamp(64, 1024) as usize
}

/// Deterministic random numbers in [0, 1) (mulberry32).
pub struct Rng(u32);
impl Rng {
    pub fn new(seed: f64) -> Rng {
        Rng((seed as i64) as u32)
    }
    pub fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x6d2b79f5);
        let mut t = self.0 as i32;
        t = imul(t ^ ((t as u32) >> 15) as i32, t | 1);
        t ^= t.wrapping_add(imul(t ^ ((t as u32) >> 7) as i32, t | 61));
        ((t ^ ((t as u32) >> 14) as i32) as u32) as f64 / 4294967296.0
    }
}

const ITERATIONS: usize = 8;
const SAMPLE_PER_GROUP: usize = 24;
const SAMPLE_MIN: usize = 20_000;

/// k-means on `n` points of `d` dimensions, `k` centres: each point's centre.
pub fn kmeans(x: &[f64], n: usize, d: usize, k: usize, seed: f64) -> Vec<i32> {
    let mut assign = vec![0i32; n];
    if k <= 1 || n <= 1 {
        return assign;
    }
    let k = k.min(n);
    let mut r = Rng::new(seed);
    let size = n.min(SAMPLE_MIN.max(k * SAMPLE_PER_GROUP));
    let sample: Vec<usize> = if size == n {
        (0..n).collect()
    } else {
        let mut all: Vec<usize> = (0..n).collect();
        for i in 0..size {
            let j = i + (r.next() * (n - i) as f64).floor() as usize;
            all.swap(i, j);
        }
        all[..size].to_vec()
    };
    let mut c = vec![0.0; k * d];
    let mut min_d = vec![f64::INFINITY; sample.len()];
    let mut pick = sample[(r.next() * sample.len() as f64).floor() as usize];
    for j in 0..k {
        c[j * d..j * d + d].copy_from_slice(&x[pick * d..pick * d + d]);
        let mut total = 0.0;
        for i in 0..sample.len() {
            let at = sample[i] * d;
            let mut s = 0.0;
            for t in 0..d {
                let v = x[at + t] - c[j * d + t];
                s += v * v;
            }
            if s < min_d[i] {
                min_d[i] = s;
            }
            total += min_d[i];
        }
        if j + 1 == k {
            break;
        }
        if !(total > 0.0) {
            pick = sample[(r.next() * sample.len() as f64).floor() as usize];
            continue;
        }
        let mut u = r.next() * total;
        let mut i = 0;
        while i < sample.len() - 1 {
            u -= min_d[i];
            if u <= 0.0 {
                break;
            }
            i += 1;
        }
        pick = sample[i];
    }
    let nearest = |c: &[f64], at: usize| -> i32 {
        let mut best = 0;
        let mut best_d = f64::INFINITY;
        for j in 0..k {
            let cj = j * d;
            let mut s = 0.0;
            for t in 0..d {
                let v = x[at + t] - c[cj + t];
                s += v * v;
                if s >= best_d {
                    break;
                }
            }
            if s < best_d {
                best_d = s;
                best = j as i32;
            }
        }
        best
    };
    let mut sums = vec![0.0; k * d];
    let mut counts = vec![0i32; k];
    let mut sample_of = vec![0i32; sample.len()];
    for it in 0..ITERATIONS {
        sums.iter_mut().for_each(|v| *v = 0.0);
        counts.iter_mut().for_each(|v| *v = 0);
        let mut moved = 0;
        for i in 0..sample.len() {
            let at = sample[i] * d;
            let j = nearest(&c, at);
            if it == 0 || sample_of[i] != j {
                moved += 1;
            }
            sample_of[i] = j;
            counts[j as usize] += 1;
            for t in 0..d {
                sums[j as usize * d + t] += x[at + t];
            }
        }
        for j in 0..k {
            if counts[j] > 0 {
                for t in 0..d {
                    c[j * d + t] = sums[j * d + t] / counts[j] as f64;
                }
            }
        }
        if it > 0 && moved == 0 {
            break;
        }
    }
    for i in 0..n {
        assign[i] = nearest(&c, i * d);
    }
    assign
}

/// Puts `games` into about `target` groups by their draft context (writes `g.dg`).
pub fn assign_draft_groups(games: &mut [PreparedGame], target: usize, seed: f64) -> GroupingStats {
    let started = std::time::Instant::now();
    let d_all = N_DRAFT;
    let scale = scale_of(games.iter().map(|g| &g.draft[..]), d_all);
    let z: Vec<Vec<f64>> = games.iter().map(|g| standardise(&g.draft, &scale)).collect();
    let mut by_pattern: Vec<(u32, Vec<usize>)> = Vec::new();
    let mut pat_at: HashMap<u32, usize> = HashMap::new();
    for (i, v) in z.iter().enumerate() {
        let mut mask = 0u32;
        for t in 0..d_all {
            if !v[t].is_nan() {
                mask |= 1 << t;
            }
        }
        let p = *pat_at.entry(mask).or_insert_with(|| {
            by_pattern.push((mask, vec![]));
            by_pattern.len() - 1
        });
        by_pattern[p].1.push(i);
    }
    by_pattern.sort_by_key(|p| p.0);
    let (mut next, mut sq, mut known) = (0usize, 0.0, 0.0);
    let n_games = games.len();
    for (mask, members) in by_pattern {
        let dims: Vec<usize> = (0..d_all).filter(|t| mask & (1 << t) != 0).collect();
        let d = dims.len();
        let k = 1usize.max(members.len().min(crate::js::round((target * members.len()) as f64 / n_games as f64) as usize));
        if d == 0 || k == 1 {
            for &i in &members {
                games[i].dg = Some(next);
            }
            next += 1;
            if d > 0 {
                let mut m = vec![0.0; d];
                for &i in &members {
                    for t in 0..d {
                        m[t] += z[i][dims[t]] / members.len() as f64;
                    }
                }
                for &i in &members {
                    for t in 0..d {
                        let v = z[i][dims[t]] - m[t];
                        sq += v * v;
                        known += 1.0;
                    }
                }
            }
            continue;
        }
        let mut x = vec![0.0; members.len() * d];
        for (r, &i) in members.iter().enumerate() {
            for t in 0..d {
                x[r * d + t] = z[i][dims[t]];
            }
        }
        let assign = kmeans(&x, members.len(), d, k, seed + mask as f64);
        let mut local: HashMap<i32, usize> = HashMap::new();
        let mut sums: HashMap<usize, (Vec<f64>, f64)> = HashMap::new();
        for (r, &i) in members.iter().enumerate() {
            let id = *local.entry(assign[r]).or_insert_with(|| {
                next += 1;
                next - 1
            });
            games[i].dg = Some(id);
            let a = sums.entry(id).or_insert_with(|| (vec![0.0; d], 0.0));
            a.1 += 1.0;
            for t in 0..d {
                a.0[t] += x[r * d + t];
            }
        }
        for (r, &i) in members.iter().enumerate() {
            let a = &sums[&games[i].dg.unwrap()];
            for t in 0..d {
                let v = x[r * d + t] - a.0[t] / a.1;
                sq += v * v;
                known += 1.0;
            }
        }
    }
    GroupingStats { games: n_games, groups: next, spread: if known > 0.0 { (sq / known).sqrt() } else { 0.0 }, ms: started.elapsed().as_secs_f64() * 1000.0 }
}

/// The groups of a draft model whose games all carry one, else None.
pub fn draft_groups_of(games: &[&PreparedGame], z_draft: &Contexts) -> Option<DraftGroups> {
    if games.is_empty() || games.iter().any(|g| g.dg.is_none()) {
        return None;
    }
    let mut local: HashMap<usize, i32> = HashMap::new();
    let mut of = vec![0i32; games.len()];
    for (i, g) in games.iter().enumerate() {
        let n = local.len() as i32;
        of[i] = *local.entry(g.dg.unwrap()).or_insert(n);
    }
    let m = local.len();
    let d = z_draft.d;
    let mut sum = vec![0.0; m * d];
    let mut cnt = vec![0.0; m * d];
    for i in 0..games.len() {
        let at = i * d;
        let to = of[i] as usize * d;
        for t in 0..d {
            let v = z_draft.flat[at + t];
            if !v.is_nan() {
                sum[to + t] += v;
                cnt[to + t] += 1.0;
            }
        }
    }
    let mut flat = vec![0.0; m * d];
    let mut any_nan = false;
    for i in 0..flat.len() {
        flat[i] = if cnt[i] > 0.0 { sum[i] / cnt[i] } else { f64::NAN };
        if cnt[i] == 0.0 {
            any_nan = true;
        }
    }
    Some(DraftGroups { of: Arc::new(of), centroids: Contexts { flat, d, rows: m, any_nan, id: crate::model::context::next_contexts_id() } })
}

/// Each group's similarity to the draft `zq` at bandwidth `h`.
pub fn group_similarities(groups: &DraftGroups, zq: &[f64], h: f64) -> Vec<f64> {
    (0..groups.centroids.rows).map(|j| kernel_at(&groups.centroids, j, zq, h, None)).collect()
}
