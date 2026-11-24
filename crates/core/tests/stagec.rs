//! Port of server/model/stagec.test.ts: Stage C on synthetic games. Matchup facts (C2) and the best purchase for your
//! gold (C3), against the tests' Data Dragon snapshot (Trinity Force = Sheen + Phage + Hearthbound Axe).

use irelia::engine::adaptive::live_summary;
use irelia::model::buy::{missing_tree, BuyIndex, BuyQuery};
use irelia::model::dataset::{ItemChoice, PreparedGame, ScoredGame};
use irelia::model::facts::{matchup_best, matchup_facts, BEST_PRIOR};
use irelia::testkit::load_test_static_data;
use irelia::testkit_model::blank_game;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const TRINITY: i64 = 3078;
const SHEEN: i64 = 3057;
const PHAGE: i64 = 3044;
const AXE: i64 = 3051;
const LONG_SWORD: i64 = 1036;
const RUBY: i64 = 1028;
const DARIUS: i64 = 122;
const SETT: i64 = 875;

#[derive(Clone)]
struct G {
    lane: &'static str,
    opponent: Option<i64>,
    win: f64,
    source: &'static str,
    minutes: f64,
    lane10: Option<f64>,
    lane15: Option<f64>,
    path: Vec<i64>,
}

impl Default for G {
    fn default() -> G {
        G { lane: "TOP", opponent: Some(DARIUS), win: 1.0, source: "otp", minutes: 30.0, lane10: None, lane15: None, path: vec![] }
    }
}

static N: AtomicUsize = AtomicUsize::new(0);

fn item(id: i64, t: f64) -> ItemChoice {
    ItemChoice { item: id as f64, t, x: None, live: [0.0; 8] }
}

fn game(o: G) -> PreparedGame {
    let n = N.fetch_add(1, Ordering::SeqCst) + 1;
    // Parts a minute apart, then Trinity Force.
    let done = (o.path.len() + 4) as f64 * 60_000.0;
    let mut purchases: Vec<(i64, f64)> = o.path.iter().enumerate().map(|(i, &id)| (id, (i + 3) as f64 * 60_000.0)).collect();
    purchases.push((TRINITY, done));
    PreparedGame {
        match_id: format!("KR_{n}"),
        pilot: format!("p{}", n % 40),
        source: o.source.into(),
        lane: o.lane,
        win: o.win,
        opponent: o.opponent,
        items: vec![item(TRINITY, done)],
        purchases,
        duration: o.minutes * 60.0,
        lane10: o.lane10,
        lane15: o.lane15,
        ..blank_game()
    }
}

fn scored(gs: Vec<PreparedGame>) -> Vec<ScoredGame> {
    gs.into_iter().map(|g| ScoredGame { g: Arc::new(g), wpa0: 0.0, item_wpa: vec![0.0], boots_wpa: None, swap_wpa: vec![], item_w0: vec![0.5], boots_w0: None }).collect()
}

fn times(n: usize, o: G) -> Vec<PreparedGame> {
    (0..n).map(|_| game(o.clone())).collect()
}

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap()
}

#[test]
fn c2_matchup_facts_the_lane_gold_at_10_and_15_and_the_win_rate_by_length_from_this_lane_against_this_opponent_only() {
    let games: Vec<Arc<PreparedGame>> = vec![
        game(G { lane10: Some(-300.0), lane15: Some(-500.0), minutes: 22.0, win: 0.0, ..G::default() }),
        game(G { lane10: Some(-100.0), lane15: Some(200.0), minutes: 27.0, win: 1.0, ..G::default() }),
        game(G { lane10: Some(200.0), lane15: Some(900.0), minutes: 33.0, win: 1.0, ..G::default() }),
        game(G { lane10: None, minutes: 40.0, win: 1.0, ..G::default() }),
        game(G { opponent: Some(SETT), lane10: Some(5000.0), ..G::default() }),
        game(G { lane: "MIDDLE", lane10: Some(5000.0), ..G::default() }),
        game(G { source: "me", lane10: Some(5000.0), ..G::default() }),
    ]
    .into_iter()
    .map(Arc::new)
    .collect();
    let facts = matchup_facts(&games, "TOP", Some(DARIUS), 3).unwrap();
    assert_eq!(facts["games"], 4);
    assert_eq!(f(&facts["winRate"]), 0.75);
    assert_eq!((f(&facts["gold10"]["median"]), f(&facts["gold10"]["ahead"]), f(&facts["gold10"]["games"])), (-100.0, 1.0 / 3.0, 3.0));
    assert_eq!((f(&facts["gold15"]["median"]), f(&facts["gold15"]["ahead"]), f(&facts["gold15"]["games"])), (200.0, 2.0 / 3.0, 3.0));
    let by: Vec<(f64, f64, f64)> = facts["byLength"].as_array().unwrap().iter().map(|b| (f(&b["from"]), f(&b["games"]), f(&b["winRate"]))).collect();
    assert_eq!(by, vec![(0.0, 1.0, 0.0), (25.0, 1.0, 1.0), (30.0, 1.0, 1.0), (35.0, 1.0, 1.0)]);
    assert!(matchup_facts(&games, "TOP", None, 3).is_none(), "no opponent, no facts");
    assert!(matchup_facts(&games, "TOP", Some(SETT), 3).is_none(), "too few games");
}

