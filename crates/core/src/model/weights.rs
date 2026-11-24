//! Port of `server/model/weights.ts`: how much each stored game counts in the draft model.

use super::dataset::PreparedGame;
use super::estimate::Observation;
use super::logistic::{js_max, js_min};
use crate::ddragon::{to_patch, Fingerprints, StaticData};
use crate::engine::recommend::js_order;
use crate::jsmath::pow;
use crate::scope::ManualHotfix;
use indexmap::IndexMap;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub struct Sources {
    pub otp: f64,
    pub main: f64,
    pub harvested: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Memory {
    pub decay: f64,
    pub patches: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WeightOptions {
    pub sources: Sources,
    pub pilot_cap: f64,
    pub region_weights: Vec<(String, f64)>,
    pub regions: Option<Vec<String>>,
    pub own_platform: Option<String>,
    pub own_region_boost: f64,
    pub patch_weights: Vec<(String, f64)>,
    pub hotfixes: Vec<ManualHotfix>,
    pub before_hotfix_weight: f64,
    pub memory: Option<Memory>,
    pub pilot_strength: Option<f64>,
    pub recency_days: Option<f64>,
}

pub fn weight_defaults() -> WeightOptions {
    WeightOptions {
        sources: Sources { otp: 1.0, main: 0.8, harvested: 0.6 },
        pilot_cap: 0.05,
        region_weights: vec![],
        regions: None,
        own_platform: None,
        own_region_boost: 1.5,
        patch_weights: vec![],
        hotfixes: vec![],
        before_hotfix_weight: 0.5,
        memory: None,
        pilot_strength: None,
        recency_days: None,
    }
}

pub const MEMORY_PATCHES: f64 = 6.0;
pub const HISTORY_PATCHES: usize = 7;

pub const PREV_UNCHANGED: f64 = 0.8;
pub const PREV_CHANGED: f64 = 0.2;
pub const PREV_UNKNOWN: f64 = 0.35;

fn tier_weight(t: &str) -> Option<f64> {
    Some(match t {
        "CHALLENGER" | "GRANDMASTER" => 1.0,
        "MASTER" => 0.9,
        "DIAMOND" => 0.75,
        "EMERALD" => 0.6,
        "PLATINUM" => 0.5,
        _ => return None,
    })
}

fn is_apex(t: &str) -> bool {
    matches!(t, "CHALLENGER" | "GRANDMASTER" | "MASTER")
}

/// Who played the game: source × tier (unknown 0.5) × Irelia points × LP within the apex tiers.
pub fn pilot_weight(g: &PreparedGame, o: &WeightOptions) -> f64 {
    if g.source == "me" {
        return 0.0;
    }
    let source = match g.source.as_str() {
        "otp" => o.sources.otp,
        "main" => o.sources.main,
        "harvested" => o.sources.harvested,
        _ => 0.0,
    };
    let tier = match g.tier.as_deref().filter(|t| !t.is_empty()) {
        Some(t) if t != "UNKNOWN" => tier_weight(t).unwrap_or(0.4),
        _ => 0.5,
    };
    let points = match g.mastery {
        None => 0.9,
        Some(m) => 0.85 + 0.15 * js_min(1.0, m / 500_000.0),
    };
    let lp = match (&g.tier, g.lp) {
        (Some(t), Some(lp)) if !t.is_empty() && is_apex(t) => 0.95 + 0.05 * js_min(1.0, lp / 1000.0),
        _ => 1.0,
    };
    let w = source * tier * points * lp;
    let strength = o.pilot_strength.unwrap_or(1.0);
    if strength == 1.0 || w <= 0.0 {
        w
    } else {
        pow(w, strength)
    }
}

const DAY_MS: f64 = 86_400_000.0;

/// Fine-tuning's fade within a patch.
pub fn recency_weigher<'a>(newest: f64, recency_days: Option<f64>, age_of: &'a (dyn Fn(&PreparedGame) -> Option<f64> + Sync)) -> Box<dyn Fn(&PreparedGame) -> f64 + Sync + 'a> {
    match recency_days {
        Some(days) if days != 0.0 && days > 0.0 => Box::new(move |g: &PreparedGame| {
            if age_of(g) == Some(0.0) {
                pow(0.5, js_max(0.0, newest - g.created) / (days * DAY_MS))
            } else {
                1.0
            }
        }),
        _ => Box::new(|_| 1.0),
    }
}

