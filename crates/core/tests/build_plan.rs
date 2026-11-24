//! Port of server/engine/buildPlan.test.ts for the Rust copy of shared/buildPlan.ts (engine/build_plan.rs, which the
//! server uses for "Next" in lane-baseline mode). Checked against the current patch's Data Dragon items and fed the way
//! the overlay is: a Live Client payload through analyse_live_game. The web's TS copy keeps its own tests.

use irelia::db::IreliaGameRow;
use irelia::engine::adaptive::analyse_live_game;
use irelia::engine::build_plan::{build_plan, plan_status, PlanEntry, PlanStatus};
use irelia::engine::exclusivity::{build_exclusivity_index, ExclusivityIndex};
use irelia::engine::item_catalog::{item_catalog, ItemCatalog};
use irelia::engine::recommend::{recommend, EngineSettings};
use irelia::ddragon::StaticData;
use irelia::testkit::load_test_static_data;
use irelia::types::{BuildRecommendation, Opt};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};

const BOTRK: i64 = 3153;
const TRINITY: i64 = 3078;
const STERAKS: i64 = 3053;
const MAW: i64 = 3156;
const DEATHS_DANCE: i64 = 6333;
const BLACK_CLEAVER: i64 = 3071;
const WITS_END: i64 = 3091;
const STEELCAPS: i64 = 3047;
const MERCURYS: i64 = 3111;
const BERSERKERS: i64 = 3006;
const ARMORED_ADVANCE: i64 = 3174;
const GUNMETAL: i64 = 3172;
const BOOTS: i64 = 1001;
const SHEEN: i64 = 3057;
const LONG_SWORD: i64 = 1036;
const TUNNELER: i64 = 2021;
const TITANIC: i64 = 3748;
const MANAMUNE: i64 = 3004;
const MURAMANA: i64 = 3042;
const STEALTH_WARD: i64 = 3340;
const SPIRIT_VISAGE: i64 = 3065;

struct Env {
    data: Arc<StaticData>,
    excl: ExclusivityIndex,
    items: ItemCatalog,
    engine_rec: BuildRecommendation,
}

fn settings() -> EngineSettings {
    EngineSettings {
        min_matchup_games: 15.0,
        general_prior_weight: 0.15,
        min_lane_games: 30.0,
        other_lane_weight: 0.15,
        previous_patch_weight: 0.35,
        min_current_patch_games: 40.0,
        starting_item_window_ms: 60_000.0,
    }
}

fn row(data: &StaticData, i: usize, its: &[i64]) -> Arc<IreliaGameRow> {
    let mut purchases = vec![json!([1055, 1000]), json!([STEELCAPS, 400_000])];
    purchases.extend(its.iter().enumerate().map(|(k, id)| json!([id, 600_000 + k * 300_000])));
    Arc::new(IreliaGameRow {
        match_id: format!("T_{i}"),
        puuid: "p".into(),
        patch: data.patch.clone(),
        win: if i % 3 != 0 { 1 } else { 0 },
        position: "TOP".into(),
        opponent_champion_id: Some(122),
        summoner1: 4,
        summoner2: 12,
        primary_style: 8000,
        sub_style: 8400,
        perks: "[8010,9111,9104,8299,8444,8242]".into(),
        stat_perks: "[5005,5008,5011]".into(),
        skill_order: "QEWQQRQEQ".into(),
        purchases: Value::Array(purchases).to_string(),
        final_items: "[]".into(),
        kills: 1,
        deaths: 1,
        assists: 1,
        game_duration: 1800,
        created_at: 0,
        ..Default::default()
    })
}

fn env() -> &'static Env {
    static E: OnceLock<Env> = OnceLock::new();
    E.get_or_init(|| {
        let data = load_test_static_data();
        let excl = build_exclusivity_index(&data);
        let items = item_catalog(&data, &excl);
        let rows: Vec<Arc<IreliaGameRow>> = (0..30).map(|i| row(&data, i, &[BOTRK, TRINITY, STERAKS, DEATHS_DANCE])).collect();
        let engine_rec = recommend(&rows, &data, &excl, Some(122), &settings(), Some("TOP"), &|_| None, None);
        Env { data, excl, items, engine_rec }
    })
}

fn opt<T>(value: T) -> Opt<T> {
    Opt { value, games: 10.0, raw_games: 10.0, win_rate: 0.6, score: 0.5 }
}

