//! Port of `server/model/buy.ts` (Stage C3): the best purchase for your current gold, toward the next completed item.

use super::dataset::ScoredGame;
use super::state::{win_probability_of, StateModel};
use crate::ddragon::StaticData;
use crate::js;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const WPA_LEAD: f64 = 0.02;
const WPA_MIN_GAMES: f64 = 30.0;
const MATCHUP_WEIGHT: f64 = 3.0;

struct Path {
    lane: &'static str,
    opponent: Option<i64>,
    /// Each part bought toward the target (first purchase of each): its id and its ΔW.
    parts: Vec<(i64, f64)>,
}

fn parts_of(data: &StaticData, id: i64, out: &mut Vec<i64>) {
    if let Some(it) = data.item(id) {
        for c in &it.from {
            out.push(*c);
            parts_of(data, *c, out);
        }
    }
}

/// A node of the target's recipe tree you don't own (arena index; 0 is the root).
#[derive(Clone, Debug)]
pub struct Missing {
    pub id: i64,
    pub cost: f64,
    pub children: Vec<usize>,
}

/// The recipe tree of `target` minus what you own (owned items cover the first matching node, each once); None when owned.
pub fn missing_tree(data: &StaticData, target: i64, owned: &[i64]) -> Option<Vec<Missing>> {
    let mut pool = owned.to_vec();
    let mut nodes: Vec<Missing> = Vec::new();
    fn walk(data: &StaticData, id: i64, pool: &mut Vec<i64>, nodes: &mut Vec<Missing>) -> Option<usize> {
        if let Some(at) = pool.iter().position(|x| *x == id) {
            pool.remove(at);
            return None;
        }
        let item = data.item(id);
        let from: Vec<i64> = item.map(|i| i.from.clone()).unwrap_or_default();
        let combine = item.map(|i| i.gold_total).unwrap_or(0.0) - from.iter().fold(0.0, |s, c| s + data.item(*c).map(|i| i.gold_total).unwrap_or(0.0));
        let me = nodes.len();
        nodes.push(Missing { id, cost: 0.0, children: vec![] });
        let children: Vec<usize> = from.iter().filter_map(|c| walk(data, *c, pool, nodes)).collect();
        let cost = combine + children.iter().fold(0.0, |s, c| s + nodes[*c].cost);
        nodes[me].cost = cost;
        nodes[me].children = children;
        Some(me)
    }
    walk(data, target, &mut pool, &mut nodes)?;
    Some(nodes)
}

/// Every missing node below the root, each with its ancestors and descendants (root excluded).
fn candidates(nodes: &[Missing]) -> Vec<(usize, HashSet<usize>)> {
    fn below(nodes: &[Missing], n: usize, out: &mut Vec<usize>) {
        for c in &nodes[n].children {
            out.push(*c);
            below(nodes, *c, out);
        }
    }
    fn visit(nodes: &[Missing], n: usize, ancestors: &[usize], out: &mut Vec<(usize, HashSet<usize>)>) {
        for c in &nodes[n].children {
            let mut rel: Vec<usize> = ancestors.to_vec();
            rel.push(n);
            below(nodes, *c, &mut rel);
            out.push((*c, rel.into_iter().filter(|x| *x != 0).collect()));
            let mut next: Vec<usize> = ancestors.to_vec();
            next.push(n);
            next.retain(|x| *x != 0);
            visit(nodes, *c, &next, out);
        }
    }
    let mut out = Vec::new();
    visit(nodes, 0, &[], &mut out);
    out
}

pub struct BuyQuery<'a> {
    pub lane: &'static str,
    pub opponent: Option<i64>,
    pub target: i64,
    pub owned: &'a [i64],
    pub gold: f64,
}

/// The stored paths to each target item, built once per model.
pub struct BuyIndex {
    scored: Arc<Vec<ScoredGame>>,
    state: Option<StateModel>,
    data: Arc<StaticData>,
    paths: Mutex<HashMap<i64, Arc<Vec<Path>>>>,
    /// 2.2.0's effective window: a part's ΔW ends when it is consumed into an upgrade (or sold).
    window: bool,
}

