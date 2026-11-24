//! Port of `server/model/draft.ts`: the B3 draft-aware recommender. Every decision is scored on its own by its
//! options' win-probability-added in games like this one (estimate.rs), then assembled under the shop's exclusivity
//! rules into the lane baseline's shape.

use super::context::{draft_context, item_dim, kernel_at, scale_of, standardise, standardise_into, ChampRef, Contexts, Scale, DRAFT_DIMS, N_DRAFT, N_ITEM, N_LIVE};
use super::dataset::{PreparedGame, ScoredGame};
use super::estimate::{estimate_decision, EstimateOptions, Memo, ObsRef, Observation, Obs, OptionEstimate, Query, Sims, Z, ESTIMATE_DEFAULTS};
use super::groups::{draft_groups_of, group_similarities, DraftGroups};
use super::profiles::ChampionProfiles;
use super::state::StateModel;
use super::swaps::{KEEP as SWAP_KEEP, SELL as SWAP_SELL};
use super::weights::{consistent, pilot_cap_factors, pilot_weight, recency_weigher, region_weight, weight_defaults, PatchAger, PatchWeigher, RegionPatchFn, WeightOptions};
use crate::ddragon::StaticData;
use crate::engine::exclusivity::{is_purchasable_item, ExclusivityIndex};
use crate::engine::recommend::{complete_path, js_order, round2};
use crate::engine::validator::STAT_SHARDS;
use crate::js;
use crate::jsmath;
use crate::types::*;
use indexmap::IndexMap;
use parking_lot::Mutex;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Below this many effective similar games, a decision keeps the lane baseline's pick.
pub const MIN_EFFECTIVE_GAMES: f64 = 30.0;

/// Scored games with their standardised contexts: built once per data version, queried per draft.
pub struct DraftModel {
    pub scored: Vec<ScoredGame>,
    pub state: Option<StateModel>,
    pub profiles: Arc<ChampionProfiles>,
    pub draft_scale: Scale,
    pub live_scale: Scale,
    pub z_draft: Arc<Contexts>,
    pub z_items: Arc<Contexts>,
    pub item_row: Vec<i32>,
    pub z_boots: Arc<Contexts>,
    pub boots_row: Vec<i32>,
    pub z_swaps: Arc<Contexts>,
    pub swap_row: Vec<i32>,
    pub groups: Option<DraftGroups>,
    /// Whether its items' ΔW ends at a sale or upgrade (2.2.0's effective window): what the best buy reads too.
    pub window: bool,
}

pub fn build_draft_model(scored: Vec<ScoredGame>, state: Option<StateModel>, profiles: Arc<ChampionProfiles>) -> DraftModel {
    let draft_scale = scale_of(scored.iter().map(|s| &s.g.draft[..]), N_DRAFT);
    let lives: Vec<&[f64]> = scored
        .iter()
        .flat_map(|s| s.g.items.iter().map(|c| &c.live[..]).chain(s.g.boots.iter().map(|b| &b.live[..])))
        .collect();
    let live_scale = scale_of(lives.iter().copied(), N_LIVE);
    let d = N_DRAFT;
    let iw = N_ITEM;
    let z_draft = Contexts::of(scored.len(), d, |out, i, at| standardise_into(&scored[i].g.draft, &draft_scale, out, at, d));
    let mut item_row = vec![0i32; scored.len() + 1];
    let mut boots_row = vec![-1i32; scored.len()];
    let mut boots = 0i32;
    for (i, s) in scored.iter().enumerate() {
        item_row[i + 1] = item_row[i] + s.g.items.len() as i32;
        if s.g.boots.is_some() {
            boots_row[i] = boots;
            boots += 1;
        }
    }
    let game_of = |row_of: &[i32], rows: usize, items: bool| -> Vec<usize> {
        let mut at = vec![0usize; rows];
        for i in 0..scored.len() {
            if items {
                for r in row_of[i]..row_of[i + 1] {
                    at[r as usize] = i;
                }
            } else if row_of[i] >= 0 {
                at[row_of[i] as usize] = i;
            }
        }
        at
    };
    let with_live = |rows: usize, game: &[usize], live: &dyn Fn(usize, usize) -> [f64; 8]| -> Contexts {
        Contexts::of(rows, iw, |out, r, at| {
            let i = game[r];
            out[at..at + d].copy_from_slice(&z_draft.flat[i * d..i * d + d]);
            standardise_into(&live(i, r), &live_scale, out, at + d, iw - d);
        })
    };
    let items = item_row[scored.len()] as usize;
    let z_items = with_live(items, &game_of(&item_row, items, true), &|i, r| scored[i].g.items[r - item_row[i] as usize].live);
    let z_boots = with_live(boots as usize, &game_of(&boots_row, boots as usize, false), &|i, _| scored[i].g.boots.as_ref().unwrap().live);
    let mut swap_row = vec![0i32; scored.len() + 1];
    for (i, s) in scored.iter().enumerate() {
        swap_row[i + 1] = swap_row[i] + s.g.swaps.len() as i32;
    }
    let swaps = swap_row[scored.len()] as usize;
    let z_swaps = with_live(swaps, &game_of(&swap_row, swaps, true), &|i, r| scored[i].g.swaps[r - swap_row[i] as usize].live);
    let games: Vec<&PreparedGame> = scored.iter().map(|s| &*s.g).collect();
    let groups = draft_groups_of(&games, &z_draft);
    DraftModel {
        scored,
        state,
        profiles,
        draft_scale,
        live_scale,
        z_draft: Arc::new(z_draft),
        z_items: Arc::new(z_items),
        item_row,
        z_boots: Arc::new(z_boots),
        boots_row,
        z_swaps: Arc::new(z_swaps),
        swap_row,
        groups,
        window: false,
    }
}

/// Game i's context row for the item finished in `slot` (-1: boots), or -1.
pub fn item_context_row(model: &DraftModel, i: usize, slot: i32) -> i32 {
    if slot < 0 {
        return model.boots_row[i];
    }
    let at = model.item_row[i] + slot;
    if at < model.item_row[i + 1] {
        at
    } else {
        -1
    }
}

