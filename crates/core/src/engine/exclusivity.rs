//! Port of `server/engine/exclusivity.ts`: unique-passive groups the shop enforces.

use crate::ddragon::{ItemMeta, StaticData};
use std::collections::HashMap;

const EXCLUSIVE_PASSIVES: [&str; 9] =
    ["Immolate", "Lifeline", "Spellblade", "Cleave", "Manaflow", "Annul", "Glory", "Eternity", "Jungle Companions"];

const STATIC_GROUPS: [(&str, &[i64]); 3] = [
    ("Last Whisper", &[3035, 3036, 3033, 6694]),
    ("Quicksilver", &[3140, 3139]),
    ("Support Quest", &[3865, 3866, 3867, 3869, 3870, 3871, 3876, 3877]),
];

/// Items removed from the game (Data Dragon sometimes still ships them).
pub const REMOVED_ITEMS: [i64; 1] = [6701];

pub fn is_removed(id: i64) -> bool {
    REMOVED_ITEMS.contains(&id)
}

pub fn is_purchasable_item(item: Option<&ItemMeta>) -> bool {
    match item {
        Some(i) => i.on_summoners_rift && i.purchasable && i.in_store && !is_removed(i.id) && i.id < 100_000,
        None => false,
    }
}

pub fn is_boots(item: &ItemMeta) -> bool {
    item.tags.iter().any(|t| t == "Boots")
}

#[derive(Clone, Debug, PartialEq)]
pub struct Conflict {
    pub group: String,
    pub items: Vec<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct ExclusivityIndex {
    groups: HashMap<i64, Vec<String>>,
    names: HashMap<i64, String>,
}

impl ExclusivityIndex {
    pub fn groups_of(&self, id: i64) -> Vec<String> {
        self.groups.get(&id).cloned().unwrap_or_default()
    }

    fn groups_ref(&self, id: i64) -> &[String] {
        self.groups.get(&id).map(|v| v.as_slice()).unwrap_or(&[])
    }

    fn name(&self, id: i64) -> String {
        self.names.get(&id).cloned().unwrap_or_else(|| format!("#{id}"))
    }

    pub fn conflicts(&self, item_ids: &[i64]) -> Vec<Conflict> {
        let mut by_group: Vec<(String, Vec<i64>)> = Vec::new();
        for &id in item_ids {
            for g in self.groups_ref(id) {
                match by_group.iter_mut().find(|(n, _)| n == g) {
                    Some(e) => e.1.push(id),
                    None => by_group.push((g.clone(), vec![id])),
                }
            }
        }
        by_group.into_iter().filter(|(_, ids)| ids.len() > 1).map(|(group, items)| Conflict { group, items }).collect()
    }

    /// Group name that blocks `candidate` given `owned`, or None if it can be added.
    pub fn blocker(&self, owned: &[i64], candidate: i64) -> Option<String> {
        let cand = self.groups_ref(candidate);
        for &id in owned {
            if id == candidate {
                return Some("Duplicate".into());
            }
            for g in self.groups_ref(id) {
                if cand.contains(g) {
                    return Some(format!("{} ({})", g, self.name(id)));
                }
            }
        }
        None
    }

    pub fn describe(&self, c: &Conflict) -> String {
        format!("{} ({})", c.items.iter().map(|i| self.name(*i)).collect::<Vec<_>>().join(" + "), c.group)
    }
}

pub fn build_exclusivity_index(data: &StaticData) -> ExclusivityIndex {
    let mut groups: HashMap<i64, Vec<String>> = HashMap::new();
    let mut add = |id: i64, g: &str| {
        let e = groups.entry(id).or_default();
        if !e.iter().any(|x| x == g) {
            e.push(g.to_string());
        }
    };
    for item in data.items.values() {
        if !item.on_summoners_rift || item.id >= 100_000 {
            continue;
        }
        for p in &item.passives {
            if EXCLUSIVE_PASSIVES.contains(&p.as_str()) {
                add(item.id, p);
            }
        }
        if is_boots(item) {
            add(item.id, "Boots");
        }
    }
    for (g, ids) in STATIC_GROUPS {
        for id in ids {
            add(*id, g);
        }
    }
    let names = data.items.iter().map(|(id, it)| (*id, it.name.clone())).collect();
    ExclusivityIndex { groups, names }
}
