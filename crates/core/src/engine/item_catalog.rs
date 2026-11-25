//! Port of `server/engine/itemCatalog.ts`: every Summoner's Rift item the Live Client can report, with what the
//! overlay's build plan needs.

use super::exclusivity::{is_boots, ExclusivityIndex};
use super::items::is_completed_item;
use crate::ddragon::{ItemMeta, StaticData};
use crate::js;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

fn recipe_of(it: &ItemMeta) -> Vec<i64> {
    if !it.from.is_empty() {
        it.from.clone()
    } else if let Some(s) = it.special_recipe.filter(|s| *s != 0) {
        vec![s]
    } else {
        vec![]
    }
}

/// What the build plan needs to know about an item (shared/buildPlan.ts PlanItem), with its name.
#[derive(Clone, Debug, PartialEq)]
pub struct MetaItem {
    pub name: String,
    pub gold: f64,
    pub groups: Vec<String>,
    pub from: Vec<i64>,
    pub boots: bool,
    pub finished: bool,
}

pub type ItemCatalog = std::collections::BTreeMap<i64, MetaItem>;

/// Record<number, MetaItem> as JSON (integer keys ascending, as JS orders them).
pub fn catalog_json(c: &ItemCatalog) -> Value {
    let mut out = Map::new();
    for (id, it) in c {
        out.insert(
            id.to_string(),
            json!({ "name": it.name, "gold": js::num(it.gold), "groups": it.groups, "from": it.from, "boots": it.boots, "finished": it.finished }),
        );
    }
    Value::Object(out)
}

pub fn item_catalog(data: &StaticData, excl: &ExclusivityIndex) -> ItemCatalog {
    let mut kinds: HashMap<i64, (bool, bool)> = HashMap::new();
    fn kind(data: &StaticData, id: i64, kinds: &mut HashMap<i64, (bool, bool)>, visiting: &mut HashSet<i64>) -> (bool, bool) {
        if let Some(k) = kinds.get(&id) {
            return *k;
        }
        let Some(it) = data.item(id) else { return (false, false) };
        if visiting.contains(&id) {
            return (false, false);
        }
        visiting.insert(id);
        let from: Vec<(bool, bool)> = recipe_of(it).into_iter().map(|c| kind(data, c, kinds, visiting)).collect();
        let boots = (is_boots(it) && it.depth >= 2) || from.iter().any(|c| c.0);
        let k = (boots, boots || is_completed_item(Some(it)) || from.iter().any(|c| c.1));
        kinds.insert(id, k);
        k
    }
    let mut out = ItemCatalog::new();
    for it in data.items.values() {
        if !it.on_summoners_rift || it.id >= 100_000 {
            continue;
        }
        let (boots, finished) = kind(data, it.id, &mut kinds, &mut HashSet::new());
        out.insert(it.id, MetaItem { name: it.name.clone(), gold: it.gold_total, groups: excl.groups_of(it.id), from: recipe_of(it), boots, finished });
    }
    out
}