#[test]
fn the_item_sets_vs_blocks_the_start_and_first_items_that_did_best_in_this_matchup_shrunk_towards_0() {
    const BOTRK: i64 = 3153;
    const STERAK: i64 = 3053;
    const REMOVED: i64 = 999_999;
    let g = |start: &str, first: i64, wpa: f64, opponent: i64, source: &'static str| {
        let mut p = game(G { opponent: Some(opponent), source, ..G::default() });
        p.start = start.into();
        p.items = vec![item(first, 0.0)];
        ScoredGame { g: Arc::new(p), wpa0: wpa, item_wpa: vec![wpa], boots_wpa: None, swap_wpa: vec![], item_w0: vec![0.5], boots_w0: None }
    };
    let mut games: Vec<ScoredGame> = vec![];
    // Doran's Blade + potion: 10 games, all a little better; long sword: 6 games, much better; a lucky item in 2 games.
    games.extend((0..10).map(|_| g("1055,2003", BOTRK, 0.1, DARIUS, "otp")));
    games.extend((0..6).map(|_| g("1036,2003", STERAK, 0.3, DARIUS, "otp")));
    games.extend((0..2).map(|_| g("", TRINITY, 0.9, DARIUS, "otp")));
    games.extend((0..8).map(|_| g("", REMOVED, 0.9, DARIUS, "otp")));
    // Not this matchup, or your own games: never counted.
    games.extend((0..30).map(|_| g("2051", AXE, 0.9, SETT, "otp")));
    games.extend((0..30).map(|_| g("2051", AXE, 0.9, DARIUS, "me")));
    let buyable = |id: i64| id != REMOVED;
    let best = matchup_best(&games, "TOP", Some(DARIUS), &buyable).unwrap();
    // Shrunk: 6 × 0.3 / (6 + 20) beats 10 × 0.1 / (10 + 20); the 2-game item has too few games; the removed one can't be bought.
    assert_eq!((best["start"]["value"].clone(), best["start"]["games"].clone()), (json!([1036, 2003]), json!(6)));
    assert!((f(&best["start"]["lift"]) - (6.0 * 0.3) / (6.0 + BEST_PRIOR)).abs() < 1e-12);
    let first: Vec<(Value, Value)> = best["first"].as_array().unwrap().iter().map(|o| (o["value"].clone(), o["games"].clone())).collect();
    assert_eq!(first, vec![(json!(STERAK), json!(6)), (json!(BOTRK), json!(10))]);
    assert!(matchup_best(&games, "TOP", None, &buyable).is_none(), "no opponent");
    assert!(matchup_best(&games[..10], "TOP", Some(DARIUS), &buyable).is_none(), "too few games against it");
}

#[test]
fn c3_the_missing_parts_of_an_item_and_their_price_with_what_you_own() {
    let data = load_test_static_data();
    assert_eq!(missing_tree(&data, TRINITY, &[]).unwrap()[0].cost, 3333.0);
    let with_sheen = missing_tree(&data, TRINITY, &[SHEEN, LONG_SWORD]).unwrap();
    assert_eq!(with_sheen[0].cost, 3333.0 - 900.0 - 350.0, "a Long Sword counts toward Phage or the Axe");
    assert!(missing_tree(&data, TRINITY, &[TRINITY]).is_none());
}

fn q(owned: &[i64], gold: f64, opponent: Option<i64>) -> BuyQuery<'_> {
    BuyQuery { lane: "TOP", opponent, target: TRINITY, owned, gold }
}

fn items(v: &Value) -> Vec<i64> {
    v["items"].as_array().unwrap().iter().map(|x| x.as_i64().unwrap()).collect()
}