#[derive(Clone, Debug)]
pub struct DraftQuery {
    pub lane: &'static str,
    pub opponent: Option<i64>,
    pub enemies: Vec<i64>,
    pub allies: Vec<i64>,
}

/// Each stored game's similarity to the draft at bandwidth `h`.
pub fn similarities(zs: &Contexts, zq: &[f64], h: f64) -> Vec<f64> {
    let rows = zs.rows;
    let d = zq.len();
    let mut out = vec![0.0; rows];
    if !h.is_finite() || zs.d != d {
        for i in 0..rows {
            out[i] = kernel_at(zs, i, zq, h, None);
        }
        return out;
    }
    let denominator = 2.0 * h * h;
    // Use GPU (crate::gpu): the card's numbers when an evaluation has it on, game by game the processor's.
    if crate::gpu::accelerate::accelerate(zs, zq, denominator, &mut out) {
        return out;
    }
    flat_similarities(&zs.flat, zs.any_nan, rows, d, zq, denominator, &mut out);
    out
}

/// similarities() over the flat contexts (`q` the draft's, `denominator` 2 h²).
pub fn flat_similarities(flat: &[f64], any_nan: bool, rows: usize, d: usize, q: &[f64], denominator: f64, out: &mut [f64]) {
    if !any_nan {
        let known: Vec<usize> = (0..d).filter(|j| !q[*j].is_nan()).collect();
        let n = known.len();
        if n == d && n > 0 {
            let nf = n as f64;
            // Four rows at a time: each row's sum keeps its own order (so the same doubles), but the four sums
            // don't wait on each other.
            let q = &q[..d];
            let mut i = 0;
            while i + 4 <= rows {
                let block = &flat[i * d..(i + 4) * d];
                let (r0, rest) = block.split_at(d);
                let (r1, rest) = rest.split_at(d);
                let (r2, r3) = rest.split_at(d);
                let mut s = [0.0f64; 4];
                for j in 0..d {
                    let qj = q[j];
                    let t = [r0[j] - qj, r1[j] - qj, r2[j] - qj, r3[j] - qj];
                    for k in 0..4 {
                        s[k] += t[k] * t[k];
                    }
                }
                for k in 0..4 {
                    out[i + k] = jsmath::exp(-(s[k] / nf) / denominator);
                }
                i += 4;
            }
            for i in i..rows {
                let at = i * d;
                let mut d2 = 0.0;
                for j in 0..d {
                    let t = flat[at + j] - q[j];
                    d2 += t * t;
                }
                out[i] = jsmath::exp(-(d2 / nf) / denominator);
            }
            return;
        }
        let nf = n as f64;
        for i in 0..rows {
            let at = i * d;
            let mut d2 = 0.0;
            for &j in &known {
                let t = flat[at + j] - q[j];
                d2 += t * t;
            }
            out[i] = if n > 0 { jsmath::exp(-(d2 / nf) / denominator) } else { 1.0 };
        }
        return;
    }
    for i in 0..rows {
        out[i] = row_similarity(flat, d, q, denominator, i);
    }
}

pub fn row_similarity(flat: &[f64], d: usize, q: &[f64], denominator: f64, i: usize) -> f64 {
    let mut d2 = 0.0;
    let mut n = 0.0;
    let at = i * d;
    for j in 0..d {
        let qj = q[j];
        let g = flat[at + j];
        if qj.is_nan() || g.is_nan() {
            continue;
        }
        let t = g - qj;
        d2 += t * t;
        n += 1.0;
    }
    if n != 0.0 {
        jsmath::exp(-(d2 / n) / denominator)
    } else {
        1.0
    }
}

/// A game's weight before the patch weight of a specific decision.
#[derive(Clone, Debug)]
pub struct WeightedGame {
    pub i: usize,
    pub base: f64,
    pub age: f64,
}

/// The weighted games of a model, with the patch weigher their decisions use.
pub struct Weighted {
    pub list: Vec<WeightedGame>,
    pub pw: PatchWeigher,
    samples: Mutex<HashMap<&'static str, DraftSample>>,
}

impl Weighted {
    pub fn patch(&self, g: &PreparedGame, items: &[i64]) -> f64 {
        self.pw.weigh(g, items)
    }
}

/// Pilot × region weights with the pilot cap applied per lane.
pub fn weigh_games(model: &DraftModel, data: &Arc<StaticData>, region_patch: &RegionPatchFn, o: &WeightOptions) -> Weighted {
    let pw = PatchWeigher::new(data.clone(), region_patch.clone(), o);
    let ager = PatchAger::new(data.clone(), region_patch.clone());
    let age_of = |g: &PreparedGame| Some(ager.age(&g.platform, &g.patch));
    let newest = if o.recency_days.map(|d| d != 0.0 && !d.is_nan()).unwrap_or(false) {
        model.scored.iter().fold(0.0f64, |m, s| super::logistic::js_max(m, s.g.created))
    } else {
        0.0
    };
    let recency = recency_weigher(newest, o.recency_days, &age_of);
    struct Pre {
        i: usize,
        w: f64,
        p0: f64,
    }
    let pre: Vec<Pre> = model
        .scored
        .iter()
        .enumerate()
        .map(|(i, s)| Pre { i, w: pilot_weight(&s.g, o) * region_weight(&s.g, o) * recency(&s.g), p0: pw.weigh(&s.g, &[]) })
        .filter(|x| x.w * x.p0 > 0.0)
        .collect();
    let caps = pilot_cap_factors(
        &pre.iter().map(|x| (model.scored[x.i].g.pilot.as_str(), model.scored[x.i].g.lane, x.w * x.p0)).collect::<Vec<_>>(),
        o.pilot_cap,
    );
    let list = pre
        .iter()
        .map(|x| {
            let g = &model.scored[x.i].g;
            WeightedGame {
                i: x.i,
                base: x.w * caps.get(&format!("{}:{}", g.lane, g.pilot)).copied().unwrap_or(1.0),
                age: ager.age(&g.platform, &g.patch),
            }
        })
        .collect();
    Weighted { list, pw, samples: Mutex::new(HashMap::new()) }
}

/// A decision's observations as the weights want them: with patch memory, after its data check.
pub fn observed(obs: Vec<Observation>, o: &WeightOptions) -> ObsRef {
    if o.memory.is_some() {
        Obs::new(consistent(obs, 30.0).0)
    } else {
        Obs::new(obs)
    }
}