pub struct PatchChanges {
    pub irelia_changed: Option<bool>,
    prev: Option<Fingerprints>,
    now: Fingerprints,
}

impl PatchChanges {
    pub fn item_changed(&self, id: i64) -> Option<bool> {
        self.prev.as_ref().map(|p| p.items.get(&id) != self.now.items.get(&id))
    }
}

pub fn patch_changes(data: &StaticData) -> PatchChanges {
    let prev = data.previous.as_ref().map(|(_, f)| f.clone());
    PatchChanges {
        irelia_changed: prev.as_ref().map(|p| p.champion != data.fingerprints.champion),
        prev,
        now: data.fingerprints.clone(),
    }
}

/// Patches newest first, from Data Dragon's version list.
pub fn patch_order(data: &StaticData) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let versions = data.versions.clone().unwrap_or_else(|| vec![data.version.clone()]);
    for v in versions {
        let p = to_patch(&v);
        if !seen.contains(&p) {
            seen.push(p);
        }
    }
    seen
}

/// How many patches `patch` is before `current` (0: the same one); None when it can't be told.
pub fn patch_age(order: &[String], patch: &str, current: &str) -> Option<f64> {
    let a = order.iter().position(|p| p == patch);
    let b = order.iter().position(|p| p == current);
    if let (Some(a), Some(b)) = (a, b) {
        return Some(a as f64 - b as f64);
    }
    let parts = |s: &str| {
        let mut it = s.split('.').map(|x| if x.is_empty() { 0.0 } else { x.parse::<f64>().unwrap_or(f64::NAN) });
        (it.next().unwrap_or(f64::NAN), it.next().unwrap_or(f64::NAN))
    };
    let (ma, na) = parts(patch);
    let (mb, nb) = parts(current);
    if ma == mb && na.is_finite() && nb.is_finite() {
        Some(nb - na)
    } else {
        None
    }
}

/// patchAge over one patch order, each pair worked out once.
pub struct PatchAges {
    order: Vec<String>,
    memo: Mutex<HashMap<(String, String), Option<f64>>>,
}

impl PatchAges {
    pub fn new(order: Vec<String>) -> PatchAges {
        PatchAges { order, memo: Mutex::new(HashMap::new()) }
    }
    pub fn age(&self, patch: &str, current: &str) -> Option<f64> {
        let key = (patch.to_string(), current.to_string());
        if let Some(a) = self.memo.lock().get(&key) {
            return *a;
        }
        let a = patch_age(&self.order, patch, current);
        self.memo.lock().insert(key, a);
        a
    }
}

/// A platform's current and previous patch, or Data Dragon's pair.
pub type RegionPatchFn = Arc<dyn Fn(&str) -> Option<(String, String)> + Send + Sync>;

fn rp_of(data: &StaticData, rp: &RegionPatchFn, platform: &str) -> (String, String) {
    rp(platform).unwrap_or_else(|| (data.patch.clone(), data.previous_patch.clone()))
}

/// A game's age in patches on its region (0: its current patch); unknown counts as too old (∞).
pub struct PatchAger {
    data: Arc<StaticData>,
    rp: RegionPatchFn,
    ages: PatchAges,
}

