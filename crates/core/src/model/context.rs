//! Port of `server/model/context.ts`: the draft context (B3) and the live context at a purchase (B5), their scale
//! and the Gaussian kernel the draft model weighs stored games with.

use super::logistic::{js_max, js_min};
use super::profiles::{team_magic_share, ChampionProfile, ChampionProfiles};
use crate::ddragon::ItemMeta;
use crate::js;
use crate::jsmath;
use std::sync::Arc;

pub struct Dim {
    pub key: &'static str,
    pub label: &'static str,
    pub format: fn(f64) -> String,
}

fn pct(v: f64) -> String {
    format!("{}%", js::num_to_string(js::round(v * 100.0)))
}
fn per_min(v: f64) -> String {
    format!("{}/min", js::thousands(js::round(v)))
}
fn one(v: f64) -> String {
    js::num_to_string(js::round(v * 10.0) / 10.0)
}
fn gold(v: f64) -> String {
    format!("{}{}k", if v >= 0.0 { "+" } else { "" }, js::to_fixed(js::round(v * 10.0) / 10.0, 1))
}
fn signed(v: f64) -> String {
    format!("{}{}", if v >= 0.0 { "+" } else { "" }, js::num_to_string(js::round(v)))
}
fn cc(v: f64) -> String {
    format!("{} s/min", one(v))
}
fn ranged(v: f64) -> String {
    if v >= 0.5 { "ranged".into() } else { "melee".into() }
}
fn minute(v: f64) -> String {
    format!("{} min", js::num_to_string(js::round(v)))
}

pub const DRAFT_DIMS: [Dim; 12] = [
    Dim { key: "eMagic", label: "enemy magic damage", format: pct },
    Dim { key: "eHeal", label: "enemy healing", format: per_min },
    Dim { key: "eShield", label: "enemy shielding", format: per_min },
    Dim { key: "eCc", label: "enemy crowd control", format: cc },
    Dim { key: "eTank", label: "enemy tankiness", format: per_min },
    Dim { key: "eRanged", label: "ranged enemies", format: pct },
    Dim { key: "aMagic", label: "ally magic damage", format: pct },
    Dim { key: "aTank", label: "ally frontline", format: per_min },
    Dim { key: "oMagic", label: "lane opponent magic damage", format: pct },
    Dim { key: "oTank", label: "lane opponent tankiness", format: per_min },
    Dim { key: "oRanged", label: "lane opponent", format: ranged },
    Dim { key: "oHeal", label: "lane opponent healing", format: per_min },
];

pub const LIVE_DIMS: [Dim; 8] = [
    Dim { key: "eMagicThreat", label: "magic damage of the enemies ahead", format: pct },
    Dim { key: "eArmor", label: "enemy armour from items", format: one },
    Dim { key: "eMr", label: "enemy magic resist from items", format: one },
    Dim { key: "eAntiHeal", label: "enemies with anti-heal", format: one },
    Dim { key: "laneGold", label: "lane item gold", format: gold },
    Dim { key: "teamGold", label: "team item gold", format: gold },
    Dim { key: "kills", label: "kill difference", format: signed },
    Dim { key: "minute", label: "game time", format: minute },
];

/// DRAFT_DIMS then LIVE_DIMS.
pub fn item_dim(i: usize) -> &'static Dim {
    if i < 12 {
        &DRAFT_DIMS[i]
    } else {
        &LIVE_DIMS[i - 12]
    }
}
pub const N_DRAFT: usize = 12;
pub const N_LIVE: usize = 8;
pub const N_ITEM: usize = 20;

pub fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return f64::NAN;
    }
    let mut s = 0.0;
    for x in xs {
        s += x;
    }
    s / xs.len() as f64
}

pub struct ChampRef<'a> {
    pub id: i64,
    pub position: Option<&'a str>,
}