/// The item query for a draft (the draft's context, the moment's unknown), with each game's similarity at `h`.
pub fn item_query_of<'a>(model: &DraftModel, q: &DraftQuery, h: f64) -> Query<'a> {
    let draft_raw = draft_raw_of(model, q);
    let zq = standardise(&draft_raw, &model.draft_scale);
    let values = if h.is_finite() { Some(Arc::new(similarities(&model.z_draft, &zq, h))) } else { None };
    let mut z = zq;
    z.extend(std::iter::repeat(f64::NAN).take(N_LIVE));
    Query { lane: q.lane, opponent: q.opponent, z, sims: values.map(|values| Sims { h, values, groups: None }), memo: None }
}

fn draft_raw_of(model: &DraftModel, q: &DraftQuery) -> Vec<f64> {
    draft_context(
        &model.profiles,
        &q.enemies.iter().map(|id| ChampRef { id: *id, position: None }).collect::<Vec<_>>(),
        &q.allies.iter().map(|id| ChampRef { id: *id, position: None }).collect::<Vec<_>>(),
        q.opponent.map(|id| ChampRef { id, position: Some(q.lane) }),
    )
}

pub fn opt_str(x: f64) -> Arc<str> {
    Arc::from(js::num_to_string(x).as_str())
}

/// The observations of what was done with `held` at late full-inventory shop visits, and each one's minute.
pub fn swap_observations(model: &DraftModel, weighted: &Weighted, held: f64) -> (Vec<Observation>, Vec<f64>) {
    let mut obs = Vec::new();
    let mut minutes = Vec::new();
    for wg in &weighted.list {
        let sg = &model.scored[wg.i];
        let swaps = &sg.g.swaps;
        if swaps.is_empty() {
            continue;
        }
        let wpa = &sg.swap_wpa;
        for (j, d) in swaps.iter().enumerate() {
            if d.held != held {
                continue;
            }
            let w = wg.base * weighted.patch(&sg.g, &[held as i64]) * d.w;
            if w <= 0.0 {
                continue;
            }
            obs.push(Observation {
                option: opt_str(d.item),
                wpa: wpa.get(j).copied().unwrap_or(f64::NAN),
                win: sg.g.win,
                start_win: f64::NAN,
                w,
                lane: sg.g.lane,
                opponent: sg.g.opponent,
                pilot: Arc::from(sg.g.pilot.as_str()),
                z: Z::Row(model.z_swaps.clone(), model.swap_row[wg.i] as usize + j),
                row: Some(wg.i),
                age: Some(wg.age),
            });
            minutes.push(d.t / 60_000.0);
        }
    }
    (obs, minutes)
}

/// Observations per decision key, and the swap lists' minutes: kept per model and weights (`extra.cache`).
#[derive(Default)]
pub struct DraftCache {
    pub obs: Mutex<HashMap<String, ObsRef>>,
    pub minutes: Mutex<HashMap<String, Arc<Vec<f64>>>>,
}

/// The swap advice for `held` in this query (an item query).
pub fn advise_swap(model: &DraftModel, weighted: &Weighted, q: &Query, held: f64, o: &EstimateOptions, cache: Option<&DraftCache>) -> Option<SwapAdvice> {
    let key = format!("swap:{}", js::num_to_string(held));
    let found = cache.and_then(|c| {
        let obs = c.obs.lock().get(&key).cloned()?;
        let minutes = c.minutes.lock().get(&key).cloned()?;
        Some((obs, minutes))
    });
    let (obs, minutes) = match found {
        Some(x) => x,
        None => {
            let (made, minutes) = swap_observations(model, weighted, held);
            let keep = js::num_to_string(SWAP_KEEP);
            let (mut sw, mut swx) = (0.0, 0.0);
            for x in &made {
                if &*x.option == keep.as_str() {
                    sw += x.w;
                    swx += x.w * x.wpa;
                }
            }
            let keep_mean = if sw > 0.0 { swx / sw } else { 0.0 };
            let obs = Obs::new(made.into_iter().map(|mut x| {
                x.wpa -= keep_mean;
                x
            }).collect());
            let minutes = Arc::new(minutes);
            if let Some(c) = cache {
                c.obs.lock().insert(key.clone(), obs.clone());
                c.minutes.lock().insert(key.clone(), minutes.clone());
            }
            (obs, minutes)
        }
    };
    if obs.is_empty() {
        return None;
    }
    let est = estimate_decision(&obs, q, &EstimateOptions { min_games: 1.0, min_share: 0.0, ..*o }, None, None);
    let keep_s = js::num_to_string(SWAP_KEEP);
    let sell_s = js::num_to_string(SWAP_SELL);
    let keep = est.options.iter().find(|x| *x.option == *keep_s);
    let best = est.options.first();
    let thin = est.n_eff < MIN_EFFECTIVE_GAMES || keep.is_none() || best.is_none();
    let se = |e: &OptionEstimate| (e.high - e.low) / (2.0 * 1.96);
    let lift = |e: &OptionEstimate| e.theta - keep.map(|k| k.theta).unwrap_or(0.0);
    let pick_of = |option: &str| -> SwapPick {
        if option == keep_s {
            SwapPick::Keep
        } else if option == sell_s {
            SwapPick::Sell
        } else {
            SwapPick::Swap { item: js::to_number(&serde_json::Value::String(option.into())) as i64 }
        }
    };
    let half_of = |e: &OptionEstimate| match keep {
        Some(k) => 1.96 * (se(e) * se(e) + se(k) * se(k)).sqrt(),
        None => 0.0,
    };
    let chosen = if thin { None } else { est.options.iter().find(|x| *x.option != *keep_s && lift(x) - half_of(x) > 0.0) };
    let minute_of = |option: &str| -> Option<f64> {
        let mut ms: Vec<f64> = obs.list.iter().enumerate().filter(|(_, x)| &*x.option == option).map(|(k, _)| minutes[k]).collect();
        ms.sort_by(|a, b| js_order(a - b));
        if ms.is_empty() {
            None
        } else {
            Some(js::round(ms[ms.len() / 2]))
        }
    };
    let half = chosen.map(half_of).unwrap_or(0.0);
    let main = chosen.or(keep).or(best);
    Some(SwapAdvice {
        held: held as i64,
        pick: chosen.map(|c| pick_of(&c.option)).unwrap_or(SwapPick::Keep),
        lift: chosen.map(lift),
        low: chosen.map(|c| lift(c) - half),
        high: chosen.map(|c| lift(c) + half),
        n_eff: main.map(|e| e.n_eff).unwrap_or(0.0),
        games: main.map(|e| e.games).unwrap_or(0.0),
        thin,
        alternatives: est
            .options
            .iter()
            .filter(|x| *x.option != *keep_s && chosen.map(|c| !std::ptr::eq(*x, c)).unwrap_or(true))
            .take(3)
            .map(|x| SwapAlternative {
                item: if *x.option == *sell_s { None } else { Some(js::to_number(&serde_json::Value::String(x.option.to_string())) as i64) },
                lift: lift(x),
                games: x.games,
            })
            .collect(),
        minute: chosen.and_then(|c| minute_of(&c.option)),
    })
}

