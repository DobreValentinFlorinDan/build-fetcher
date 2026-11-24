//! Port of `server/model/swaps.ts`: late-game swap decisions (0.17.0, docs/SWAPS.md).

use crate::capture::{TimelineCapture, BUY, DESTROY, SELL as EV_SELL, UNDO};
use crate::ddragon::ItemMeta;
use crate::engine::validator::{is_completed_item, is_tier2_boots};

#[derive(Clone, Copy, Debug)]
pub struct SwapOptions {
    pub from_minute: f64,
    pub visit_gap_ms: f64,
    pub replace_within_ms: f64,
    pub full_slots: usize,
}
pub const SWAP_DEFAULTS: SwapOptions = SwapOptions { from_minute: 20.0, visit_gap_ms: 30_000.0, replace_within_ms: 10.0 * 60_000.0, full_slots: 6 };

/// Kept the item; sold it with no legendary finished in time. Any other option is the item it was sold for.
pub const KEEP: f64 = 0.0;
pub const SELL: f64 = -1.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SwapDecision {
    pub t: f64,
    pub held: f64,
    pub option: f64,
    pub w: f64,
}

pub fn swappable(item: Option<&ItemMeta>) -> bool {
    is_completed_item(item) || is_tier2_boots(item)
}

pub fn swap_decisions<'d>(cap: &TimelineCapture, pid: f64, item_of: &dyn Fn(f64) -> Option<&'d ItemMeta>, o: &SwapOptions) -> Vec<SwapDecision> {
    let mut events: Vec<(f64, f64, f64, f64, Option<f64>)> = cap.items.iter().copied().filter(|e| e.1 == pid).collect();
    events.sort_by(|a, b| crate::engine::recommend::js_order(a.0 - b.0));
    let mut inv: Vec<f64> = Vec::new();
    let apply = |inv: &mut Vec<f64>, e: &(f64, f64, f64, f64, Option<f64>)| {
        let (_, _, kind, item, after) = *e;
        let remove = |inv: &mut Vec<f64>, id: f64| {
            if let Some(i) = inv.iter().rposition(|x| *x == id) {
                inv.remove(i);
            }
        };
        if kind == BUY {
            inv.push(item);
        } else if kind == EV_SELL || kind == DESTROY {
            remove(inv, item);
        } else if kind == UNDO {
            if item != 0.0 && !item.is_nan() {
                remove(inv, item);
            }
            if let Some(a) = after.filter(|a| *a != 0.0 && !a.is_nan()) {
                inv.push(a);
            }
        }
    };
    let finished: Vec<usize> = (0..events.len()).filter(|&k| events[k].2 == BUY && is_completed_item(item_of(events[k].3))).collect();
    let mut points: Vec<(f64, Vec<f64>, Vec<f64>)> = Vec::new();
    let mut claimed: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < events.len() {
        let mut j = i + 1;
        while j < events.len() && events[j].0 - events[j - 1].0 <= o.visit_gap_ms {
            j += 1;
        }
        let visit = &events[i..j];
        let start = visit[0].0;
        let held: Vec<f64> = inv.iter().copied().filter(|id| swappable(item_of(*id))).collect();
        for e in visit {
            apply(&mut inv, e);
        }
        if start >= o.from_minute * 60_000.0 && held.len() >= o.full_slots {
            let mut after = inv.clone();
            let mut gone: Vec<f64> = Vec::new();
            for id in &held {
                if let Some(k) = after.iter().position(|x| x == id) {
                    after.remove(k);
                } else {
                    gone.push(*id);
                }
            }
            let mut options = Vec::with_capacity(held.len());
            for id in &held {
                let Some(g) = gone.iter().position(|x| x == id) else {
                    options.push(KEEP);
                    continue;
                };
                gone.remove(g);
                let sell_at = visit.iter().rev().find(|e| e.2 == EV_SELL && e.3 == *id).map(|e| e.0).unwrap_or(start);
                let next = finished.iter().copied().find(|&b| {
                    let be = &events[b];
                    !claimed.contains(&b) && be.0 >= start && be.0 - sell_at <= o.replace_within_ms && be.3 != *id && !held.contains(&be.3)
                });
                match next {
                    None => options.push(SELL),
                    Some(b) => {
                        claimed.push(b);
                        options.push(events[b].3);
                    }
                }
            }
            points.push((start, held, options));
        }
        i = j;
    }
    let w = if points.is_empty() { 0.0 } else { 1.0 / points.len() as f64 };
    points.iter().flat_map(|(t, held, options)| held.iter().enumerate().map(move |(k, h)| SwapDecision { t: *t, held: *h, option: options[k], w })).collect()
}
