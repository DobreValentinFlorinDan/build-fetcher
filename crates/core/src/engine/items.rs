//! Item classes over Data Dragon (from irelia-companion engine/validator.rs): completed items, tier-2 boots, starting items.

use super::exclusivity::{is_boots, is_purchasable_item};
use crate::ddragon::ItemMeta;

/// Stat shard perk ids (the three small runes under the trees).
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