impl PatchAger {
    pub fn new(data: Arc<StaticData>, rp: RegionPatchFn) -> PatchAger {
        let order = patch_order(&data);
        PatchAger { data, rp, ages: PatchAges::new(order) }
    }
    pub fn age(&self, platform: &str, patch: &str) -> f64 {
        let (p, prev) = rp_of(&self.data, &self.rp, platform);
        if patch == p {
            return 0.0;
        }
        if patch == prev && prev != p {
            return 1.0;
        }
        self.ages.age(patch, &p).unwrap_or(f64::INFINITY)
    }
}

/// Patch weight of a game for a decision involving `items`.
pub struct PatchWeigher {
    data: Arc<StaticData>,
    rp: RegionPatchFn,
    o: WeightOptions,
    changes: PatchChanges,
    ages: PatchAges,
    latest_hotfix: HashMap<String, f64>,
}

impl PatchWeigher {
    pub fn new(data: Arc<StaticData>, rp: RegionPatchFn, o: &WeightOptions) -> PatchWeigher {
        let changes = patch_changes(&data);
        let ages = PatchAges::new(if o.memory.is_some() { patch_order(&data) } else { vec![] });
        let mut latest_hotfix: HashMap<String, f64> = HashMap::new();
        for h in &o.hotfixes {
            if let Some(at) = crate::js::date_parse(&h.date) {
                let cur = latest_hotfix.get(&h.patch).copied().unwrap_or(0.0);
                latest_hotfix.insert(h.patch.clone(), js_max(cur, at));
            }
        }
        PatchWeigher { data, rp, o: o.clone(), changes, ages, latest_hotfix }
    }

    fn fingerprints_of(&self, patch: &str) -> Option<&Fingerprints> {
        if patch == self.data.patch {
            Some(&self.data.fingerprints)
        } else {
            self.data.history.as_ref().and_then(|h| h.get(patch))
        }
    }

    fn changed_since(&self, from: &str, to: &str, items: &[i64]) -> Option<bool> {
        let a = self.fingerprints_of(from)?;
        let b = self.fingerprints_of(to)?;
        Some(a.champion != b.champion || items.iter().any(|id| a.items.get(id) != b.items.get(id)))
    }

    fn override_of(&self, patch: &str) -> Option<f64> {
        self.o.patch_weights.iter().find(|(k, _)| k == patch).map(|(_, w)| *w)
    }

    pub fn weigh(&self, g: &PreparedGame, items: &[i64]) -> f64 {
        let (p, prev) = rp_of(&self.data, &self.rp, &g.platform);
        if g.patch == p {
            let hotfix = self.latest_hotfix.get(&g.patch).copied();
            return match hotfix {
                Some(h) if h != 0.0 && g.created < h => self.o.before_hotfix_weight,
                _ => 1.0,
            };
        }
        if g.patch == prev && prev != p {
            if let Some(w) = self.override_of(&g.patch) {
                return w;
            }
            if g.patch != self.data.previous_patch || self.changes.irelia_changed.is_none() {
                return PREV_UNKNOWN;
            }
            let changed = self.changes.irelia_changed == Some(true) || items.iter().any(|id| self.changes.item_changed(*id) != Some(false));
            return if changed { PREV_CHANGED } else { PREV_UNCHANGED };
        }
        let Some(mem) = &self.o.memory else { return 0.0 };
        let age = self.ages.age(&g.patch, &p);
        let Some(age) = age else { return 0.0 };
        if age < 2.0 || age >= mem.patches {
            return 0.0;
        }
        let fade = pow(mem.decay, age - 1.0);
        if let Some(w) = self.override_of(&g.patch) {
            return w * fade;
        }
        let changed = self.changed_since(&g.patch, &p, items);
        fade * match changed {
            None => PREV_UNKNOWN,
            Some(true) => PREV_CHANGED,
            Some(false) => PREV_UNCHANGED,
        }
    }
}