/// Just the parts of a recommendation the plan reads (the rest from a real one).
fn rec(core: &[&[i64]], boots: &[i64], slot4: &[i64], slot5: &[i64], slot6: &[i64]) -> BuildRecommendation {
    let mut r = env().engine_rec.clone();
    r.core_build = core.iter().map(|c| opt(c.to_vec())).collect();
    r.boots = boots.iter().map(|b| opt(*b)).collect();
    r.situational.slot4 = slot4.iter().map(|x| opt(*x)).collect();
    r.situational.slot5 = slot5.iter().map(|x| opt(*x)).collect();
    r.situational.slot6 = slot6.iter().map(|x| opt(*x)).collect();
    r.boots_after = None;
    r
}

fn irelia_top() -> BuildRecommendation {
    rec(&[&[BOTRK, TRINITY, STERAKS], &[BOTRK, TRINITY, DEATHS_DANCE]], &[STEELCAPS, MERCURYS], &[DEATHS_DANCE, BLACK_CLEAVER, MAW], &[BLACK_CLEAVER, DEATHS_DANCE], &[WITS_END])
}

/// allgamedata with you (Irelia) holding `inventory`, in that slot order, and a lane opponent.
fn payload_as(inventory: &[i64], active: Value) -> Value {
    let p = |name: &str, champion: &str, team: &str, it: &[i64]| {
        json!({
            "championName": champion, "riotId": format!("{name}#KR1"), "riotIdGameName": name, "summonerName": format!("{name}#KR1"), "team": team,
            "position": "TOP", "level": 11, "items": it.iter().enumerate().map(|(slot, id)| json!({ "itemID": id, "count": 1, "slot": slot })).collect::<Vec<_>>(),
            "scores": { "kills": 2, "deaths": 1, "assists": 1, "creepScore": 150, "wardScore": 10 },
        })
    };
    json!({ "activePlayer": active, "allPlayers": [p("Me", "Irelia", "ORDER", inventory), p("Opp", "Darius", "CHAOS", &[BLACK_CLEAVER])], "gameData": { "gameTime": 1200 } })
}

fn payload(inventory: &[i64]) -> Value {
    payload_as(inventory, json!({ "riotId": "Me#KR1", "riotIdGameName": "Me", "currentGold": 900 }))
}

struct Status {
    entries: Vec<PlanStatus>,
    next: Option<usize>,
}

impl Status {
    fn next(&self) -> Option<&PlanStatus> {
        self.next.map(|n| &self.entries[n])
    }
    fn next_id(&self) -> Option<i64> {
        self.next().map(|e| e.entry.id)
    }
    fn next_cost(&self) -> Option<f64> {
        self.next().map(|e| e.cost)
    }
    fn states(&self) -> Vec<String> {
        self.entries.iter().map(|e| format!("{}:{}", e.entry.slot, e.state)).collect()
    }
    fn entry(&self, slot: &str) -> &PlanStatus {
        self.entries.iter().find(|e| e.entry.slot == slot).unwrap()
    }
}

fn status(plan: &[PlanEntry], owned: &[i64]) -> Status {
    let (entries, next) = plan_status(plan, owned, &env().items);
    Status { entries, next }
}

/// What the overlay does: owned ids from the Live Client, matched against the plan.
fn overlay(r: &BuildRecommendation, raw: &Value) -> Status {
    let e = env();
    let live = analyse_live_game(raw, &e.data, &e.excl, None, None);
    let owned: Vec<i64> = live["players"].as_array().unwrap().iter().find(|p| p["isMe"] == true).map(|me| me["items"].as_array().unwrap().iter().filter_map(|i| i.as_i64()).collect()).unwrap_or_default();
    status(&build_plan(r, &e.items), &owned)
}

