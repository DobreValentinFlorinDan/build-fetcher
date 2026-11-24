//! 2.2.0: two candidates for win probability added (WPA), after the methods of Jalovaara (2024), "Win probability
//! estimation for strategic decision-making in esports" (Aalto University): ΔW of an action is w(z) − w(x), the win
//! chance when its effect ends minus when it took effect.
//!
//! - The effective window: an item sold, or consumed into an upgrade, stops counting then. Its ΔW is the win chance at
//!   the state-model frame at or just before it left the inventory, minus the chance just before it was bought. Items
//!   kept to the end keep today's ΔW (the result minus the chance before).
//! - The state model's sample: one frame drawn uniformly at random from each 5-minute interval of each game, instead of
//!   an even stride through every frame.
//!
//! Each is a candidate: the evaluation measures it against today's method on held-out games and the app takes it only
//! when it wins (evaluate.rs, run_eval.rs, service.rs).

use super::state::N_FEATURES;
use crate::capture::{TimelineCapture, BUY, DESTROY, SELL, UNDO};

/// When an item left the inventory (sold or consumed into an upgrade), and the state-model frame then.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowEnd {
    /// When it left (ms).
    pub t: f64,
    /// The frame at or just before `t` (index into the game's frames, which start at 1:00); None: before the first.
    pub frame: Option<usize>,
    /// That frame's state-model features (kept for the scored decisions: core items and boots).
    pub x: Option<Box<[f64; N_FEATURES]>>,
}

/// One purchase of a participant: the item, when, and when it left the inventory (None: kept to the end).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Life {
    pub item: f64,
    pub bought: f64,
    pub end: Option<f64>,
}

/// A destroyed part counts as consumed by an upgrade bought within this many ms of it.
const SAME_MOMENT_MS: f64 = 1000.0;

/// Every purchase `pid` kept (undone ones left out, as the stored purchase list does), in order, with when each left
/// the inventory. A sale or a destroy (a part consumed into an upgrade) ends it; undoing a sale brings it back, and
/// undoing an upgrade brings back the parts it consumed.
pub fn purchase_lives(cap: &TimelineCapture, pid: f64) -> Vec<Life> {
    struct Held {
        life: Life,
        undone: bool,
        sold: bool,
    }
    let mut all: Vec<Held> = Vec::new();
    let mut held: Vec<usize> = Vec::new();
    let mut gone: Vec<usize> = Vec::new();
    let real = |x: f64| x != 0.0 && !x.is_nan();
    for &(t, p, kind, item, after) in &cap.items {
        if p != pid {
            continue;
        }
        if kind == BUY {
            all.push(Held { life: Life { item, bought: t, end: None }, undone: false, sold: false });
            held.push(all.len() - 1);
        } else if kind == SELL || kind == DESTROY {
            if let Some(k) = held.iter().rposition(|i| all[*i].life.item == item) {
                let i = held.remove(k);
                all[i].life.end = Some(t);
                all[i].sold = kind == SELL;
                gone.push(i);
            }
        } else if kind == UNDO {
            if real(item) {
                if let Some(k) = held.iter().rposition(|i| all[*i].life.item == item) {
                    let i = held.remove(k);
                    all[i].undone = true;
                    let at = all[i].life.bought;
                    // The parts that purchase consumed are back.
                    let back: Vec<usize> =
                        gone.iter().copied().filter(|g| !all[*g].sold && all[*g].life.end.map(|e| (e - at).abs() <= SAME_MOMENT_MS).unwrap_or(false)).collect();
                    for g in back {
                        all[g].life.end = None;
                        gone.retain(|x| *x != g);
                        held.push(g);
                    }
                }
            }
            if let Some(a) = after.filter(|a| real(*a)) {
                if let Some(k) = gone.iter().rposition(|i| all[*i].sold && all[*i].life.item == a) {
                    let i = gone.remove(k);
                    all[i].life.end = None;
                    all[i].sold = false;
                    held.push(i);
                }
            }
        }
    }
    all.into_iter().filter(|h| !h.undone).map(|h| h.life).collect()
}