#[test]
fn c3_best_buy_the_part_high_elo_irelias_buy_first_the_rest_of_the_gold_to_the_next_part_the_item_itself_once_affordable() {
    let data = load_test_static_data();
    let mut games = times(30, G { path: vec![SHEEN, LONG_SWORD, RUBY, PHAGE, AXE], ..G::default() });
    games.extend(times(10, G { path: vec![PHAGE, SHEEN, AXE], ..G::default() }));
    let buys = BuyIndex::new(Arc::new(scored(games.clone())), None, data.clone());
    let a = buys.best(&q(&[], 1000.0, Some(SETT))).unwrap();
    assert_eq!(items(&a), vec![SHEEN]);
    assert_eq!(a["finishes"], false);
    assert_eq!(f(&a["share"]), 30.0 / 40.0);
    assert!(a["reason"].as_str().unwrap().contains("75% of 40 high-elo games buy Sheen first toward Trinity Force"), "{}", a["reason"]);
    assert_eq!(items(&buys.best(&q(&[], 1300.0, Some(SETT))).unwrap()), vec![SHEEN, LONG_SWORD], "400g left: the next part they buy");
    let done = buys.best(&q(&[SHEEN], 2500.0, Some(SETT))).unwrap();
    assert_eq!((items(&done), done["finishes"].clone(), f(&done["cost"])), (vec![TRINITY], json!(true), 2433.0));
    assert!(buys.best(&q(&[], 100.0, Some(SETT))).is_none(), "nothing affordable");
    // Games against your lane opponent count three times: here they start with Phage.
    let mut more = games;
    more.extend(times(12, G { opponent: Some(SETT), path: vec![PHAGE, SHEEN], ..G::default() }));
    let vs_opponent = BuyIndex::new(Arc::new(scored(more)), None, data);
    let vs = vs_opponent.best(&q(&[], 1200.0, Some(SETT))).unwrap();
    assert_eq!(items(&vs), vec![PHAGE]);
    assert!(
        vs["reason"].as_str().unwrap().contains("42% of 52 high-elo games buy Phage first toward Trinity Force (12 of 12 against your opponent)"),
        "the text counts games, the ranking weighs them: {}",
        vs["reason"]
    );
}

#[test]
fn c3_a_part_bought_first_less_often_wins_when_its_games_clearly_did_better_than_the_game_state_predicted() {
    let data = load_test_static_data();
    let mut games = times(40, G { path: vec![SHEEN], win: 0.0, ..G::default() });
    games.extend(times(35, G { path: vec![PHAGE], win: 1.0, ..G::default() }));
    let b = BuyIndex::new(Arc::new(scored(games)), None, data).best(&q(&[], 1150.0, None)).unwrap();
    assert_eq!(items(&b), vec![PHAGE]);
    assert!(b["reason"].as_str().unwrap().contains("less often than Sheen, but those games went better"), "{}", b["reason"]);
    assert!(f(&b["lift"]) > 0.0);
}

#[test]
fn who_is_ahead_from_the_scoreboard_team_items_and_kills_and_a_most_fed_player_only_when_clearly_above_the_team() {
    let data = load_test_static_data();
    let p = |champion_id: i64, team: &str, items: &[i64], kills: i64, is_me: bool| {
        json!({ "championName": "", "championId": champion_id, "team": team, "position": "", "level": 10, "items": items, "kills": kills, "deaths": 0, "assists": 0, "creepScore": 0, "isMe": is_me })
    };
    let me = p(39, "ORDER", &[TRINITY], 3, true);
    let players = vec![
        me.clone(),
        p(222, "ORDER", &[SHEEN], 0, false),
        p(64, "ORDER", &[LONG_SWORD], 0, false),
        p(134, "CHAOS", &[TRINITY, PHAGE], 4, false),
        p(DARIUS, "CHAOS", &[SHEEN], 1, false),
        p(SETT, "CHAOS", &[], 0, false),
    ];
    let s = live_summary(&players, &me, &data, None);
    assert_eq!(f(&s["teamItems"]), (3333.0 + 900.0 + 350.0) - (3333.0 + 1100.0 + 900.0));
    assert_eq!(f(&s["kills"]), -2.0);
    assert_eq!(s["fedEnemy"], json!({ "championId": 134, "items": 4433, "ap": true }));
    assert_eq!(s["fedAlly"], json!({ "championId": 39, "items": 3333, "isMe": true }));
    let even = live_summary(&[me.clone(), p(134, "CHAOS", &[TRINITY], 0, false)], &me, &data, None);
    assert!(even["fedEnemy"].is_null(), "one enemy: nobody stands out");
}
