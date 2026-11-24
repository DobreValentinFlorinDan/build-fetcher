//! Port of `server/engine/recommend.ts`: the lane baseline build (weighted tallies ranked by the Wilson lower bound).

use super::exclusivity::ExclusivityIndex;
use super::validator::{validate_game, ParsedGame, Verdict};
use crate::db::IreliaGameRow;
use crate::ddragon::{compare_patch, patch_ordering, StaticData};
use crate::js;
use crate::riot::routing::platform_of_match;
use crate::types::*;
use indexmap::IndexMap;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub struct EngineSettings {
    pub min_matchup_games: f64,
    pub general_prior_weight: f64,
    pub min_lane_games: f64,
    pub other_lane_weight: f64,
    pub previous_patch_weight: f64,
    pub min_current_patch_games: f64,
    pub starting_item_window_ms: f64,
}

impl Default for EngineSettings {
    fn default() -> Self {
        use crate::config::*;
        EngineSettings {
            min_matchup_games: MIN_MATCHUP_GAMES,
            general_prior_weight: GENERAL_PRIOR_WEIGHT,
            min_lane_games: MIN_LANE_GAMES,
            other_lane_weight: OTHER_LANE_WEIGHT,
            previous_patch_weight: PREVIOUS_PATCH_WEIGHT,
            min_current_patch_games: MIN_CURRENT_PATCH_GAMES,
            starting_item_window_ms: STARTING_ITEM_WINDOW_MS,
        }
    }
}

impl EngineSettings {
    /// The settings as JSON, in config.ts's key order (the evaluation's key reads them).
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "minMatchupGames": js::num(self.min_matchup_games),
            "generalPriorWeight": js::num(self.general_prior_weight),
            "minLaneGames": js::num(self.min_lane_games),
            "otherLaneWeight": js::num(self.other_lane_weight),
            "previousPatchWeight": js::num(self.previous_patch_weight),
            "minCurrentPatchGames": js::num(self.min_current_patch_games),
            "startingItemWindowMs": js::num(self.starting_item_window_ms),
        })
    }
}

/// The weighted median of whole numbers; None without data.
fn weighted_median(xs: &[(f64, f64)]) -> Option<f64> {
    let total: f64 = xs.iter().map(|x| x.1).sum();
    if total == 0.0 || total.is_nan() {
        return None;
    }
    let mut s = xs.to_vec();
    s.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut acc = 0.0;
    for x in s {
        acc += x.1;
        if acc >= total / 2.0 {
            return Some(x.0);
        }
    }
    None
}

/// Wilson score lower bound on (weighted) win rate.
pub fn wilson(wins: f64, n: f64, z: f64) -> f64 {
    if n <= 0.0 {
        return 0.0;
    }
    let p = wins / n;
    let denom = 1.0 + (z * z) / n;
    let centre = p + (z * z) / (2.0 * n);
    let margin = z * ((p * (1.0 - p)) / n + (z * z) / (4.0 * n * n)).sqrt();
    (centre - margin) / denom
}

/// A JS comparator result (`a - b || …`) as an Ordering for a stable sort.
pub fn js_order(d: f64) -> std::cmp::Ordering {
    if d < 0.0 {
        std::cmp::Ordering::Less
    } else if d > 0.0 {
        std::cmp::Ordering::Greater
    } else {
        std::cmp::Ordering::Equal
    }
}
/// `x || y` for comparator differences (0 and NaN fall through).
pub fn or_else(x: f64, y: impl FnOnce() -> f64) -> f64 {
    if x != 0.0 && !x.is_nan() {
        x
    } else {
        y()
    }
}

struct Entry<T> {
    value: T,
    w: f64,
    wins: f64,
    n: f64,
}

pub struct Tally<T: Clone> {
    m: IndexMap<String, Entry<T>>,
}