/// The draft context: enemy and ally team profiles, and the lane opponent's. Unknown parts are NaN.
pub fn draft_context(profiles: &ChampionProfiles, enemies: &[ChampRef], allies: &[ChampRef], opponent: Option<ChampRef>) -> Vec<f64> {
    let e: Vec<Arc<ChampionProfile>> = enemies.iter().map(|c| profiles.get(c.id, c.position)).collect();
    let a: Vec<Arc<ChampionProfile>> = allies.iter().map(|c| profiles.get(c.id, c.position)).collect();
    let o = opponent.map(|c| profiles.get(c.id, c.position));
    let if_any = |ps: &[Arc<ChampionProfile>], f: &dyn Fn(&[Arc<ChampionProfile>]) -> f64| if ps.is_empty() { f64::NAN } else { f(ps) };
    let m = |ps: &[Arc<ChampionProfile>], g: fn(&ChampionProfile) -> f64| mean(&ps.iter().map(|p| g(p)).collect::<Vec<_>>());
    vec![
        if_any(&e, &|ps| team_magic_share(ps, None)),
        if_any(&e, &|ps| m(ps, |p| p.heal_per_min + p.ally_heal_per_min)),
        if_any(&e, &|ps| m(ps, |p| p.shield_per_min)),
        if_any(&e, &|ps| m(ps, |p| p.cc_per_min)),
        if_any(&e, &|ps| m(ps, |p| p.tank_per_min)),
        if_any(&e, &|ps| m(ps, |p| if p.ranged { 1.0 } else { 0.0 })),
        if_any(&a, &|ps| team_magic_share(ps, None)),
        if_any(&a, &|ps| m(ps, |p| p.tank_per_min)),
        o.as_ref().map(|o| o.magic_share).unwrap_or(f64::NAN),
        o.as_ref().map(|o| o.tank_per_min).unwrap_or(f64::NAN),
        o.as_ref().map(|o| if o.ranged { 1.0 } else { 0.0 }).unwrap_or(f64::NAN),
        o.as_ref().map(|o| o.heal_per_min + o.ally_heal_per_min).unwrap_or(f64::NAN),
    ]
}

pub struct EnemyNow<'a> {
    pub profile: Arc<ChampionProfile>,
    pub items: &'a [f64],
}

pub struct LiveSituation {
    pub lane_gold: f64,
    pub team_gold: f64,
    pub kills: f64,
    pub minute: f64,
}

/// The live context when an item is bought.
pub fn live_context<'d>(enemies: &[EnemyNow], item_of: &dyn Fn(f64) -> Option<&'d ItemMeta>, o: &LiveSituation) -> Vec<f64> {
    let value = |items: &[f64]| {
        let mut s = 0.0;
        for id in items {
            s += item_of(*id).map(|i| i.gold_total).unwrap_or(0.0);
        }
        s
    };
    let values: Vec<f64> = enemies.iter().map(|e| value(e.items)).collect();
    let avg = {
        let m = mean(&values);
        if m == 0.0 || m.is_nan() {
            1.0
        } else {
            m
        }
    };
    let threat: Vec<f64> = values.iter().map(|v| js_max(0.25, v / avg)).collect();
    let sum = |items: &[f64], f: fn(&ItemMeta) -> f64| {
        let mut s = 0.0;
        for id in items {
            s += item_of(*id).map(f).unwrap_or(0.0);
        }
        s
    };
    let n = !enemies.is_empty();
    vec![
        if n { team_magic_share(&enemies.iter().map(|e| e.profile.clone()).collect::<Vec<_>>(), Some(&threat)) } else { f64::NAN },
        if n { mean(&enemies.iter().map(|e| sum(e.items, |it| it.armor)).collect::<Vec<_>>()) } else { f64::NAN },
        if n { mean(&enemies.iter().map(|e| sum(e.items, |it| it.magic_resist)).collect::<Vec<_>>()) } else { f64::NAN },
        if n { enemies.iter().filter(|e| e.items.iter().any(|id| item_of(*id).map(|i| i.anti_heal).unwrap_or(false))).count() as f64 } else { f64::NAN },
        o.lane_gold / 1000.0,
        o.team_gold / 1000.0,
        o.kills,
        o.minute,
    ]
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Scale {
    pub mean: Vec<f64>,
    pub sd: Vec<f64>,
}

pub fn scale_of<'a>(vectors: impl Iterator<Item = &'a [f64]> + Clone, dims: usize) -> Scale {
    let mut m = vec![0.0; dims];
    let mut s = vec![0.0; dims];
    let mut n = vec![0.0; dims];
    for v in vectors.clone() {
        for i in 0..dims {
            let x = v.get(i).copied().unwrap_or(f64::NAN);
            if x.is_finite() {
                m[i] += x;
                n[i] += 1.0;
            }
        }
    }
    for i in 0..dims {
        m[i] = if n[i] != 0.0 { m[i] / n[i] } else { 0.0 };
    }
    for v in vectors {
        for i in 0..dims {
            let x = v.get(i).copied().unwrap_or(f64::NAN);
            if x.is_finite() {
                s[i] += (x - m[i]) * (x - m[i]);
            }
        }
    }
    let sd = s
        .iter()
        .enumerate()
        .map(|(i, x)| {
            let sd = if n[i] > 1.0 { (x / (n[i] - 1.0)).sqrt() } else { 0.0 };
            if sd > 1e-4 * (1.0 + m[i].abs()) {
                sd
            } else {
                f64::INFINITY
            }
        })
        .collect();
    Scale { mean: m, sd }
}