fn to_stats(e: &OptionEstimate) -> PickStats {
    PickStats { lift: e.theta, low: e.low, high: e.high, n_eff: e.n_eff, games: e.games, pilots: e.pilots, win_rate: e.win_rate, matchup_games: e.matchup_games, start_win: e.start_win }
}

pub struct DecisionResult {
    pub pick: BuildPick,
    pub chosen: Option<OptionEstimate>,
    pub options: Vec<OptionEstimate>,
}

pub struct DecideInput<'a, 'q> {
    pub key: &'a str,
    pub label: &'a str,
    pub obs: ObsRef,
    pub q: &'a Query<'q>,
    pub o: EstimateOptions,
    pub filter: Option<&'a dyn Fn(&str) -> bool>,
    pub baseline: Option<String>,
    pub item: bool,
    pub raw: &'a [f64],
    pub opponent_name: Option<String>,
    pub reasons: bool,
}

fn reason_for(chosen: &OptionEstimate, lane_best: Option<&OptionEstimate>, d: &DecideInput) -> Option<String> {
    let lane_best = lane_best?;
    if lane_best.option == chosen.option {
        return None;
    }
    if d.q.opponent.is_some() && chosen.matchup_games >= 3.0 && chosen.context_theta <= lane_best.context_theta {
        return Some(format!(
            "vs {} ({} games)",
            d.opponent_name.clone().unwrap_or_else(|| "this opponent".into()),
            js::num_to_string(chosen.matchup_games)
        ));
    }
    let dims = |j: usize| if d.item { item_dim(j) } else { &DRAFT_DIMS[j] };
    driving_dimension(&chosen.option, &lane_best.option, &d.obs, d.q, &d.o, &|j| format!("{} {}", dims(j).label, (dims(j).format)(d.raw[j])))
}

/// The context dimension that alone gives `a` its largest lead over `b`, named by `name`.
pub fn driving_dimension(a: &str, b: &str, obs: &ObsRef, q: &Query, o: &EstimateOptions, name: &dyn Fn(usize) -> String) -> Option<String> {
    let pair = [a, b];
    let mut best: Option<usize> = None;
    let mut best_lead = 1e-6;
    for j in 0..q.z.len() {
        if q.z[j].is_nan() {
            continue;
        }
        let e = estimate_decision(obs, q, o, Some(&pair), Some(j));
        let x = e.options.iter().find(|v| &*v.option == a);
        let y = e.options.iter().find(|v| &*v.option == b);
        let (Some(x), Some(y)) = (x, y) else { continue };
        let lead = x.context_theta - y.context_theta;
        if lead > best_lead {
            best_lead = lead;
            best = Some(j);
        }
    }
    best.map(name)
}

pub fn decide(d: &DecideInput) -> DecisionResult {
    let est = estimate_decision(&d.obs, d.q, &d.o, None, None);
    let ok = |o: &str| d.filter.map(|f| f(o)).unwrap_or(true);
    let options: Vec<OptionEstimate> = est.options.into_iter().filter(|x| ok(&x.option)).collect();
    let thin = est.n_eff < MIN_EFFECTIVE_GAMES || options.is_empty();
    let baseline_est = d.baseline.as_ref().and_then(|b| options.iter().find(|x| *x.option == **b)).cloned();
    let mut chosen = options.first().cloned();
    let mut value = chosen.as_ref().map(|c| c.option.to_string()).or_else(|| d.baseline.clone()).unwrap_or_default();
    let mut fallback = false;
    if thin {
        if let Some(b) = &d.baseline {
            if ok(b) {
                chosen = baseline_est.clone();
                value = b.clone();
                fallback = true;
            }
        }
    }
    let mut by_lane: Vec<&OptionEstimate> = options.iter().collect();
    by_lane.sort_by(|a, b| js_order(b.lane_theta - a.lane_theta));
    let lane_best = by_lane.first().copied();
    let reason = match (&chosen, fallback, d.reasons) {
        (Some(c), false, true) => reason_for(c, lane_best, d),
        _ => None,
    };
    let pick = BuildPick {
        key: d.key.into(),
        label: d.label.into(),
        value: value.clone(),
        stats: chosen.as_ref().map(to_stats),
        reason,
        fallback,
        changed: d.baseline.as_ref().map(|b| value != *b).unwrap_or(false),
        baseline: d.baseline.as_ref().map(|b| PickBaseline { value: b.clone(), lift: baseline_est.as_ref().map(|e| e.theta) }),
        alternatives: options.iter().filter(|x| *x.option != *value).take(3).map(|x| PickAlternative { value: x.option.to_string(), stats: to_stats(x) }).collect(),
    };
    DecisionResult { pick, chosen, options }
}

/// Where a rune sits: its tree and row (0 = keystones).
fn rune_index(data: &StaticData) -> HashMap<i64, (i64, usize)> {
    let mut at = HashMap::new();
    for tree in &data.runes {
        for (row, slot) in tree.slots.iter().enumerate() {
            for r in &slot.runes {
                at.insert(r.id, (tree.id, row));
            }
        }
    }
    at
}