impl<T: Clone> Tally<T> {
    pub fn new() -> Self {
        Tally { m: IndexMap::new() }
    }
    pub fn add(&mut self, value: T, key: String, weight: f64, win: bool) {
        let e = self.m.entry(key).or_insert(Entry { value, w: 0.0, wins: 0.0, n: 0.0 });
        e.w += weight;
        e.n += 1.0;
        if win {
            e.wins += weight;
        }
    }
    /// Options ranked by Wilson score, ignoring ones under `min_share` of the total weight (noise).
    pub fn options(&self, min_share: f64, mut filter: impl FnMut(&T) -> bool) -> Vec<Opt<T>> {
        let total: f64 = self.m.values().map(|e| e.w).sum();
        let mut out: Vec<Opt<T>> = self
            .m
            .values()
            .filter(|e| e.w >= (0.5f64).max(total * min_share) && filter(&e.value))
            .map(|e| Opt { value: e.value.clone(), games: round2(e.w), raw_games: e.n, win_rate: e.wins / e.w, score: wilson(e.wins, e.w, 1.0) })
            .collect();
        out.sort_by(|a, b| js_order(or_else(b.score - a.score, || b.games - a.games)));
        out
    }
}

impl<T: Clone> Default for Tally<T> {
    fn default() -> Self {
        Self::new()
    }
}

pub fn round2(x: f64) -> f64 {
    js::round(x * 100.0) / 100.0
}

/// Q, W and E in the order they are maxed along a skill path (ties: more points first).
pub fn derive_priority(path: &[String]) -> Vec<String> {
    let mut counts: HashMap<&str, f64> = [("Q", 0.0), ("W", 0.0), ("E", 0.0)].into_iter().collect();
    let mut maxed: HashMap<&str, f64> = [("Q", 99.0), ("W", 99.0), ("E", 99.0)].into_iter().collect();
    for (i, s) in path.iter().enumerate() {
        if s != "R" {
            let c = counts.entry(s.as_str()).or_insert(f64::NAN);
            *c += 1.0;
            if *c == 5.0 {
                maxed.insert(s.as_str(), i as f64);
            }
        }
    }
    let mut out = vec!["Q", "W", "E"];
    out.sort_by(|a, b| js_order(or_else(maxed[a] - maxed[b], || counts[b] - counts[a])));
    out.into_iter().map(String::from).collect()
}

/// Levels 1-18: `prefix`, then R at 6/11/16 and the rest by `priority`.
pub fn complete_path(prefix: &[String], priority: &[String]) -> Vec<String> {
    let mut path = prefix.to_vec();
    let count = |p: &Vec<String>, s: &str| p.iter().filter(|x| *x == s).count();
    while path.len() < 18 {
        let lvl = path.len() + 1;
        if [6, 11, 16].contains(&lvl) && count(&path, "R") < 3 {
            path.push("R".into());
            continue;
        }
        let next = priority.iter().find(|s| count(&path, s) < 5).cloned().unwrap_or_else(|| "R".into());
        path.push(next);
    }
    path
}

/// A platform's current and previous patch (server/patches.ts); None: Data Dragon's pair.
pub type RegionPatchOf<'a> = &'a dyn Fn(&str) -> Option<(String, String)>;

#[derive(Clone, Debug)]
pub struct Discarded {
    pub row: Arc<IreliaGameRow>,
    pub stale: bool,
    pub reason: String,
}

/// Stored games validated for a build: the usable ones, and the others with why.
#[derive(Clone, Debug, Default)]
pub struct ValidatedRows {
    pub games: Vec<ParsedGame>,
    pub discarded: Vec<Discarded>,
}