#[inline]
pub fn standard_one(x: f64, sc: &Scale, i: usize) -> f64 {
    let sd = sc.sd.get(i).copied().unwrap_or(f64::NAN);
    if x.is_finite() && sd.is_finite() {
        js_max(-4.0, js_min(4.0, (x - sc.mean[i]) / sd))
    } else {
        f64::NAN
    }
}

pub fn standardise(v: &[f64], sc: &Scale) -> Vec<f64> {
    v.iter().enumerate().map(|(i, x)| standard_one(*x, sc, i)).collect()
}

/// Standardised contexts of one width, one row after another in one array.
#[derive(Clone, Debug, Default)]
pub struct Contexts {
    pub flat: Vec<f64>,
    pub d: usize,
    pub rows: usize,
    pub any_nan: bool,
    /// Which contexts these are, for a copy kept elsewhere (Use GPU's on the card); 0: none.
    pub id: u64,
}

static NEXT_CONTEXTS_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A new Contexts::id.
pub fn next_contexts_id() -> u64 {
    NEXT_CONTEXTS_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Contexts {
    pub fn of(rows: usize, d: usize, mut fill: impl FnMut(&mut [f64], usize, usize)) -> Contexts {
        let mut flat = vec![f64::NAN; rows * d];
        for r in 0..rows {
            fill(&mut flat, r, r * d);
        }
        let any_nan = flat.iter().any(|x| x.is_nan());
        Contexts { flat, d, rows, any_nan, id: next_contexts_id() }
    }

    /// Contexts from arrays: longer ones cut to `d`, shorter ones NaN-padded.
    pub fn from_rows(zs: &[Vec<f64>], d: Option<usize>) -> Contexts {
        let d = d.unwrap_or_else(|| zs.first().map(|z| z.len()).unwrap_or(0));
        Contexts::of(zs.len(), d, |out, r, at| {
            for (j, x) in zs[r].iter().enumerate().take(d) {
                out[at + j] = *x;
            }
        })
    }

    pub fn row(&self, r: usize) -> &[f64] {
        &self.flat[r * self.d..(r + 1) * self.d]
    }
}

/// standardise(v, sc) written into `out` from `at` (at most `d` values).
pub fn standardise_into(v: &[f64], sc: &Scale, out: &mut [f64], at: usize, d: usize) {
    for i in 0..v.len().min(d) {
        out[at + i] = standard_one(v[i], sc, i);
    }
}

/// Similarity of a stored game's context to the query's (Gaussian kernel on the mean squared standardised distance
/// over the dimensions both know). `only` uses one dimension alone. 1 when none is known.
#[inline]
pub fn kernel(game: &[f64], query: &[f64], h: f64, only: Option<usize>) -> f64 {
    if !h.is_finite() {
        return 1.0;
    }
    let mut d2 = 0.0;
    let mut n = 0.0;
    let (from, to) = match only {
        Some(o) => (o, o + 1),
        None => (0, query.len()),
    };
    for i in from..to {
        let q = query[i];
        let g = game.get(i).copied().unwrap_or(f64::NAN);
        if q.is_nan() || g.is_nan() {
            continue;
        }
        d2 += (g - q) * (g - q);
        n += 1.0;
    }
    if n != 0.0 {
        jsmath::exp(-(d2 / n) / (2.0 * h * h))
    } else {
        1.0
    }
}

/// kernel() for row `row` of `c`.
#[inline]
pub fn kernel_at(c: &Contexts, row: usize, query: &[f64], h: f64, only: Option<usize>) -> f64 {
    if !h.is_finite() {
        return 1.0;
    }
    let at = row * c.d;
    let mut d2 = 0.0;
    let mut n = 0.0;
    let (from, to) = match only {
        Some(o) => (o, o + 1),
        None => (0, query.len()),
    };
    let to = to.min(c.d);
    for i in from..to {
        let q = query[i];
        let g = c.flat[at + i];
        if q.is_nan() || g.is_nan() {
            continue;
        }
        d2 += (g - q) * (g - q);
        n += 1.0;
    }
    if n != 0.0 {
        jsmath::exp(-(d2 / n) / (2.0 * h * h))
    } else {
        1.0
    }
}