fn option_of<T>(e: Option<&OptionEstimate>, value: T) -> Opt<T> {
    Opt {
        value,
        games: round2(e.map(|e| e.n_eff).unwrap_or(0.0)),
        raw_games: e.map(|e| e.games).unwrap_or(0.0),
        win_rate: e.map(|e| e.win_rate).unwrap_or(0.0),
        score: e.map(|e| e.theta).unwrap_or(0.0),
    }
}

#[derive(Clone, Debug)]
pub struct DraftSettings {
    pub weights: WeightOptions,
    pub estimate: EstimateOptions,
    pub item_k: Option<f64>,
}

pub fn draft_defaults() -> DraftSettings {
    DraftSettings { weights: weight_defaults(), estimate: ESTIMATE_DEFAULTS, item_k: None }
}

#[derive(Default)]
pub struct DraftExtra<'a> {
    pub reasons: Option<bool>,
    pub detail: bool,
    pub weighted: Option<&'a Weighted>,
    pub cache: Option<&'a DraftCache>,
    pub memo: Option<&'a RefCell<Memo>>,
    pub decisions: Option<&'a HashSet<&'static str>>,
}

fn num_of(v: &str) -> f64 {
    js::to_number(&serde_json::Value::String(v.into()))
}

/// The draft-adjusted build for `q`, next to the lane baseline `baseline`.
#[allow(clippy::too_many_arguments)]
pub fn recommend_draft(
    model: &DraftModel,
    q: &DraftQuery,
    data: &Arc<StaticData>,
    excl: &ExclusivityIndex,
    region_patch: &RegionPatchFn,
    baseline: &BuildRecommendation,
    s: &DraftSettings,
    extra: &DraftExtra,
) -> DraftRecommendation {
    let own_weighted;
    let weighted: &Weighted = match extra.weighted {
        Some(w) => w,
        None => {
            own_weighted = weigh_games(model, data, region_patch, &s.weights);
            &own_weighted
        }
    };
    let mut estimates: IndexMap<String, IndexMap<String, f64>> = IndexMap::new();
    let draft_raw = draft_raw_of(model, q);
    let zq = standardise(&draft_raw, &model.draft_scale);
    let h = s.estimate.h;
    let own_memo = RefCell::new(Memo::default());
    let memo = extra.memo.unwrap_or(&own_memo);
    let mut sims: Option<Sims> = None;
    if h.is_finite() {
        let mut values = if extra.memo.is_some() { memo.borrow().sims.clone() } else { None };
        let mut grouped: Option<(Arc<Vec<i32>>, Arc<Vec<f64>>)> = None;
        if let Some(g) = &model.groups {
            let gs = match if extra.memo.is_some() { memo.borrow().group_sims.clone() } else { None } {
                Some(x) => x,
                None => Arc::new(group_similarities(g, &zq, h)),
            };
            if extra.memo.is_some() {
                memo.borrow_mut().group_sims = Some(gs.clone());
            }
            if values.is_none() {
                let v: Vec<f64> = g.of.iter().map(|x| gs[*x as usize]).collect();
                let v = Arc::new(v);
                if extra.memo.is_some() {
                    memo.borrow_mut().sims = Some(v.clone());
                }
                values = Some(v);
            }
            grouped = Some((g.of.clone(), gs));
        } else if values.is_none() {
            let v = Arc::new(similarities(&model.z_draft, &zq, h));
            if extra.memo.is_some() {
                memo.borrow_mut().sims = Some(v.clone());
            }
            values = Some(v);
        }
        sims = values.map(|values| Sims { h, values, groups: grouped });
    }
    let mut item_z = zq.clone();
    item_z.extend(std::iter::repeat(f64::NAN).take(N_LIVE));
    let item_query = Query { lane: q.lane, opponent: q.opponent, z: item_z, sims: sims.clone(), memo: Some(memo) };
    let draft_query = Query { lane: q.lane, opponent: q.opponent, z: zq.clone(), sims, memo: Some(memo) };
    let mut item_raw = draft_raw.clone();
    item_raw.extend(std::iter::repeat(f64::NAN).take(N_LIVE));
    let opponent_name = q.opponent.and_then(|o| data.champions.get(&o).map(|c| c.name.clone()));
    let runes = rune_index(data);
    let mut picks: Vec<BuildPick> = Vec::new();

    let pregame = |option: &dyn Fn(&PreparedGame) -> Option<Vec<String>>, items: Option<&dyn Fn(&PreparedGame) -> Vec<i64>>| -> Vec<Observation> {
        let mut out = Vec::new();
        for wg in &weighted.list {
            let sg = &model.scored[wg.i];
            let Some(v) = option(&sg.g) else { continue };
            let w = wg.base * weighted.patch(&sg.g, &items.map(|f| f(&sg.g)).unwrap_or_default());
            if w <= 0.0 {
                continue;
            }
            let pilot: Arc<str> = Arc::from(sg.g.pilot.as_str());
            for opt in v {
                out.push(Observation {
                    option: Arc::from(opt.as_str()),
                    wpa: sg.wpa0,
                    win: sg.g.win,
                    start_win: f64::NAN,
                    w,
                    lane: sg.g.lane,
                    opponent: sg.g.opponent,
                    pilot: pilot.clone(),
                    z: Z::Row(model.z_draft.clone(), wg.i),
                    row: Some(wg.i),
                    age: Some(wg.age),
                });
            }
        }
        out
    };
    let item_obs = |slot: i32| -> Vec<Observation> {
        let mut out = Vec::new();
        for wg in &weighted.list {
            let sg = &model.scored[wg.i];
            let choice = if slot < 0 { sg.g.boots.as_ref() } else { sg.g.items.get(slot as usize) };
            let z_row = item_context_row(model, wg.i, slot);
            let Some(choice) = choice else { continue };
            if z_row < 0 {
                continue;
            }
            let w = wg.base * weighted.patch(&sg.g, &[choice.item as i64]);
            if w <= 0.0 {
                continue;
            }
            let wpa = if slot < 0 { sg.boots_wpa.unwrap_or(f64::NAN) } else { sg.item_wpa[slot as usize] };
            let zs = if slot < 0 { model.z_boots.clone() } else { model.z_items.clone() };
            out.push(Observation {
                option: opt_str(choice.item),
                wpa,
                win: sg.g.win,
                start_win: if slot < 0 { sg.boots_w0.unwrap_or(f64::NAN) } else { sg.item_w0[slot as usize] },
                w,
                lane: sg.g.lane,
                opponent: sg.g.opponent,
                pilot: Arc::from(sg.g.pilot.as_str()),
                z: Z::Row(zs, z_row as usize),
                row: Some(wg.i),
                age: Some(wg.age),
            });
        }
        out
    };
    let cached = |key: &str, make: &dyn Fn() -> Vec<Observation>| -> ObsRef {
        match extra.cache {
            None => observed(make(), &s.weights),
            Some(c) => {
                if let Some(o) = c.obs.lock().get(key) {
                    return o.clone();
                }
                let o = observed(make(), &s.weights);
                c.obs.lock().insert(key.to_string(), o.clone());
                o
            }
        }
    };
    let item_o = EstimateOptions { k: s.item_k.unwrap_or(s.estimate.k), ..s.estimate };

    #[allow(clippy::too_many_arguments)]
    fn run_decision(
        key: &str,
        label: &str,
        obs_of: &dyn Fn() -> ObsRef,
        baseline_value: Option<String>,
        filter: Option<&dyn Fn(&str) -> bool>,
        item: bool,
        ctx: &RunCtx,
        picks: &mut Vec<BuildPick>,
        estimates: &mut IndexMap<String, IndexMap<String, f64>>,
    ) -> DecisionResult {
        if let Some(dec) = ctx.decisions {
            if !dec.contains(key) {
                let pick = BuildPick {
                    key: key.into(),
                    label: label.into(),
                    value: baseline_value.clone().unwrap_or_default(),
                    stats: None,
                    reason: None,
                    fallback: true,
                    changed: false,
                    baseline: baseline_value.map(|v| PickBaseline { value: v, lift: None }),
                    alternatives: vec![],
                };
                picks.push(pick.clone());
                return DecisionResult { pick, chosen: None, options: vec![] };
            }
        }
        let obs = obs_of();
        let r = decide(&DecideInput {
            key,
            label,
            obs,
            q: if item { ctx.item_query } else { ctx.draft_query },
            o: if item { ctx.item_o } else { ctx.draft_o },
            filter,
            baseline: baseline_value,
            item,
            raw: if item { ctx.item_raw } else { ctx.draft_raw },
            opponent_name: ctx.opponent_name.clone(),
            reasons: ctx.reasons,
        });
        picks.push(r.pick.clone());
        if ctx.detail {
            estimates.insert(key.into(), r.options.iter().map(|x| (x.option.to_string(), x.theta)).collect());
        }
        r
    }
    struct RunCtx<'a, 'q> {
        decisions: Option<&'a HashSet<&'static str>>,
        item_query: &'a Query<'q>,
        draft_query: &'a Query<'q>,
        item_o: EstimateOptions,
        draft_o: EstimateOptions,
        item_raw: &'a [f64],
        draft_raw: &'a [f64],
        opponent_name: Option<String>,
        reasons: bool,
        detail: bool,
    }
    let ctx = RunCtx {
        decisions: extra.decisions,
        item_query: &item_query,
        draft_query: &draft_query,
        item_o,
        draft_o: s.estimate,
        item_raw: &item_raw,
        draft_raw: &draft_raw,
        opponent_name: opponent_name.clone(),
        reasons: extra.reasons != Some(false),
        detail: extra.detail,
    };
    let runes_of = |o: &str| runes.get(&(num_of(o) as i64)).copied();

    // ---- Runes
    let b_runes = baseline.runes.as_ref().map(|r| r.value.clone());
    let keystone = run_decision(
        "keystone",
        "Keystone",
        &|| cached("keystone", &|| pregame(&|g| Some(vec![g.perks[0].to_string()]), None)),
        b_runes.as_ref().map(|r| r.perks[0].to_string()),
        Some(&|o| runes_of(o).map(|x| x.1 == 0).unwrap_or(false)),
        false,
        &ctx,
        &mut picks,
        &mut estimates,
    );
    let tree = runes_of(&keystone.pick.value).map(|x| x.0).or(b_runes.as_ref().map(|r| r.primary_style)).unwrap_or(0);
    let same_tree = b_runes.as_ref().map(|r| r.primary_style == tree).unwrap_or(false);
    let mut rows: Vec<DecisionResult> = Vec::new();
    for row in 1..=3usize {
        let key = format!("row{row}");
        let cache_key = format!("row{row}:{tree}");
        let r = run_decision(
            &key,
            &format!("Primary row {row}"),
            &|| cached(&cache_key, &|| pregame(&|g| if g.primary == tree { Some(vec![g.perks[row].to_string()]) } else { None }, None)),
            if same_tree { b_runes.as_ref().map(|r| r.perks[row].to_string()) } else { None },
            Some(&|o| runes_of(o).map(|x| x.0 == tree && x.1 == row).unwrap_or(false)),
            false,
            &ctx,
            &mut picks,
            &mut estimates,
        );
        rows.push(r);
    }
    let sub_tree = run_decision(
        "secondaryTree",
        "Secondary tree",
        &|| cached(&format!("secondaryTree:{tree}"), &|| pregame(&|g| if g.primary == tree { Some(vec![g.sub.to_string()]) } else { None }, None)),
        if same_tree { b_runes.as_ref().map(|r| r.sub_style.to_string()) } else { None },
        Some(&|o| num_of(o) != tree as f64 && data.runes.iter().any(|t| t.id as f64 == num_of(o))),
        false,
        &ctx,
        &mut picks,
        &mut estimates,
    );
    let sub = num_of(&sub_tree.pick.value);
    let sub_key = format!("secondary:{tree}:{}", js::num_to_string(sub));
    let sub_obs = || {
        cached(&sub_key, &|| {
            pregame(&|g| if g.primary == tree && g.sub as f64 == sub { Some(vec![g.perks[4].to_string(), g.perks[5].to_string()]) } else { None }, None)
        })
    };
    let same_sub = same_tree && b_runes.as_ref().map(|r| r.sub_style as f64 == sub).unwrap_or(false);
    let in_sub = |o: &str| runes_of(o).map(|x| x.0 as f64 == sub && x.1 > 0).unwrap_or(false);
    let sec1 = run_decision(
        "secondary1",
        "Secondary rune",
        &sub_obs,
        if same_sub { b_runes.as_ref().map(|r| r.perks[4].to_string()) } else { None },
        Some(&in_sub),
        false,
        &ctx,
        &mut picks,
        &mut estimates,
    );
    let row1 = runes_of(&sec1.pick.value).map(|x| x.1);
    let sec2 = run_decision(
        "secondary2",
        "Secondary rune",
        &sub_obs,
        if same_sub { b_runes.as_ref().map(|r| r.perks[5].to_string()) } else { None },
        Some(&|o| in_sub(o) && runes_of(o).map(|x| x.1) != row1),
        false,
        &ctx,
        &mut picks,
        &mut estimates,
    );
    let shard_labels = ["Offense shard", "Flex shard", "Defense shard"];
    let mut shards: Vec<DecisionResult> = Vec::new();
    for i in 0..3usize {
        let key = format!("shard{}", i + 1);
        let r = run_decision(
            &key,
            shard_labels[i],
            &|| cached(&key, &|| pregame(&|g| Some(vec![g.shards[i].to_string()]), None)),
            b_runes.as_ref().map(|r| r.stat_perks[i].to_string()),
            Some(&|o| STAT_SHARDS.contains(&(num_of(o) as i64)) && num_of(o).fract() == 0.0),
            false,
            &ctx,
            &mut picks,
            &mut estimates,
        );
        shards.push(r);
    }

    // ---- Summoners, start, skills
    let summs = run_decision(
        "summoners",
        "Summoner spells",
        &|| cached("summoners", &|| pregame(&|g| Some(vec![g.summoners.clone()]), None)),
        baseline.summoners.as_ref().map(|o| {
            let mut v = o.value.clone();
            v.sort();
            v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",")
        }),
        None,
        false,
        &ctx,
        &mut picks,
        &mut estimates,
    );
    let start_items = |g: &PreparedGame| -> Vec<i64> { g.start.split(',').map(|x| num_of(x) as i64).collect() };
    let start = run_decision(
        "start",
        "Starting items",
        &|| cached("start", &|| pregame(&|g| if g.start.is_empty() { None } else { Some(vec![g.start.clone()]) }, Some(&start_items))),
        baseline.starting_items.first().map(|o| o.value.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",")),
        Some(&|o| o.split(',').all(|id| {
            let n = num_of(id);
            n.fract() == 0.0 && is_purchasable_item(data.items.get(&(n as i64)))
        })),
        false,
        &ctx,
        &mut picks,
        &mut estimates,
    );
    let max_first = run_decision(
        "maxFirst",
        "Ability maxed first",
        &|| cached("maxFirst", &|| pregame(&|g| g.max_first.clone().filter(|m| !m.is_empty()).map(|m| vec![m]), None)),
        baseline.skills.as_ref().map(|s| s.value.priority[0].clone()),
        None,
        false,
        &ctx,
        &mut picks,
        &mut estimates,
    );
    let early = run_decision(
        "early",
        "Levels 1-3",
        &|| cached("early", &|| pregame(&|g| g.early.clone().filter(|m| !m.is_empty()).map(|m| vec![m]), None)),
        baseline.skills.as_ref().map(|s| s.value.path[..3.min(s.value.path.len())].concat()),
        Some(&|o| o.chars().count() == 3 && o.chars().all(|c| c == 'Q' || c == 'W' || c == 'E')),
        false,
        &ctx,
        &mut picks,
        &mut estimates,
    );

    // ---- Items: core slots, boots, items 4-6
    let mut owned: Vec<i64> = Vec::new();
    let item_ok = |o: &str| {
        let n = num_of(o);
        n.fract() == 0.0 && is_purchasable_item(data.items.get(&(n as i64)))
    };
    let b_core: Vec<i64> = baseline.core_build.first().map(|o| o.value.clone()).unwrap_or_default();
    let core_labels = ["First item", "Second item", "Third item"];
    let mut core: Vec<DecisionResult> = Vec::new();
    for slot in 0..3usize {
        let key = format!("core{}", slot + 1);
        let snapshot = owned.clone();
        let can_buy = |o: &str| item_ok(o) && excl.blocker(&snapshot, num_of(o) as i64).is_none();
        let r = run_decision(
            &key,
            core_labels[slot],
            &|| cached(&key, &|| item_obs(slot as i32)),
            b_core.get(slot).map(|x| x.to_string()),
            Some(&can_buy),
            true,
            &ctx,
            &mut picks,
            &mut estimates,
        );
        if !r.pick.value.is_empty() {
            owned.push(num_of(&r.pick.value) as i64);
        }
        core.push(r);
    }
    let snapshot = owned.clone();
    let can_buy = |o: &str| item_ok(o) && excl.blocker(&snapshot, num_of(o) as i64).is_none();
    let boots = run_decision(
        "boots",
        "Boots",
        &|| cached("boots", &|| item_obs(-1)),
        baseline.boots.first().map(|o| o.value.to_string()),
        Some(&can_buy),
        true,
        &ctx,
        &mut picks,
        &mut estimates,
    );
    if !boots.pick.value.is_empty() {
        owned.push(num_of(&boots.pick.value) as i64);
    }
    let b_slots = [&baseline.situational.slot4, &baseline.situational.slot5, &baseline.situational.slot6];
    let mut later: Vec<DecisionResult> = Vec::new();
    for (i, slot) in [3usize, 4, 5].iter().enumerate() {
        let key = format!("item{}", slot + 1);
        let snapshot = owned.clone();
        let can_buy = |o: &str| item_ok(o) && excl.blocker(&snapshot, num_of(o) as i64).is_none();
        let r = run_decision(
            &key,
            &format!("Item {}", slot + 1),
            &|| cached(&key, &|| item_obs(*slot as i32)),
            b_slots[i].first().map(|o| o.value.to_string()),
            Some(&can_buy),
            true,
            &ctx,
            &mut picks,
            &mut estimates,
        );
        if !r.pick.value.is_empty() {
            owned.push(num_of(&r.pick.value) as i64);
        }
        later.push(r);
    }

    // ---- Assembly in the baseline's shape
    let num = |r: &DecisionResult| num_of(&r.pick.value);
    let rune_page = RunePage {
        primary_style: tree,
        sub_style: sub as i64,
        perks: [num(&keystone), num(&rows[0]), num(&rows[1]), num(&rows[2]), num(&sec1), num(&sec2)].iter().map(|x| *x as i64).collect(),
        stat_perks: shards.iter().map(|r| num(r) as i64).collect(),
    };
    let first_skill = max_first.pick.value.clone();
    let base_priority: Vec<String> = baseline.skills.as_ref().map(|s| s.value.priority.clone()).unwrap_or_else(|| vec!["Q".into(), "E".into(), "W".into()]);
    let rest: Vec<String> = base_priority.into_iter().filter(|x| *x != first_skill).collect();
    let priority: Vec<String> = if !first_skill.is_empty() { std::iter::once(first_skill.clone()).chain(rest.iter().cloned()).collect() } else { rest.clone() };
    let early_value = if early.pick.value.is_empty() { "QEW".to_string() } else { early.pick.value.clone() };
    let early_path: Vec<String> = early_value.chars().map(|c| c.to_string()).collect();
    let skill_plan = SkillPlan { path: complete_path(&early_path, &priority), priority };
    let core_value: Vec<i64> = core.iter().map(|r| num(r)).filter(|id| *id > 0.0).map(|x| x as i64).collect();
    let can_buy_after = |o: &str| item_ok(o) && excl.blocker(&owned, num_of(o) as i64).is_none();
    let slot_options = |r: &DecisionResult| -> Vec<Opt<i64>> {
        let mut v: Vec<Opt<i64>> = Vec::new();
        if let (Some(c), false) = (&r.chosen, r.pick.value.is_empty()) {
            v.push(option_of(Some(c), num(r) as i64));
        }
        v.extend(r.options.iter().filter(|x| *x.option != *r.pick.value && can_buy_after(&x.option)).take(3).map(|x| option_of(Some(x), num_of(&x.option) as i64)));
        v
    };
    let sample = lane_sample(model, weighted, q.lane);
    let mut chips: Vec<ContextChip> = DRAFT_DIMS
        .iter()
        .enumerate()
        .map(|(i, d)| ContextChip {
            key: d.key.into(),
            label: d.label.into(),
            value: if draft_raw[i].is_finite() { (d.format)(draft_raw[i]) } else { String::new() },
            z: zq[i],
        })
        .filter(|c| c.z.is_finite() && c.z.abs() >= 0.5)
        .collect();
    chips.sort_by(|a, b| js_order(b.z.abs() - a.z.abs()));
    chips.truncate(4);

    let mut build = baseline.clone();
    build.runes = Some(option_of(keystone.chosen.as_ref(), rune_page));
    if !summs.pick.value.is_empty() {
        build.summoners = Some(option_of(summs.chosen.as_ref(), summs.pick.value.split(',').map(|x| num_of(x) as i64).collect()));
    }
    if !start.pick.value.is_empty() {
        build.starting_items = vec![option_of(start.chosen.as_ref(), start.pick.value.split(',').map(|x| num_of(x) as i64).collect())];
    }
    if core_value.len() == 3 {
        build.core_build = vec![option_of(core[0].chosen.as_ref(), core_value.clone())];
    }
    if !boots.pick.value.is_empty() {
        let mut b = vec![option_of(boots.chosen.as_ref(), num(&boots) as i64)];
        b.extend(
            boots
                .options
                .iter()
                .filter(|x| *x.option != *boots.pick.value)
                .filter(|x| excl.blocker(&core_value, num_of(&x.option) as i64).is_none())
                .take(2)
                .map(|x| option_of(Some(x), num_of(&x.option) as i64)),
        );
        build.boots = b;
    }
    build.situational = Situational { slot4: slot_options(&later[0]), slot5: slot_options(&later[1]), slot6: slot_options(&later[2]) };
    build.skills = Some(option_of(max_first.chosen.as_ref(), skill_plan));

    let swaps = if extra.decisions.map(|d| !d.contains("swaps")).unwrap_or(false) {
        None
    } else {
        let mut held: Vec<f64> = core_value.iter().map(|x| *x as f64).collect();
        if !boots.pick.value.is_empty() {
            held.push(num(&boots));
        }
        held.extend(later.iter().take(2).map(num).filter(|id| *id > 0.0));
        Some(held.iter().filter_map(|h| advise_swap(model, weighted, &item_query, *h, &item_o, extra.cache)).collect())
    };
    if build.situational.slot4.is_empty() {
        build.situational.slot4 = baseline.situational.slot4.clone();
    }
    if build.situational.slot5.is_empty() {
        build.situational.slot5 = baseline.situational.slot5.clone();
    }
    if build.situational.slot6.is_empty() {
        build.situational.slot6 = baseline.situational.slot6.clone();
    }
    DraftRecommendation { estimates: if extra.detail { Some(estimates) } else { None }, build, picks, context: chips, sample, swaps }
}

