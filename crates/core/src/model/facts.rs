//! Port of `server/model/facts.ts` (Stage C2): how a matchup tends to go, and what did best in it.

use super::dataset::{PreparedGame, ScoredGame};
use crate::capture::median;
use crate::engine::recommend::{js_order, or_else};
use crate::js;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

pub const LENGTH_BUCKETS: [(f64, Option<f64>); 4] = [(0.0, Some(25.0)), (25.0, Some(30.0)), (30.0, Some(35.0)), (35.0, None)];

fn gold_at(values: Vec<Option<f64>>) -> Value {
    let xs: Vec<f64> = values.into_iter().flatten().collect();
    if xs.is_empty() {
        return Value::Null;
    }
    json!({ "median": js::num(js::round(median(&xs))), "ahead": js::num(xs.iter().filter(|x| **x > 0.0).count() as f64 / xs.len() as f64), "games": xs.len() })
}

/// MatchupFacts as JSON; None without an opponent or with fewer than `min_games` games against it in this lane.
pub fn matchup_facts(games: &[Arc<PreparedGame>], lane: &str, opponent: Option<i64>, min_games: usize) -> Option<Value> {
    let opponent = opponent?;
    let vs: Vec<&Arc<PreparedGame>> = games.iter().filter(|g| g.source != "me" && g.lane == lane && g.opponent == Some(opponent)).collect();
    if vs.len() < min_games {
        return None;
    }
    let rate = |gs: &[&Arc<PreparedGame>]| if gs.is_empty() { 0.0 } else { gs.iter().filter(|g| g.win != 0.0).count() as f64 / gs.len() as f64 };
    let pilots: HashSet<&str> = vs.iter().map(|g| g.pilot.as_str()).collect();
    let by_length: Vec<Value> = LENGTH_BUCKETS
        .iter()
        .filter_map(|(from, to)| {
            let inside: Vec<&Arc<PreparedGame>> = vs.iter().copied().filter(|g| g.duration / 60.0 >= *from && to.map(|t| g.duration / 60.0 < t).unwrap_or(true)).collect();
            (!inside.is_empty()).then(|| json!({ "from": js::num(*from), "to": to.map(js::num), "games": inside.len(), "winRate": js::num(rate(&inside)) }))
        })
        .collect();
    Some(json!({
        "lane": lane, "opponentId": opponent, "games": vs.len(), "pilots": pilots.len(), "winRate": js::num(rate(&vs)),
        "gold10": gold_at(vs.iter().map(|g| g.lane10).collect()), "gold15": gold_at(vs.iter().map(|g| g.lane15).collect()),
        "byLength": by_length,
    }))
}

pub const BEST_MIN_MATCHUP: usize = 15;
pub const BEST_MIN_OPTION: f64 = 5.0;
pub const BEST_PRIOR: f64 = 20.0;

/// MatchupBest as JSON: the start and the first items that did best against `opponent` in this lane.
pub fn matchup_best(scored: &[ScoredGame], lane: &str, opponent: Option<i64>, buyable: &dyn Fn(i64) -> bool) -> Option<Value> {
    let opponent = opponent?;
    let vs: Vec<&ScoredGame> = scored.iter().filter(|s| s.g.source != "me" && s.g.lane == lane && s.g.opponent == Some(opponent)).collect();
    if vs.len() < BEST_MIN_MATCHUP {
        return None;
    }
    fn rank(rows: Vec<(String, Value, f64)>) -> Vec<(Value, f64, f64)> {
        let mut by: indexmap::IndexMap<String, (Value, f64, f64)> = indexmap::IndexMap::new();
        for (k, v, wpa) in rows {
            let e = by.entry(k).or_insert((v, 0.0, 0.0));
            e.1 += 1.0;
            e.2 += wpa;
        }
        let mut out: Vec<(Value, f64, f64)> = by.into_values().filter(|e| e.1 >= BEST_MIN_OPTION).map(|e| (e.0, e.1, e.2 / (e.1 + BEST_PRIOR))).collect();
        out.sort_by(|a, b| js_order(or_else(b.2 - a.2, || b.1 - a.1)));
        out
    }
    let num = |s: &str| js::to_number(&Value::String(s.into()));
    let starts = rank(
        vs.iter()
            .filter(|s| !s.g.start.is_empty() && s.g.start.split(',').all(|id| { let n = num(id); n.fract() == 0.0 && buyable(n as i64) }))
            .map(|s| (s.g.start.clone(), json!(s.g.start.split(',').map(|x| num(x) as i64).collect::<Vec<_>>()), s.wpa0))
            .collect(),
    );
    let firsts = rank(
        vs.iter()
            .filter(|s| s.g.items.first().map(|c| buyable(c.item as i64)).unwrap_or(false))
            .map(|s| (js::num_to_string(s.g.items[0].item), json!(s.g.items[0].item as i64), s.item_wpa[0]))
            .collect(),
    );
    if starts.is_empty() && firsts.is_empty() {
        return None;
    }
    let opt = |o: &(Value, f64, f64)| json!({ "value": o.0, "games": js::num(o.1), "lift": js::num(o.2) });
    Some(json!({ "start": starts.first().map(opt), "first": firsts.iter().take(2).map(opt).collect::<Vec<_>>() }))
}