impl BuyIndex {
    pub fn new(scored: Arc<Vec<ScoredGame>>, state: Option<StateModel>, data: Arc<StaticData>) -> BuyIndex {
        BuyIndex { scored, state, data, paths: Mutex::new(HashMap::new()), window: false }
    }

    /// The same with the ΔW definition the model was built with (DraftModel::window).
    pub fn with_window(mut self, window: bool) -> BuyIndex {
        self.window = window;
        self
    }

    fn paths_to(&self, target: i64) -> Arc<Vec<Path>> {
        if let Some(p) = self.paths.lock().get(&target) {
            return p.clone();
        }
        let mut pv = Vec::new();
        parts_of(&self.data, target, &mut pv);
        let parts: HashSet<i64> = pv.into_iter().collect();
        let width = super::state::N_FEATURES;
        let mut paths = Vec::new();
        for s in self.scored.iter() {
            let g = &s.g;
            if g.source == "me" {
                continue;
            }
            let Some(done) = g.items.iter().find(|c| c.item == target as f64) else { continue };
            let base = self.state.as_ref().map(|m| m.base).unwrap_or(0.5);
            let p_at = |ts: f64| -> f64 {
                let i = (ts / 60_000.0).floor() - 1.0;
                let Some(m) = &self.state else { return base };
                if i < 0.0 {
                    return base;
                }
                let i = i as usize;
                if let Some(pa) = &g.p_at {
                    return if i < pa.len() { pa[i] } else { m.base };
                }
                match &g.frames {
                    Some(f) if (i + 1) * width <= f.len() => {
                        let x: Vec<f64> = f[i * width..(i + 1) * width].iter().map(|v| *v as f64).collect();
                        win_probability_of(&m.beta, &x)
                    }
                    _ => m.base,
                }
            };
            // The chance at frame k (the effective window's end).
            let p_frame = |k: usize| -> Option<f64> {
                let m = self.state.as_ref()?;
                if let Some(pa) = &g.p_at {
                    return pa.get(k).copied();
                }
                let f = g.frames.as_ref().filter(|f| (k + 1) * width <= f.len())?;
                let x: Vec<f64> = f[k * width..(k + 1) * width].iter().map(|v| *v as f64).collect();
                Some(win_probability_of(&m.beta, &x))
            };
            let mut ps: Vec<(i64, f64, usize)> = g.purchases.iter().enumerate().map(|(i, (id, ts))| (*id, *ts, i)).filter(|(_, ts, _)| *ts <= done.t).collect();
            ps.sort_by(|a, b| crate::engine::recommend::js_order(a.1 - b.1));
            let mut seen = HashSet::new();
            let mut bought = Vec::new();
            for (id, ts, i) in ps {
                if parts.contains(&id) && seen.insert(id) {
                    let before = p_at(ts);
                    let end = if self.window { g.purchase_ends.get(i).and_then(|e| e.as_ref()) } else { None };
                    let after = end.map(|e| e.frame.and_then(p_frame).unwrap_or(before));
                    bought.push((id, super::window::item_delta(g.win, before, after, self.window)));
                }
            }
            paths.push(Path { lane: g.lane, opponent: g.opponent, parts: bought });
        }
        let paths = Arc::new(paths);
        self.paths.lock().insert(target, paths.clone());
        paths
    }