fn states(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn single(id: i64) -> Vec<PlanEntry> {
    vec![PlanEntry { id, slot: "Core", alternatives: vec![] }]
}

// ---- Data Dragon facts the rules rely on

#[test]
fn catalog_tier_3_boots_upgrade_tier_2_ones_gunmetal_greaves_is_boots_without_the_boots_tag() {
    let e = env();
    let items = &e.items;
    assert_eq!(items[&ARMORED_ADVANCE].from, vec![STEELCAPS]);
    assert_eq!((items[&ARMORED_ADVANCE].boots, items[&ARMORED_ADVANCE].finished), (true, true));
    assert!(!e.data.items[&GUNMETAL].tags.iter().any(|t| t == "Boots"), "Data Dragon still lacks the tag");
    assert_eq!((items[&GUNMETAL].from.clone(), items[&GUNMETAL].boots), (vec![BERSERKERS], true));
    assert_eq!((items[&BOOTS].boots, items[&BOOTS].finished), (false, false), "tier-1 boots are a component");
    assert_eq!((items[&SHEEN].finished, items[&TRINITY].finished), (false, true));
    assert_eq!((items[&MURAMANA].from.clone(), items[&MURAMANA].finished), (vec![MANAMUNE], true), "transform via specialRecipe");
    assert_eq!(e.data.items[&ARMORED_ADVANCE].gold_total, e.data.items[&STEELCAPS].gold_total, "the upgrade itself is free");
}

// ---- The cases from the bug report

#[test]
fn a_recommended_boots_bought_third_ticked_next_is_the_first_entry_still_missing() {
    let s = overlay(&irelia_top(), &payload(&[BOTRK, TRINITY, STEELCAPS, STEALTH_WARD]));
    assert_eq!(s.states(), states(&["Core:done", "Core:done", "Core:todo", "Boots:done", "4th:todo", "5th:todo", "6th:todo"]));
    assert_eq!(s.next_id(), Some(STERAKS));
    assert_eq!(s.next_cost(), Some(3200.0));
    // Inventory order never matters.
    assert_eq!(overlay(&irelia_top(), &payload(&[STEELCAPS, STEALTH_WARD, TRINITY, BOTRK])).states(), s.states());
}

#[test]
fn b_a_different_pair_of_boots_fills_the_boots_slot_as_a_swap() {
    let s = overlay(&irelia_top(), &payload(&[BOTRK, TRINITY, MERCURYS]));
    let boots = s.entry("Boots");
    assert_eq!((boots.state, boots.entry.id, boots.owned), ("swap", STEELCAPS, Some(MERCURYS)));
    let s = overlay(&irelia_top(), &payload(&[BOTRK, TRINITY, BERSERKERS]));
    let not_listed = s.entry("Boots");
    assert_eq!((not_listed.state, not_listed.owned), ("swap", Some(BERSERKERS)), "any boots, listed or not");
}

#[test]
fn c_upgraded_tier_3_boots_count_as_the_planned_pair_including_gunmetal_greaves() {
    let s = overlay(&irelia_top(), &payload(&[BOTRK, TRINITY, ARMORED_ADVANCE]));
    assert_eq!((s.entry("Boots").state, s.entry("Boots").owned), ("upgraded", Some(ARMORED_ADVANCE)));
    assert_eq!(s.next_id(), Some(STERAKS));
    let zerker = rec(&[&[BOTRK, TRINITY, STERAKS]], &[BERSERKERS], &[], &[], &[]);
    assert_eq!(overlay(&zerker, &payload(&[GUNMETAL])).entry("Boots").state, "upgraded");
    assert_eq!(overlay(&irelia_top(), &payload(&[GUNMETAL])).entry("Boots").state, "swap", "boots, though untagged");
}

#[test]
fn d_components_never_tick_an_item_it_is_being_built_and_next_costs_what_is_left_to_pay() {
    let s = overlay(&irelia_top(), &payload(&[BOTRK, SHEEN, STEELCAPS]));
    assert_eq!(s.states()[..4], states(&["Core:done", "Core:building", "Core:todo", "Boots:done"])[..]);
    assert_eq!(s.entries[1].components, vec![SHEEN]);
    assert_eq!(s.next_id(), Some(TRINITY));
    assert_eq!(s.next_cost(), Some(3333.0 - 900.0));
    let t1 = overlay(&irelia_top(), &payload(&[BOTRK, TRINITY, STERAKS, BOOTS]));
    assert_eq!((t1.entry("Boots").state, t1.next_id(), t1.next_cost()), ("building", Some(STEELCAPS), Some(1200.0 - 300.0)));
}

#[test]
fn d_an_item_of_the_same_unique_group_replaces_the_planned_one_maw_for_steraks() {
    let s = overlay(&irelia_top(), &payload(&[BOTRK, TRINITY, MAW, STEELCAPS]));
    assert_eq!((s.entries[2].state, s.entries[2].entry.id, s.entries[2].owned), ("swap", STERAKS, Some(MAW)));
    assert_eq!(s.next().unwrap().entry.slot, "4th", "never \"Next: Sterak's\", which the shop would refuse");
}

#[test]
fn d_an_alternative_the_recommendation_lists_for_that_slot_fills_it_as_a_swap() {
    let r = rec(&[&[BOTRK, TRINITY, STERAKS], &[BOTRK, TRINITY, BLACK_CLEAVER]], &[STEELCAPS], &[DEATHS_DANCE, SPIRIT_VISAGE], &[WITS_END], &[]);
    let s = overlay(&r, &payload(&[BOTRK, TRINITY, BLACK_CLEAVER, STEELCAPS, SPIRIT_VISAGE]));
    assert_eq!((s.entries[2].state, s.entries[2].owned), ("swap", Some(BLACK_CLEAVER)), "core alternative: Black Cleaver 3rd");
    assert_eq!((s.entry("4th").state, s.entry("4th").owned), ("swap", Some(SPIRIT_VISAGE)));
    assert_eq!(s.next().unwrap().entry.slot, "5th");
    // An item planned for a later slot is that slot's, even when it is also an earlier slot's alternative.
    let early = overlay(&irelia_top(), &payload(&[BOTRK, TRINITY, DEATHS_DANCE, STEELCAPS]));
    assert_eq!(early.states()[2..5], states(&["Core:todo", "Boots:done", "4th:done"])[..]);
    assert_eq!(early.next_id(), Some(STERAKS));
}

#[test]
fn e_duplicates_the_plan_never_repeats_an_item_and_one_owned_item_fills_one_entry() {
    // The 5th slot's top option is already the 4th: take its next option that fits.
    let plan = build_plan(&rec(&[&[BOTRK, TRINITY, STERAKS]], &[STEELCAPS], &[DEATHS_DANCE], &[DEATHS_DANCE, MAW, BLACK_CLEAVER], &[]), &env().items);
    assert_eq!(plan.iter().map(|p| p.id).collect::<Vec<_>>(), vec![BOTRK, TRINITY, STERAKS, STEELCAPS, DEATHS_DANCE, BLACK_CLEAVER], "Maw is blocked by Sterak's");

    let s = overlay(&irelia_top(), &payload(&[BOTRK, TRINITY, STEELCAPS, STEELCAPS, LONG_SWORD, LONG_SWORD]));
    assert_eq!(s.states()[..4], states(&["Core:done", "Core:done", "Core:building", "Boots:done"])[..]);
    assert_eq!(s.entries[2].components, vec![LONG_SWORD], "Sterak's needs one Long Sword (in Tunneler)");
    assert_eq!(s.next_cost(), Some(3200.0 - 350.0));
    assert_eq!(status(&single(STERAKS), &[TUNNELER, LONG_SWORD]).next_cost(), Some(3200.0 - 1150.0), "an owned Tunneler covers its Long Sword; the spare one is not counted twice");
    // Two Long Swords both count towards Titanic Hydra (via Tiamat), each only once.
    assert_eq!(status(&single(TITANIC), &[LONG_SWORD, LONG_SWORD]).next_cost(), Some(3300.0 - 2.0 * 350.0));
    assert_eq!(status(&single(TITANIC), &[LONG_SWORD]).next_cost(), Some(3300.0 - 350.0));
}

#[test]
fn f_you_are_found_even_when_the_active_player_has_no_riot_id_like_the_live_tracker_does() {
    let raw = payload_as(&[BOTRK, TRINITY, STEELCAPS], json!({ "riotIdGameName": "Me", "summonerName": "Me", "currentGold": 900 }));
    assert_eq!(overlay(&irelia_top(), &raw).states()[..4], states(&["Core:done", "Core:done", "Core:todo", "Boots:done"])[..]);
    let e = env();
    assert_eq!(analyse_live_game(&raw, &e.data, &e.excl, None, None)["laneOpponent"]["championName"], "Darius");
}

#[test]
fn transformed_items_count_as_their_base_item_and_a_full_build_leaves_nothing_next() {
    let s = status(&single(MANAMUNE), &[MURAMANA]);
    assert_eq!(s.entries[0].state, "upgraded");
    assert!(s.next.is_none());
    let full = overlay(&irelia_top(), &payload(&[BOTRK, TRINITY, STERAKS, ARMORED_ADVANCE, DEATHS_DANCE, BLACK_CLEAVER, WITS_END]));
    assert!(full.next.is_none());
}

#[test]
fn end_to_end_a_plan_from_the_engines_own_recommendation() {
    let r = &env().engine_rec;
    let s = overlay(r, &payload(&[BOTRK, TRINITY, ARMORED_ADVANCE]));
    // These games buy their boots at 6:40, before any item: the plan starts with them.
    assert_eq!(r.boots_after, Some(Some(0.0)));
    assert_eq!(s.states()[..4], states(&["Boots:upgraded", "Core:done", "Core:done", "Core:todo"])[..]);
    assert_eq!(s.next_id(), Some(STERAKS));
}

#[test]
fn the_boots_take_their_place_in_the_build_order_after_the_first_item_when_the_games_buy_them_there() {
    let early = BuildRecommendation { boots_after: Some(Some(1.0)), ..irelia_top() };
    let plan = overlay(&early, &payload(&[BOTRK, STEALTH_WARD]));
    assert_eq!(plan.states(), states(&["Core:done", "Boots:todo", "Core:todo", "Core:todo", "4th:todo", "5th:todo", "6th:todo"]));
    assert_eq!(plan.next_id(), Some(STEELCAPS), "boots are next after the first item");
    let unknown = BuildRecommendation { boots_after: Some(None), ..irelia_top() };
    assert_eq!(overlay(&unknown, &payload(&[])).states()[3], "Boots:todo", "unknown: after the core");
}