/// Patch memory's data check (see weights.ts consistent).
pub fn consistent(obs: Vec<Observation>, min_games: f64) -> (Vec<Observation>, bool) {
    #[derive(Default, Clone, Copy)]
    struct Side {
        sw: f64,
        sw2: f64,
        swx: f64,
    }
    let mut groups: IndexMap<String, (Side, Side)> = IndexMap::new();
    let (mut sq, mut sq_w) = (0.0, 0.0);
    for ob in &obs {
        if ob.w <= 0.0 {
            continue;
        }
        let key = format!("{}|{}", ob.lane, ob.option);
        let g = groups.entry(key).or_default();
        let side = if ob.age.unwrap_or(0.0) == 0.0 { &mut g.0 } else { &mut g.1 };
        side.sw += ob.w;
        side.sw2 += ob.w * ob.w;
        side.swx += ob.w * ob.wpa;
        sq += ob.w * ob.wpa * ob.wpa;
        sq_w += ob.w;
    }
    let sigma = js_max(0.3, if sq_w > 0.0 { (sq / sq_w).sqrt() } else { 0.5 });
    let mut factor: HashMap<String, f64> = HashMap::new();
    for (key, (now, old)) in &groups {
        let n_now = if now.sw2 > 0.0 { now.sw * now.sw / now.sw2 } else { 0.0 };
        let n_old = if old.sw2 > 0.0 { old.sw * old.sw / old.sw2 } else { 0.0 };
        if n_now < min_games || n_old < min_games {
            continue;
        }
        let z = (now.swx / now.sw - old.swx / old.sw) / (sigma * (1.0 / n_now + 1.0 / n_old).sqrt());
        if z.abs() > 2.0 {
            factor.insert(key.clone(), 4.0 / (z * z));
        }
    }
    if factor.is_empty() {
        return (obs, false);
    }
    let out = obs
        .into_iter()
        .map(|mut ob| {
            if ob.age.unwrap_or(0.0) > 0.0 {
                if let Some(f) = factor.get(&format!("{}|{}", ob.lane, ob.option)) {
                    ob.w *= f;
                }
            }
            ob
        })
        .collect();
    (out, true)
}

pub fn region_weight(g: &PreparedGame, o: &WeightOptions) -> f64 {
    if let Some(r) = &o.regions {
        if !r.contains(&g.platform) {
            return 0.0;
        }
    }
    let base = o.region_weights.iter().find(|(k, _)| *k == g.platform).map(|(_, w)| *w).unwrap_or(1.0);
    base * if o.own_platform.as_deref().map(|p| !p.is_empty() && g.platform == p).unwrap_or(false) { o.own_region_boost } else { 1.0 }
}

/// Per-pilot factors so no pilot holds more than `cap` of a lane's weight (water-filling). Keyed `${lane}:${pilot}`.
pub fn pilot_cap_factors(games: &[(&str, &str, f64)], cap: f64) -> HashMap<String, f64> {
    let mut factors = HashMap::new();
    let mut lanes: Vec<&str> = Vec::new();
    for g in games {
        if !lanes.contains(&g.1) {
            lanes.push(g.1);
        }
    }
    for lane in lanes {
        let mut totals: IndexMap<&str, f64> = IndexMap::new();
        for g in games {
            if g.1 == lane && g.2 > 0.0 {
                *totals.entry(g.0).or_insert(0.0) += g.2;
            }
        }
        if totals.is_empty() {
            continue;
        }
        let limit = js_max(cap, 1.0 / totals.len() as f64);
        let mut sorted: Vec<(&str, f64)> = totals.into_iter().collect();
        sorted.sort_by(|a, b| js_order(b.1 - a.1));
        let mut rest = 0.0;
        for (_, w) in &sorted {
            rest += w;
        }
        let mut level = f64::INFINITY;
        for capped in 0..sorted.len() {
            let c = (limit * rest) / (1.0 - limit * capped as f64);
            if sorted[capped].1 <= c {
                level = c;
                break;
            }
            rest -= sorted[capped].1;
        }
        for (p, w) in sorted {
            factors.insert(format!("{lane}:{p}"), if w > level { level / w } else { 1.0 });
        }
    }
    factors
}