    /// The best purchase for `gold` toward `target` (BestBuy as JSON), or None.
    pub fn best(&self, q: &BuyQuery) -> Option<Value> {
        let name = |id: i64| self.data.item(id).map(|i| i.name.clone()).unwrap_or_else(|| format!("#{}", id));
        let nodes = missing_tree(&self.data, q.target, q.owned)?;
        let gold = q.gold.floor();
        if nodes[0].cost <= gold {
            return Some(json!({
                "target": q.target, "items": [q.target], "cost": js::num(nodes[0].cost), "gold": js::num(gold), "finishes": true, "games": 0,
                "share": null, "lift": null, "reason": format!("Enough gold to finish {}", name(q.target)),
            }));
        }
        let all = self.paths_to(q.target);
        let paths: Vec<&Path> = all.iter().filter(|p| p.lane == q.lane).collect();
        let mut open = candidates(&nodes);
        let mut items: Vec<i64> = Vec::new();
        let mut left = gold;
        let mut first: Option<(f64, Value, Value, String)> = None;
        #[derive(Default, Clone, Copy)]
        struct Vote {
            w: f64,
            n: f64,
            m: f64,
            wpa: f64,
        }
        while !open.is_empty() {
            let affordable: Vec<usize> = (0..open.len()).filter(|k| nodes[open[*k].0].cost <= left).collect();
            if affordable.is_empty() {
                break;
            }
            let ids: HashSet<i64> = open.iter().map(|c| nodes[c.0].id).collect();
            let mut votes: HashMap<i64, Vote> = HashMap::new();
            let (mut games, mut matchup_games) = (0.0, 0.0);
            for p in &paths {
                let Some(choice) = p.parts.iter().find(|x| ids.contains(&x.0)) else { continue };
                let vs = q.opponent.is_some() && p.opponent == q.opponent;
                let v = votes.entry(choice.0).or_default();
                v.w += if vs { MATCHUP_WEIGHT } else { 1.0 };
                v.n += 1.0;
                v.m += if vs { 1.0 } else { 0.0 };
                v.wpa += choice.1;
                games += 1.0;
                if vs {
                    matchup_games += 1.0;
                }
            }
            let score = |k: usize| votes.get(&nodes[open[k].0].id).map(|v| v.w).unwrap_or(0.0);
            let mut ranked = affordable.clone();
            ranked.sort_by(|a, b| crate::engine::recommend::js_order(crate::engine::recommend::or_else(score(*b) - score(*a), || nodes[open[*b].0].cost - nodes[open[*a].0].cost)));
            let mut pick = ranked[0];
            let mean = |id: i64| votes.get(&id).map(|v| if v.n != 0.0 { v.wpa / v.n } else { 0.0 }).unwrap_or(0.0);
            let pv = votes.get(&nodes[open[pick].0].id).copied();
            let better = ranked.iter().copied().find(|c| {
                let v = votes.get(&nodes[open[*c].0].id);
                *c != pick
                    && matches!((v, pv), (Some(v), Some(pv)) if v.n >= WPA_MIN_GAMES && pv.n >= WPA_MIN_GAMES)
                    && mean(nodes[open[*c].0].id) - mean(nodes[open[pick].0].id) >= WPA_LEAD
            });
            if let Some(b) = better {
                pick = b;
            }
            let pick_id = nodes[open[pick].0].id;
            if first.is_none() {
                let v = votes.get(&pick_id);
                let vs_text = match v {
                    Some(v) if matchup_games != 0.0 => format!(" ({} of {} against your opponent)", js::num_to_string(v.m), js::num_to_string(matchup_games)),
                    _ => String::new(),
                };
                first = Some(match v {
                    None => (0.0, Value::Null, Value::Null, format!("No stored games show an order here: {} spends the most of your gold", name(pick_id))),
                    Some(v) if better.is_some() => (
                        games,
                        js::num(v.n / games),
                        js::num(mean(pick_id)),
                        format!("Bought first less often than {}, but those games went better (estimate)", name(nodes[open[ranked[0]].0].id)),
                    ),
                    Some(v) => (
                        games,
                        js::num(v.n / games),
                        js::num(mean(pick_id)),
                        format!(
                            "{}% of {} high-elo games buy {} first toward {}{}",
                            js::num_to_string(js::round((v.n / games) * 100.0)),
                            js::num_to_string(games),
                            name(pick_id),
                            name(q.target),
                            vs_text
                        ),
                    ),
                });
            }
            items.push(pick_id);
            left -= nodes[open[pick].0].cost;
            let (pn, related) = (open[pick].0, open[pick].1.clone());
            open = open.into_iter().filter(|c| c.0 != pn && !related.contains(&c.0)).collect();
        }
        let first = first?;
        if items.is_empty() {
            return None;
        }
        Some(json!({
            "target": q.target, "items": items, "cost": js::num(gold - left), "gold": js::num(gold), "finishes": false,
            "games": js::num(first.0), "share": first.1, "lift": first.2, "reason": first.3,
        }))
    }
}
