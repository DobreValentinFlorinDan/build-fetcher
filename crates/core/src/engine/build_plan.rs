//! Port of `shared/buildPlan.ts`: the overlay's build plan and which entries your inventory already covers. (The web
//! keeps its TS copy; the server needs the plan's next entry for "Next" in lane-baseline mode.)

use super::item_catalog::ItemCatalog;
use crate::types::BuildRecommendation;
use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq)]
pub struct PlanEntry {
    pub id: i64,
    pub slot: &'static str,
    pub alternatives: Vec<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlanStatus {
    pub entry: PlanEntry,
    pub state: &'static str,
    pub owned: Option<i64>,
    pub components: Vec<i64>,
    pub cost: f64,
}

fn unique(ids: Vec<i64>) -> Vec<i64> {
    let mut out = Vec::new();
    for id in ids {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

pub fn build_plan(rec: &BuildRecommendation, items: &ItemCatalog) -> Vec<PlanEntry> {
    let core: Vec<i64> = rec.core_build.first().map(|o| o.value.clone()).unwrap_or_default();
    let mut plan: Vec<PlanEntry> = core
        .iter()
        .enumerate()
        .map(|(k, id)| PlanEntry {
            id: *id,
            slot: "Core",
            alternatives: unique(rec.core_build.iter().skip(1).filter_map(|o| o.value.get(k).copied()).filter(|x| !core.contains(x)).collect()),
        })
        .collect();
    if let Some(b) = rec.boots.first() {
        let after = rec.boots_after.flatten().unwrap_or(core.len() as f64);
        let at = crate::model::logistic::js_max(0.0, crate::model::logistic::js_min(core.len() as f64, after));
        let at = if at.is_nan() { 0 } else { at as usize };
        plan.insert(at.min(plan.len()), PlanEntry { id: b.value, slot: "Boots", alternatives: rec.boots.iter().skip(1).map(|o| o.value).collect() });
    }
    let groups = |id: i64| items.get(&id).map(|i| i.groups.clone()).unwrap_or_default();
    for (options, slot) in [(&rec.situational.slot4, "4th"), (&rec.situational.slot5, "5th"), (&rec.situational.slot6, "6th")] {
        let options: Vec<i64> = options.iter().map(|o| o.value).collect();
        let planned: Vec<i64> = plan.iter().map(|p| p.id).collect();
        let pick = options.iter().copied().find(|id| !planned.contains(id) && !planned.iter().any(|p| groups(*p).iter().any(|g| groups(*id).contains(g))));
        if let Some(pick) = pick {
            plan.push(PlanEntry { id: pick, slot, alternatives: options.iter().copied().filter(|id| *id != pick && !planned.contains(id)).collect() });
        }
    }
    plan
}

fn ancestors(id: i64, items: &ItemCatalog) -> HashSet<i64> {
    let mut seen = HashSet::new();
    let mut stack: Vec<i64> = items.get(&id).map(|i| i.from.clone()).unwrap_or_default();
    while let Some(x) = stack.pop() {
        if !seen.insert(x) {
            continue;
        }
        stack.extend(items.get(&x).map(|i| i.from.clone()).unwrap_or_default());
    }
    seen
}

fn owned_components(id: i64, pool: &[i64], items: &ItemCatalog) -> Vec<i64> {
    let mut left = pool.to_vec();
    let mut used = Vec::new();
    fn walk(x: i64, left: &mut Vec<i64>, used: &mut Vec<i64>, items: &ItemCatalog) {
        for c in items.get(&x).map(|i| i.from.clone()).unwrap_or_default() {
            if let Some(i) = left.iter().position(|v| *v == c) {
                used.push(c);
                left.remove(i);
            } else {
                walk(c, left, used, items);
            }
        }
    }
    walk(id, &mut left, &mut used, items);
    used
}

/// Matches your inventory against the plan; `next` (the index into the entries) is the first entry not yet filled.
pub fn plan_status(plan: &[PlanEntry], owned_ids: &[i64], items: &ItemCatalog) -> (Vec<PlanStatus>, Option<usize>) {
    let mut pool: Vec<i64> = owned_ids.iter().copied().filter(|id| *id > 0).collect();
    let mut entries: Vec<PlanStatus> = plan
        .iter()
        .map(|e| PlanStatus { entry: e.clone(), state: "todo", owned: None, components: vec![], cost: items.get(&e.id).map(|i| i.gold).unwrap_or(0.0) })
        .collect();
    fn take(pool: &mut Vec<i64>, id: i64) -> i64 {
        if let Some(i) = pool.iter().position(|x| *x == id) {
            pool.remove(i);
        }
        id
    }
    let finished = |id: i64| items.get(&id).map(|i| i.finished).unwrap_or(false);
    let shares_group = |a: i64, b: i64| {
        let gb = items.get(&b).map(|i| i.groups.clone()).unwrap_or_default();
        items.get(&a).map(|i| i.groups.iter().any(|g| gb.contains(g))).unwrap_or(false)
    };
    let open = |entries: &Vec<PlanStatus>| entries.iter().enumerate().filter(|(_, e)| e.state == "todo").map(|(i, _)| i).collect::<Vec<_>>();
    for i in open(&entries) {
        let id = entries[i].entry.id;
        if pool.contains(&id) {
            entries[i].state = "done";
            entries[i].owned = Some(take(&mut pool, id));
            entries[i].cost = 0.0;
        }
    }
    for i in open(&entries) {
        let id = entries[i].entry.id;
        if let Some(up) = pool.iter().copied().find(|x| ancestors(*x, items).contains(&id)) {
            entries[i].state = "upgraded";
            entries[i].owned = Some(take(&mut pool, up));
            entries[i].cost = 0.0;
        }
    }
    let can_swap = |e: &PlanStatus, id: i64| finished(id) && !ancestors(e.entry.id, items).contains(&id);
    for i in open(&entries) {
        let e = entries[i].clone();
        let hard = pool.iter().copied().find(|id| {
            can_swap(&e, *id) && if e.entry.slot == "Boots" { items.get(id).map(|x| x.boots).unwrap_or(false) } else { shares_group(*id, e.entry.id) }
        });
        if let Some(h) = hard {
            entries[i].state = "swap";
            entries[i].owned = Some(take(&mut pool, h));
            entries[i].cost = 0.0;
        }
    }
    for i in open(&entries) {
        let e = entries[i].clone();
        if let Some(a) = pool.iter().copied().find(|id| can_swap(&e, *id) && e.entry.alternatives.contains(id)) {
            entries[i].state = "swap";
            entries[i].owned = Some(take(&mut pool, a));
            entries[i].cost = 0.0;
        }
    }
    for i in open(&entries) {
        let comps = owned_components(entries[i].entry.id, &pool, items);
        let comps: Vec<i64> = comps.into_iter().map(|c| take(&mut pool, c)).collect();
        let paid = comps.iter().fold(0.0, |s, id| s + items.get(id).map(|x| x.gold).unwrap_or(0.0));
        entries[i].cost = crate::model::logistic::js_max(0.0, entries[i].cost - paid);
        if !comps.is_empty() {
            entries[i].state = "building";
        }
        entries[i].components = comps;
    }
    let next = entries.iter().position(|e| e.state == "building" || e.state == "todo");
    (entries, next)
}
