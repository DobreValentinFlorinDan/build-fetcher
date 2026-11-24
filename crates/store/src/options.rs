//! Every `BuildOption` a player's record shows, and the enemy team's damage profile.
//!
//! Options (always through the `BuildOption` constructors, so keys are canonical):
//! - RunePage, Keystone, Summoners: from the match itself;
//! - SkillMax, SkillOrder: from the timeline's SKILL_LEVEL_UP events (NORMAL ones);
//! - StartingItems: purchases before 60 s (undos already applied, minus anything sold before 60 s), trinkets aside;
//! - FirstItem, CoreItems, LateItem: completed items in purchase order (first purchase of each), the 1st, the first
//!   three, and each one from the 4th on. A completed item is a purchasable non-boots item that `engine::items`
//!   calls completed, or one worth 2000+ gold whose upgrades can't be bought (Manamune → Muramana);
//! - Boots: the first tier-2 boots bought, else the tier-2 boots in the final inventory.
//!
//! Order-dependent options need the purchase order, which only the timeline has: games without one give RunePage,
//! Keystone, Summoners and Boots only (final-inventory slots are not purchase order: players move items around).
//!
//! Damage profile: `bf_core::build::DamageProfile::of_team`, the one rule the store's counts and the learner's
//! queries share (a count stored under "Magic" must be the count the recommender asks for under "Magic").

use bf_core::build::{BuildOption, DamageProfile, MatchRecord, PlayerRecord, Role};
use bf_core::ddragon::StaticData;
use bf_core::engine::exclusivity::{is_boots, is_purchasable_item};
use bf_core::engine::items::{is_completed_item, is_starting_item, is_tier2_boots};

pub const STARTING_WINDOW_S: u32 = 60;

pub fn is_completed(data: &StaticData, id: i64) -> bool {
    let item = data.item(id);
    if is_completed_item(item) {
        return true;
    }
    match item {
        Some(i) if is_purchasable_item(item) && !is_boots(i) && i.gold_total >= 2000.0 => i.into.iter().all(|u| !is_purchasable_item(data.item(*u))),
        _ => false,
    }
}

/// Completed items in the order they were first bought.
pub fn completed_order(p: &PlayerRecord, data: &StaticData) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    for (item, _) in &p.purchases {
        if *item > 0 && !out.contains(item) && is_completed(data, *item) {
            out.push(*item);
        }
    }
    out
}

pub fn player_options(p: &PlayerRecord, data: &StaticData, has_timeline: bool) -> Vec<BuildOption> {
    let mut out = Vec::with_capacity(12);
    if p.rune_style > 0 && p.perks.iter().all(|x| *x > 0) {
        out.push(BuildOption::rune_page(p.rune_style, &p.perks, p.rune_sub_style, &p.sub_perks, &p.shards));
        out.push(BuildOption::keystone(p.perks[0]));
    }
    if p.summoners.iter().all(|s| *s > 0) {
        out.push(BuildOption::summoners(p.summoners[0], p.summoners[1]));
    }
    if !p.skill_order.is_empty() {
        out.extend(BuildOption::skill_max(&p.skill_order));
        out.extend(BuildOption::skill_order(&p.skill_order));
    }
    let mut boots = None;
    if has_timeline {
        let mut start: Vec<i64> = Vec::new();
        for (item, t) in &p.purchases {
            if *t >= STARTING_WINDOW_S {
                break;
            }
            if *item > 0 && is_starting_item(data.item(*item)) {
                start.push(*item);
            } else if *item < 0 {
                if let Some(i) = start.iter().position(|x| *x == -item) {
                    start.remove(i);
                }
            }
        }
        out.extend(BuildOption::starting_items(&start));
        let done = completed_order(p, data);
        if let Some(first) = done.first() {
            out.push(BuildOption::first_item(*first));
        }
        out.extend(BuildOption::core_items(&done));
        for it in done.iter().skip(3) {
            out.push(BuildOption::late_item(*it));
        }
        boots = p.purchases.iter().find(|(i, _)| *i > 0 && is_tier2_boots(data.item(*i))).map(|x| x.0);
    }
    if let Some(b) = boots.or_else(|| p.final_items.iter().copied().find(|i| *i > 0 && is_tier2_boots(data.item(*i)))) {
        out.push(BuildOption::boots(b));
    }
    out
}