/// The index of the frame at or just before `t` in `frame_t` (ascending); None when every frame is later.
pub fn frame_at_or_before(frame_t: &[f64], t: f64) -> Option<usize> {
    frame_t.partition_point(|x| *x <= t).checked_sub(1)
}

/// When each of `wanted` (item, when bought) left the inventory, from `lives`; each life answers once.
pub fn ends_of(lives: &[Life], wanted: &[(f64, f64)]) -> Vec<Option<f64>> {
    let mut used = vec![false; lives.len()];
    wanted
        .iter()
        .map(|(item, t)| {
            let k = (0..lives.len()).find(|k| !used[*k] && lives[*k].item == *item && lives[*k].bought == *t)?;
            used[k] = true;
            lives[k].end
        })
        .collect()
}

/// ΔW of an item. With the effective window and an end, the chance at the end minus the chance before it was bought;
/// otherwise (or kept to the end) the result minus the chance before.
pub fn item_delta(win: f64, before: f64, end: Option<f64>, window: bool) -> f64 {
    match (window, end) {
        (true, Some(e)) => e - before,
        _ => win - before,
    }
}

// ----------------------------------------------------------------------------------- the state model's sample

/// Which frames the state model is fitted on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StateSample {
    /// Today's: an even stride through every frame of every game, up to the cap.
    #[default]
    Even,
    /// One frame drawn uniformly at random from each 5-minute interval of each game (a fixed seed), up to the cap.
    Interval,
}

impl StateSample {
    /// Its name in the report and latest.json.
    pub fn name(self) -> &'static str {
        match self {
            StateSample::Even => "even",
            StateSample::Interval => "5min",
        }
    }
    pub fn from_name(s: Option<&str>) -> StateSample {
        if s == Some("5min") {
            StateSample::Interval
        } else {
            StateSample::Even
        }
    }
    /// How the log says it.
    pub fn label(self) -> &'static str {
        match self {
            StateSample::Even => "an even stride through every frame",
            StateSample::Interval => "one frame per 5 minutes",
        }
    }
}

pub const SAMPLE_INTERVAL_MS: f64 = 5.0 * 60_000.0;
/// The sampler's seed: the same frames in the evaluation and in the app, on any number of threads.
pub const SAMPLE_SEED: u32 = 1;

fn mulberry32(seed: u32) -> impl FnMut() -> f64 {
    let mut a = seed;
    move || {
        a = a.wrapping_add(0x6d2b_79f5);
        let mut t = a;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        (t ^ (t >> 14)) as f64 / 4_294_967_296.0
    }
}

/// One frame drawn uniformly at random from each 5-minute interval of the game (by its frames' times, ascending).
/// Seeded by the game, so a game gives the same frames whatever the other games, their order or the threads.
pub fn interval_frames(frame_t: &[f64], game: &str, seed: u32) -> Vec<usize> {
    let mut r = mulberry32(super::expected::fnv(&format!("{seed}:{game}")));
    let mut out = Vec::new();
    let mut k = 0;
    while k < frame_t.len() {
        let bin = (frame_t[k] / SAMPLE_INTERVAL_MS).floor();
        let mut e = k + 1;
        while e < frame_t.len() && (frame_t[e] / SAMPLE_INTERVAL_MS).floor() == bin {
            e += 1;
        }
        out.push(k + ((r() * (e - k) as f64).floor() as usize).min(e - k - 1));
        k = e;
    }
    out
}

/// Expected calibration error: the share-weighted gap between the observed and the predicted rate over `bins` equal
/// bins of the prediction.
pub fn ece(p: &[f64], y: &[f64], bins: usize) -> f64 {
    if p.is_empty() {
        return f64::NAN;
    }
    let rows = super::logistic::calibration(p, y, bins);
    rows.iter().map(|r| r.n * (r.observed - r.predicted).abs()).sum::<f64>() / p.len() as f64
}