/// The games behind a lane's builds: the same for every draft on these weighted games.
fn lane_sample(model: &DraftModel, weighted: &Weighted, lane: &'static str) -> DraftSample {
    if let Some(s) = weighted.samples.lock().get(lane) {
        return s.clone();
    }
    let lane_games: Vec<&WeightedGame> = weighted.list.iter().filter(|w| model.scored[w.i].g.lane == lane).collect();
    let mut regions: IndexMap<String, f64> = IndexMap::new();
    for sg in &model.scored {
        if sg.g.lane == lane && sg.g.source != "me" {
            *regions.entry(sg.g.platform.clone()).or_insert(0.0) += 1.0;
        }
    }
    let (mut sw, mut sw2) = (0.0, 0.0);
    for w in &lane_games {
        sw += w.base;
    }
    for w in &lane_games {
        sw2 += w.base * w.base;
    }
    let pilots: HashSet<&str> = lane_games.iter().map(|w| model.scored[w.i].g.pilot.as_str()).collect();
    let sample = DraftSample { games: lane_games.len() as f64, n_eff: if sw2 > 0.0 { (sw * sw) / sw2 } else { 0.0 }, pilots: pilots.len() as f64, regions };
    weighted.samples.lock().insert(lane, sample.clone());
    sample
}