fn validate(rows: &[Arc<IreliaGameRow>], data: &StaticData, excl: &ExclusivityIndex, patch_weight: &dyn Fn(&IreliaGameRow) -> Option<f64>, window: f64) -> ValidatedRows {
    let mut out = ValidatedRows::default();
    for row in rows {
        match validate_game(row, data, patch_weight, window) {
            Verdict::Bad { reason, stale } => out.discarded.push(Discarded { row: row.clone(), stale, reason }),
            Verdict::Ok(g) => {
                let mut ids = g.completed.clone();
                if let Some(b) = g.boots {
                    ids.push(b);
                }
                if !excl.conflicts(&ids).is_empty() {
                    out.discarded.push(Discarded { row: row.clone(), stale: false, reason: "exclusive-item conflict".into() });
                    continue;
                }
                out.games.push(g);
            }
        }
    }
    out
}

fn key_ints(v: &[i64]) -> String {
    format!("[{}]", v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","))
}

pub fn rune_key(r: &RunePage) -> String {
    format!(
        "{{\"primaryStyle\":{},\"subStyle\":{},\"perks\":{},\"statPerks\":{}}}",
        r.primary_style,
        r.sub_style,
        key_ints(&r.perks),
        key_ints(&r.stat_perks)
    )
}

#[allow(clippy::too_many_arguments)]
pub fn recommend(
    rows: &[Arc<IreliaGameRow>],
    data: &StaticData,
    excl: &ExclusivityIndex,
    enemy_champion_id: Option<i64>,
    s: &EngineSettings,
    lane: Option<&str>,
    region_patch: RegionPatchOf,
    memo: Option<&parking_lot::Mutex<HashMap<String, Arc<ValidatedRows>>>>,
) -> BuildRecommendation {
    let enemy = enemy_champion_id.filter(|e| *e != 0);
    let in_lane = |r: &IreliaGameRow| lane.is_none() || to_lane(Some(&r.position)) == lane;
    let patch_of = |r: &IreliaGameRow| -> (String, String) {
        region_patch(platform_of_match(&r.match_id)).unwrap_or_else(|| (data.patch.clone(), data.previous_patch.clone()))
    };

    // ---- 1. Patch admission
    let current_count = rows.iter().filter(|r| r.patch == patch_of(r).0 && in_lane(r)).count() as f64;
    let use_previous = current_count < s.min_current_patch_games;
    let patch_weight = |r: &IreliaGameRow| -> Option<f64> {
        let (p, prev) = patch_of(r);
        if r.patch == p {
            return Some(1.0);
        }
        if use_previous && prev != p && r.patch == prev {
            Some(s.previous_patch_weight)
        } else {
            None
        }
    };
    let mut region_patches: Vec<String> = Vec::new();
    for r in rows {
        let p = patch_of(r).0;
        if !region_patches.contains(&p) {
            region_patches.push(p);
        }
    }
    let mut patch = region_patches.first().cloned().unwrap_or_else(|| data.patch.clone());
    for b in region_patches.iter().skip(1) {
        if compare_patch(b, &patch) > 0.0 {
            patch = b.clone();
        }
    }
    let mut admitted: Vec<String> = Vec::new();
    for r in rows {
        let (p, prev) = patch_of(r);
        let list = if use_previous && prev != p { vec![p, prev] } else { vec![p] };
        for x in list {
            if !admitted.contains(&x) {
                admitted.push(x);
            }
        }
    }

    // ---- 2. Validation
    let key = use_previous.to_string();
    let validated: Arc<ValidatedRows> = match memo {
        Some(m) => {
            let mut m = m.lock();
            if !m.contains_key(&key) {
                m.insert(key.clone(), Arc::new(validate(rows, data, excl, &patch_weight, s.starting_item_window_ms)));
            }
            m[&key].clone()
        }
        None => Arc::new(validate(rows, data, excl, &patch_weight, s.starting_item_window_ms)),
    };
    let games = &validated.games;
    let discarded = &validated.discarded;

    // ---- 3. Lane + matchup weighting
    let lane_games: Vec<&ParsedGame> = games.iter().filter(|g| in_lane(&g.row)).collect();
    let used_lane_fallback = lane.is_some() && (lane_games.len() as f64) < s.min_lane_games;
    let feeds_build = |r: &IreliaGameRow| in_lane(r) || (used_lane_fallback && to_lane(Some(&r.position)).is_some());
    let lane_weight = |g: &ParsedGame| if in_lane(&g.row) { 1.0 } else if feeds_build(&g.row) { s.other_lane_weight } else { 0.0 };
    let is_matchup = |g: &ParsedGame| enemy.is_none() || g.row.opponent_champion_id == enemy;
    let matchup: Vec<&&ParsedGame> = lane_games.iter().filter(|g| is_matchup(g)).collect();
    let used_fallback = enemy.is_some() && (matchup.len() as f64) < s.min_matchup_games;
    let weighted: Vec<(&ParsedGame, f64)> = games
        .iter()
        .map(|g| {
            let w = if is_matchup(g) { 1.0 } else if used_fallback { s.general_prior_weight } else { 0.0 };
            (g, w * lane_weight(g) * g.weight)
        })
        .filter(|x| x.1 > 0.0)
        .collect();
    let other_lane_games = weighted.iter().filter(|x| !in_lane(&x.0.row)).count() as f64;

    let mut validation = ValidationReport {
        current_patch: patch.clone(),
        fresh_games: 0.0,
        previous_patch_games: 0.0,
        stale_discarded: 0.0,
        invalid_discarded: 0.0,
        invalid_reasons: IndexMap::new(),
        conflicts_rejected: vec![],
    };
    for d in discarded.iter().filter(|d| feeds_build(&d.row)) {
        if d.stale {
            validation.stale_discarded += 1.0;
        } else {
            validation.invalid_discarded += 1.0;
            *validation.invalid_reasons.entry(d.reason.clone()).or_insert(0.0) += 1.0;
        }
    }
    for g in games.iter().filter(|g| feeds_build(&g.row)) {
        if g.row.patch == patch_of(&g.row).0 {
            validation.fresh_games += 1.0;
        } else {
            validation.previous_patch_games += 1.0;
        }
    }

    // ---- 4. Tallies
    let mut runes: Tally<RunePage> = Tally::new();
    let mut summs: Tally<Vec<i64>> = Tally::new();
    let mut starts: Tally<Vec<i64>> = Tally::new();
    let mut cores: Tally<Vec<i64>> = Tally::new();
    let mut boots: Tally<i64> = Tally::new();
    let mut slots: [Tally<i64>; 3] = [Tally::new(), Tally::new(), Tally::new()];
    let mut skills: Tally<Vec<String>> = Tally::new();
    let mut boots_at: Vec<(f64, f64)> = Vec::new();
    for &(g, w) in &weighted {
        let r = &g.row;
        let page = RunePage { primary_style: r.primary_style, sub_style: r.sub_style, perks: g.perks.clone(), stat_perks: g.stat_perks.clone() };
        let k = rune_key(&page);
        runes.add(page, k, w, g.win);
        let mut sm = vec![r.summoner1, r.summoner2];
        sm.sort();
        let k = key_ints(&sm);
        summs.add(sm, k, w, g.win);
        if !g.start.is_empty() {
            starts.add(g.start.clone(), key_ints(&g.start), w, g.win);
        }
        if g.completed.len() >= 3 {
            let c = g.completed[..3].to_vec();
            let k = key_ints(&c);
            cores.add(c, k, w, g.win);
        }
        if let Some(b) = g.boots {
            boots.add(b, b.to_string(), w, g.win);
            let first = |id: i64| g.purchases.iter().find(|p| p.0 == id).map(|p| p.1).unwrap_or(f64::INFINITY);
            let t = first(b);
            let before = g.completed.iter().filter(|id| first(**id) < t).count() as f64;
            boots_at.push((before.min(3.0), w));
        }
        for (i, id) in g.completed.iter().skip(3).take(3).enumerate() {
            slots[i].add(*id, id.to_string(), w, g.win);
        }
        let order = &r.skill_order;
        let n = order.chars().count();
        let len = if n >= 15 { 15 } else if n >= 9 { 9 } else { 0 };
        if len > 0 {
            let v: Vec<String> = order.chars().take(len).map(|c| c.to_string()).collect();
            let k = format!("[{}]", v.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(","));
            skills.add(v, k, w, g.win);
        }
    }

    // ---- 5. Exclusivity-safe assembly
    let mut rejected: Vec<String> = Vec::new();
    let core_options: Vec<Opt<Vec<i64>>> = cores
        .options(0.03, |ids| {
            let c = excl.conflicts(ids);
            for x in &c {
                rejected.push(excl.describe(x));
            }
            c.is_empty()
        })
        .into_iter()
        .take(3)
        .collect();
    let core: Vec<i64> = core_options.first().map(|o| o.value.clone()).unwrap_or_default();
    let boot_options: Vec<Opt<i64>> = boots.options(0.05, |id| excl.blocker(&core, *id).is_none()).into_iter().take(3).collect();
    let mut owned = core.clone();
    if let Some(b) = boot_options.first() {
        owned.push(b.value);
    }
    let slot_options: Vec<Vec<Opt<i64>>> = slots
        .iter()
        .map(|t| {
            t.options(0.03, |id| {
                let block = excl.blocker(&owned, *id);
                if let Some(b) = &block {
                    if b != "Duplicate" {
                        let name = data.items.get(id).map(|i| i.name.clone()).unwrap_or_else(|| "undefined".into());
                        rejected.push(format!("{name} blocked by {b}"));
                    }
                }
                block.is_none()
            })
            .into_iter()
            .take(4)
            .collect()
        })
        .collect();
    validation.conflicts_rejected = rejected;

    let skill_plan = skills.options(0.03, |_| true).into_iter().next().map(|o| {
        let priority = derive_priority(&complete_path(&o.value, &derive_priority(&o.value)));
        let path = complete_path(&o.value, &priority);
        Opt { value: SkillPlan { priority, path }, games: o.games, raw_games: o.raw_games, win_rate: o.win_rate, score: o.score }
    });

    let matchup_wins = matchup.iter().filter(|g| g.win).count() as f64;
    let patches = if !admitted.is_empty() {
        let mut a = admitted.clone();
        a.sort_by(|x, y| patch_ordering(y, x));
        a
    } else if use_previous && data.previous_patch != data.patch {
        vec![data.patch.clone(), data.previous_patch.clone()]
    } else {
        vec![data.patch.clone()]
    };
    let mut seen = Vec::new();
    for c in validation.conflicts_rejected.drain(..) {
        if !seen.contains(&c) {
            seen.push(c);
        }
    }
    validation.conflicts_rejected = seen;
    BuildRecommendation {
        patch,
        ddragon_version: data.version.clone(),
        enemy_champion_id,
        lane: lane.map(String::from),
        sample: SampleInfo {
            matchup_games: matchup.len() as f64,
            total_games: games.len() as f64,
            patches,
            used_fallback,
            lane_games: lane_games.len() as f64,
            other_lane_games,
            used_lane_fallback,
        },
        win_rate: if matchup.is_empty() { None } else { Some(matchup_wins / matchup.len() as f64) },
        runes: runes.options(0.03, |_| true).into_iter().next(),
        summoners: summs.options(0.05, |_| true).into_iter().next(),
        starting_items: starts.options(0.05, |_| true).into_iter().take(2).collect(),
        core_build: core_options,
        boots: boot_options,
        boots_after: Some(weighted_median(&boots_at)),
        situational: Situational {
            slot4: slot_options[0].clone(),
            slot5: slot_options[1].clone(),
            slot6: slot_options[2].clone(),
        },
        skills: skill_plan,
        validation,
    }
}
