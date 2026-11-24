//! Port of `server/engine/laneOpponent.ts`: the likely lane opponent among the enemy picks (ranked hides roles).

use crate::ddragon::ChampionMeta;
use std::collections::HashMap;

fn lane_classes(lane: &str) -> &'static [&'static str] {
    if lane == "TOP" {
        &["Fighter", "Tank"]
    } else {
        &["Mage", "Assassin"]
    }
}

const MIN_SHARE_GAMES: f64 = 20.0;

pub type Faced = HashMap<i64, f64>;

fn tags_of<'a>(champions: &'a dyn Fn(i64) -> Option<&'a ChampionMeta>, id: i64) -> &'a [String] {
    champions(id).map(|c| c.tags.as_slice()).unwrap_or(&[])
}

/// The pick Irelia faced most often in that lane, else the first whose main class fits, else one whose second class
/// does, else the first pick.
pub fn guess_lane_opponent<'a>(enemy_ids: &[i64], lane: &str, faced: &Faced, champions: &'a dyn Fn(i64) -> Option<&'a ChampionMeta>) -> Option<i64> {
    let mut ranked = enemy_ids.to_vec();
    let g = |id: &i64| faced.get(id).copied().unwrap_or(0.0);
    ranked.sort_by(|a, b| crate::engine::recommend::js_order(g(b) - g(a)));
    if let Some(first) = ranked.first() {
        if g(first) != 0.0 && !g(first).is_nan() {
            return Some(*first);
        }
    }
    let classes = lane_classes(lane);
    enemy_ids
        .iter()
        .find(|id| tags_of(champions, **id).first().map(|t| classes.contains(&t.as_str())).unwrap_or(false))
        .or_else(|| enemy_ids.iter().find(|id| tags_of(champions, **id).iter().any(|t| classes.contains(&t.as_str()))))
        .or_else(|| enemy_ids.first())
        .copied()
}

/// The likely opponent for each of `lanes`, never the same pick for two lanes.
pub fn guess_lane_opponents<'a>(enemy_ids: &[i64], lanes: &[&'static str], faced: &HashMap<&'static str, Faced>, champions: &'a dyn Fn(i64) -> Option<&'a ChampionMeta>) -> Vec<(&'static str, Option<i64>)> {
    let none = Faced::new();
    let fit = |id: i64, lane: &str| -> (f64, f64) {
        let games = faced.get(lane).unwrap_or(&none);
        let total = crate::model::logistic::js_max(MIN_SHARE_GAMES, games.values().fold(0.0, |s, n| s + n));
        let tags = tags_of(champions, id);
        let classes = lane_classes(lane);
        let class = if tags.first().map(|t| classes.contains(&t.as_str())).unwrap_or(false) {
            2.0
        } else if tags.iter().any(|t| classes.contains(&t.as_str())) {
            1.0
        } else {
            0.0
        };
        (games.get(&id).copied().unwrap_or(0.0) / total, class)
    };
    let mut out: Vec<(&'static str, Option<i64>)> = Vec::new();
    let mut pool = enemy_ids.to_vec();
    let mut open: Vec<&'static str> = lanes.to_vec();
    while !open.is_empty() {
        let mut guesses: Vec<(&'static str, i64)> =
            open.iter().filter_map(|lane| guess_lane_opponent(&pool, lane, faced.get(lane).unwrap_or(&none), champions).map(|id| (*lane, id))).collect();
        if guesses.is_empty() {
            break;
        }
        guesses.sort_by(|a, b| {
            let (fa, fb) = (fit(a.1, a.0), fit(b.1, b.0));
            crate::engine::recommend::js_order(crate::engine::recommend::or_else(fb.0 - fa.0, || fb.1 - fa.1))
        });
        let (lane, id) = guesses[0];
        out.push((lane, Some(id)));
        pool.retain(|x| *x != id);
        open.retain(|l| *l != lane);
    }
    for lane in open {
        out.push((lane, None));
    }
    out
}
