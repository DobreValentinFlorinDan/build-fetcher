//! Port of `server/engine/validator.ts`: patch-aware validation of stored games.

use super::exclusivity::{is_boots, is_purchasable_item, is_removed};
use crate::capture::REMAKE_MAX_SECONDS;
use crate::db::IreliaGameRow;
use crate::ddragon::{ItemMeta, StaticData};
use std::sync::Arc;

pub const STAT_SHARDS: [i64; 7] = [5001, 5005, 5007, 5008, 5010, 5011, 5013];

/// A completed ("legendary") item.
pub fn is_completed_item(item: Option<&ItemMeta>) -> bool {
    match item {
        Some(i) if is_purchasable_item(item) => {
            !is_boots(i)
                && i.into.is_empty()
                && i.gold_total >= 2000.0
                && !i.tags.iter().any(|t| t == "Consumable")
                && !i.tags.iter().any(|t| t == "Trinket")
        }
        _ => false,
    }
}

pub fn is_tier2_boots(item: Option<&ItemMeta>) -> bool {
    match item {
        Some(i) => is_purchasable_item(item) && is_boots(i) && i.depth >= 2,
        None => false,
    }
}

pub fn is_starting_item(item: Option<&ItemMeta>) -> bool {
    match item {
        Some(i) => is_purchasable_item(item) && !i.tags.iter().any(|t| t == "Trinket") && i.gold_total > 0.0,
        None => false,
    }
}

#[derive(Clone, Debug)]
pub struct ParsedGame {
    pub row: Arc<IreliaGameRow>,
    pub weight: f64,
    pub win: bool,
    pub perks: Vec<i64>,
    pub stat_perks: Vec<i64>,
    pub purchases: Vec<(i64, f64)>,
    pub completed: Vec<i64>,
    pub boots: Option<i64>,
    pub start: Vec<i64>,
}

#[derive(Clone, Debug)]
pub enum Verdict {
    Ok(ParsedGame),
    Bad { reason: String, stale: bool },
}

fn parse_ints(s: &str) -> Vec<i64> {
    serde_json::from_str::<Vec<serde_json::Value>>(s)
        .map(|v| v.iter().map(|x| x.as_f64().map(|f| f as i64).unwrap_or(i64::MIN)).collect())
        .unwrap_or_default()
}

pub fn parse_purchases(s: &str) -> Vec<(i64, f64)> {
    serde_json::from_str::<Vec<Vec<serde_json::Value>>>(s)
        .map(|v| {
            v.iter()
                .map(|p| (p.first().and_then(|x| x.as_f64()).unwrap_or(0.0) as i64, p.get(1).and_then(|x| x.as_f64()).unwrap_or(f64::NAN)))
                .collect()
        })
        .unwrap_or_default()
}

/// Patch-aware validation. `patch_weight` gives the game's patch weight on its own platform (0/None: stale).
pub fn validate_game(row: &Arc<IreliaGameRow>, data: &StaticData, patch_weight: &dyn Fn(&IreliaGameRow) -> Option<f64>, start_window_ms: f64) -> Verdict {
    let bad = |reason: String, stale: bool| Verdict::Bad { reason, stale };
    let weight = match patch_weight(row) {
        Some(w) if w != 0.0 && !w.is_nan() => w,
        _ => return bad(format!("stale patch {}", row.patch), true),
    };
    if row.remake == Some(1) || (row.game_duration as f64) < REMAKE_MAX_SECONDS {
        return bad("remake".into(), false);
    }
    if row.leaver == Some(1) {
        return bad("a player left the game".into(), false);
    }
    if let Some(r) = row.end_result.as_deref().filter(|r| !r.is_empty()) {
        if r != "GameComplete" {
            return bad("aborted game".into(), false);
        }
    }
    let perks = parse_ints(&row.perks);
    let stat_perks = parse_ints(&row.stat_perks);
    if perks.len() != 6
        || !perks.iter().all(|p| data.rune_ids.contains(p))
        || !data.rune_ids.contains(&row.primary_style)
        || !data.rune_ids.contains(&row.sub_style)
    {
        return bad("rune no longer exists".into(), false);
    }
    if !stat_perks.iter().all(|s| STAT_SHARDS.contains(s)) {
        return bad("stat shard no longer exists".into(), false);
    }
    let purchases = parse_purchases(&row.purchases);
    for &(id, _) in &purchases {
        let item = data.items.get(&id);
        if is_removed(id) {
            return bad(format!("removed item: {}", item.map(|i| i.name.clone()).unwrap_or_else(|| id.to_string())), false);
        }
        let Some(it) = item else { return bad(format!("unknown item #{id}"), false) };
        if it.into.is_empty() && it.gold_total >= 2000.0 && !is_purchasable_item(item) {
            return bad(format!("removed item: {}", it.name), false);
        }
    }
    let mut completed: Vec<i64> = Vec::new();
    let mut boots = None;
    let mut start: Vec<i64> = Vec::new();
    for &(id, ts) in &purchases {
        let item = data.items.get(&id);
        if ts < start_window_ms && is_starting_item(item) {
            start.push(id);
        }
        if is_completed_item(item) && !completed.contains(&id) {
            completed.push(id);
        }
        if boots.is_none() && is_tier2_boots(item) {
            boots = Some(id);
        }
    }
    start.sort();
    Verdict::Ok(ParsedGame { row: row.clone(), weight, win: row.win == 1, perks, stat_perks, purchases, completed, boots, start })
}