/// The damage profile of a team (roles are accepted for callers' convenience; the shared rule ignores them).
pub fn damage_profile(team: &[(i64, Option<Role>)], data: &StaticData) -> DamageProfile {
    let champions: Vec<i64> = team.iter().map(|(c, _)| *c).collect();
    DamageProfile::of_team(data, &champions).unwrap_or(DamageProfile::Mixed)
}

/// The damage profile of the team facing player `i`.
pub fn enemy_profile(r: &MatchRecord, i: usize, data: &StaticData) -> DamageProfile {
    let me = r.players[i].team;
    let enemies: Vec<(i64, Option<Role>)> = r.players.iter().filter(|p| p.team != me).map(|p| (p.champion, p.role)).collect();
    damage_profile(&enemies, data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bf_core::build::OptionKind;

    #[test]
    fn options_from_a_timeline_player() {
        let data = bf_core::testkit::load_test_static_data();
        let p = PlayerRecord {
            champion: 39,
            role: Some(Role::Top),
            rune_style: 8000,
            perks: [8010, 9111, 9104, 8299],
            rune_sub_style: 8400,
            sub_perks: [8444, 8473],
            shards: [5008, 5008, 5011],
            summoners: [12, 4],
            final_items: [3153, 3078, 3047, 0, 0, 0, 3340],
            // Doran's + potion, a ward sold back, boots, Trinity, Steelcaps, BotRK, Sterak's, Death's Dance, BotRK again.
            purchases: vec![(1055, 10), (2003, 11), (2055, 20), (-2055, 30), (1001, 300), (3078, 600), (3047, 700), (3153, 1100), (3053, 1500), (3074, 1700), (3153, 1900)],
            skill_order: vec![1, 3, 2, 1, 1, 4, 1, 3, 1, 3, 4, 3, 3, 2, 2, 4, 2, 2],
            ..Default::default()
        };
        let o = player_options(&p, &data, true);
        let key = |k: OptionKind| o.iter().filter(|x| x.kind == k).map(|x| x.key.clone()).collect::<Vec<_>>();
        assert_eq!(key(OptionKind::Keystone), vec!["8010"]);
        assert_eq!(key(OptionKind::Summoners), vec!["4,12"]);
        assert_eq!(key(OptionKind::SkillMax), vec!["Q>E>W"]);
        assert_eq!(key(OptionKind::SkillOrder), vec!["Q,E,W,Q,Q,R"]);
        assert_eq!(key(OptionKind::StartingItems), vec!["1055,2003"]);
        assert_eq!(key(OptionKind::FirstItem), vec!["3078"]);
        assert_eq!(key(OptionKind::CoreItems), vec!["3078>3153>3053"]);
        assert_eq!(key(OptionKind::LateItem), vec!["3074"]);
        assert_eq!(key(OptionKind::Boots), vec!["3047"]);
        assert_eq!(key(OptionKind::RunePage).len(), 1);
        // Without a timeline: no order-dependent options, boots from the inventory.
        let o = player_options(&PlayerRecord { purchases: vec![], skill_order: vec![], ..p }, &data, false);
        let kinds: Vec<OptionKind> = o.iter().map(|x| x.kind).collect();
        assert_eq!(kinds, vec![OptionKind::RunePage, OptionKind::Keystone, OptionKind::Summoners, OptionKind::Boots]);
    }

    #[test]
    fn damage_profiles() {
        let data = bf_core::testkit::load_test_static_data();
        let id = |name: &str| data.champion_by_key(name).unwrap().id;
        let mages = [("Syndra", None), ("Lux", None), ("Brand", None), ("Veigar", None), ("Annie", Some(Role::Utility))];
        let mages: Vec<_> = mages.iter().map(|(n, r)| (id(n), *r)).collect();
        assert_eq!(damage_profile(&mages, &data), DamageProfile::Magic);
        let ad: Vec<_> = ["Darius", "Jinx", "Zed", "Talon", "Caitlyn"].iter().map(|n| (id(n), None)).collect();
        assert_eq!(damage_profile(&ad, &data), DamageProfile::Physical);
        let mixed: Vec<_> = ["Darius", "Jinx", "Syndra", "Lux", "Leona"].iter().map(|n| (id(n), None)).collect();
        assert_eq!(damage_profile(&mixed, &data), DamageProfile::Mixed);
    }
}
