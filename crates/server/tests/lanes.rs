//! Port of server/lanes.test.ts: lane awareness outside the build engine. Database queries, champ select (your lane and
//! the likely lane opponent of each lane), and the live simulator with the tracker.

use irelia::db::{Db, IreliaGameRow, MatchRow};
use irelia::ddragon::ChampionMeta;
use irelia::engine::lane_opponent::{guess_lane_opponent, guess_lane_opponents, Faced};
use irelia::testkit::load_test_static_data;
use irelia_server::lcu::connector::parse_champ_select;
use irelia_server::lcu::live_client::PerformanceTracker;
use irelia_server::lcu::simulate_live::SimulatedGame;
use serde_json::{json, Value};
use std::collections::HashMap;

const DARIUS: i64 = 122;
const YASUO: i64 = 157;
const SYNDRA: i64 = 134;
const AHRI: i64 = 103;
const JINX: i64 = 222;
const LEESIN: i64 = 64;
const ZED: i64 = 238;
const AATROX: i64 = 266;
const ORIANNA: i64 = 61;
const LANES: [&str; 2] = ["TOP", "MIDDLE"];

fn seed(db: &Db, id: &str, patch: &str, position: &str, opponent: Option<i64>) {
    db.insert_match(&MatchRow { id: id.into(), platform: "KR".into(), patch: patch.into(), version: format!("{patch}.1"), queue: 420, creation: 0, duration: 1800, has_irelia: true }).unwrap();
    db.insert_game(&IreliaGameRow {
        match_id: id.into(),
        puuid: "p".into(),
        patch: patch.into(),
        win: 1,
        position: position.into(),
        opponent_champion_id: opponent,
        summoner1: 4,
        summoner2: 12,
        primary_style: 8000,
        sub_style: 8400,
        perks: "[]".into(),
        stat_perks: "[]".into(),
        purchases: "[]".into(),
        final_items: "[]".into(),
        game_duration: 1800,
        ..Default::default()
    })
    .unwrap();
}

#[test]
fn stored_games_split_by_lane_per_patch_and_opponents_can_be_counted_per_lane() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_default(&dir.path().join("counts.db")).unwrap();
    seed(&db, "KR_1", "16.19", "TOP", Some(DARIUS));
    seed(&db, "KR_2", "16.19", "TOP", Some(YASUO));
    seed(&db, "KR_3", "16.19", "MIDDLE", Some(YASUO));
    seed(&db, "KR_4", "16.19", "MIDDLE", Some(YASUO));
    seed(&db, "KR_5", "16.19", "UNKNOWN", None);
    seed(&db, "KR_6", "16.18", "MIDDLE", Some(SYNDRA));
    let src = vec!["otp".to_string()];
    let b: Vec<(String, i64, Vec<(String, i64)>)> = db
        .patch_breakdown(&src)
        .unwrap()
        .into_iter()
        .map(|(p, g, mut l)| {
            l.sort();
            (p, g, l)
        })
        .collect();
    assert_eq!(
        b,
        vec![
            ("16.19".to_string(), 5, vec![("MIDDLE".to_string(), 2), ("TOP".to_string(), 2), ("UNKNOWN".to_string(), 1)]),
            ("16.18".to_string(), 1, vec![("MIDDLE".to_string(), 1)]),
        ]
    );
    let by_id = |rows: Vec<(i64, i64)>| rows.into_iter().collect::<HashMap<_, _>>();
    let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(by_id(db.opponent_counts(&p(&["16.19"]), None, &src).unwrap()), HashMap::from([(DARIUS, 1), (YASUO, 3)]));
    assert_eq!(by_id(db.opponent_counts(&p(&["16.19"]), Some("TOP"), &src).unwrap()), HashMap::from([(DARIUS, 1), (YASUO, 1)]));
    assert_eq!(by_id(db.opponent_counts(&p(&["16.19", "16.18"]), Some("MIDDLE"), &src).unwrap()), HashMap::from([(SYNDRA, 1), (YASUO, 2)]));
}

// ---- Champ select: your lane and the likely lane opponent ----------------------------------------------

fn classes() -> HashMap<i64, ChampionMeta> {
    let data = load_test_static_data();
    let base = data.champion(DARIUS).unwrap().clone();
    [
        (DARIUS, vec!["Fighter", "Tank"]),
        (YASUO, vec!["Fighter", "Assassin"]),
        (SYNDRA, vec!["Mage"]),
        (AHRI, vec!["Mage", "Assassin"]),
        (JINX, vec!["Marksman"]),
        (LEESIN, vec!["Fighter", "Assassin"]),
        (ZED, vec!["Assassin"]),
    ]
    .into_iter()
    .map(|(id, tags)| (id, ChampionMeta { id, tags: tags.into_iter().map(String::from).collect(), ..base.clone() }))
    .collect()
}

fn faced(pairs: &[(i64, f64)]) -> Faced {
    pairs.iter().copied().collect()
}

#[test]
fn the_lane_opponent_guess_uses_who_irelia_faced_in_that_lane_then_the_classes_that_play_it() {
    let c = classes();
    let champs = |id: i64| c.get(&id);
    let picks = [JINX, LEESIN, SYNDRA, DARIUS, YASUO];
    let none = Faced::new();
    assert_eq!(guess_lane_opponent(&picks, "TOP", &none, &champs), Some(LEESIN), "first Fighter/Tank");
    assert_eq!(guess_lane_opponent(&picks, "MIDDLE", &none, &champs), Some(SYNDRA), "first Mage/Assassin");
    assert_eq!(guess_lane_opponent(&[JINX, DARIUS, YASUO], "MIDDLE", &none, &champs), Some(YASUO), "second class when no main class fits");
    assert_eq!(guess_lane_opponent(&[JINX], "MIDDLE", &none, &champs), Some(JINX), "nothing fits: first pick");
    assert_eq!(guess_lane_opponent(&[], "TOP", &none, &champs), None);
    // Per-lane database counts beat the class heuristic.
    assert_eq!(guess_lane_opponent(&picks, "MIDDLE", &faced(&[(YASUO, 12.0), (SYNDRA, 3.0)]), &champs), Some(YASUO));
    assert_eq!(guess_lane_opponent(&picks, "TOP", &faced(&[(DARIUS, 7.0)]), &champs), Some(DARIUS));
}

/// Yasuo is 30 of Irelia's 300 top games (10%) and 20 of her 100 mid games (20%).
fn faced_both() -> HashMap<&'static str, Faced> {
    HashMap::from([("TOP", faced(&[(YASUO, 30.0), (DARIUS, 20.0), (AATROX, 250.0)])), ("MIDDLE", faced(&[(YASUO, 20.0), (SYNDRA, 10.0), (ORIANNA, 70.0)]))])
}

fn lanes(v: Vec<(&'static str, Option<i64>)>) -> HashMap<&'static str, Option<i64>> {
    v.into_iter().collect()
}

#[test]
fn two_lanes_never_get_the_same_likely_opponent_the_lane_where_the_pick_is_likelier_keeps_it() {
    let c = classes();
    let champs = |id: i64| c.get(&id);
    let picks = [JINX, LEESIN, SYNDRA, DARIUS, YASUO];
    let f = faced_both();
    assert_eq!(guess_lane_opponent(&picks, "TOP", &f["TOP"], &champs), Some(YASUO), "on its own, top would take Yasuo too");
    assert_eq!(lanes(guess_lane_opponents(&picks, &LANES, &f, &champs)), HashMap::from([("TOP", Some(DARIUS)), ("MIDDLE", Some(YASUO))]));
    // A lane with a handful of games can't claim a pick on one match.
    let thin = HashMap::from([("TOP", f["TOP"].clone()), ("MIDDLE", faced(&[(YASUO, 1.0), (ORIANNA, 1.0)]))]);
    assert_eq!(lanes(guess_lane_opponents(&picks, &LANES, &thin, &champs)), HashMap::from([("TOP", Some(YASUO)), ("MIDDLE", Some(SYNDRA))]));
    // One pick so far: it goes to the lane its class plays, and the other lane waits for the next pick.
    let empty = HashMap::new();
    assert_eq!(lanes(guess_lane_opponents(&[SYNDRA], &LANES, &empty, &champs)), HashMap::from([("TOP", None), ("MIDDLE", Some(SYNDRA))]));
    assert_eq!(lanes(guess_lane_opponents(&[DARIUS], &LANES, &empty, &champs)), HashMap::from([("TOP", Some(DARIUS)), ("MIDDLE", None)]));
    assert_eq!(lanes(guess_lane_opponents(&[JINX], &LANES, &empty, &champs)), HashMap::from([("TOP", Some(JINX)), ("MIDDLE", None)]), "top keeps it");
    assert_eq!(lanes(guess_lane_opponents(&[], &LANES, &empty, &champs)), HashMap::from([("TOP", None), ("MIDDLE", None)]));
    // Only the lanes asked for are guessed.
    assert_eq!(lanes(guess_lane_opponents(&picks, &["TOP"], &f, &champs)), HashMap::from([("TOP", Some(YASUO))]));
}

fn session(my_position: &str, enemies: &[(i64, &str)]) -> Value {
    json!({
        "localPlayerCellId": 2,
        "myTeam": (0..5).map(|cell| json!({ "cellId": cell, "championId": if cell == 2 { 39 } else { 0 }, "championPickIntent": 0, "assignedPosition": if cell == 2 { my_position } else { "" } })).collect::<Vec<_>>(),
        "theirTeam": enemies.iter().enumerate().map(|(i, (id, pos))| json!({ "cellId": 5 + i, "championId": id, "championPickIntent": 0, "assignedPosition": pos })).collect::<Vec<_>>(),
        "actions": [[{ "actorCellId": 2, "championId": 39, "completed": true, "type": "pick" }]],
    })
}

#[test]
fn champ_select_reads_your_assigned_lane_and_suggests_an_opponent_for_each_lane() {
    let asked = std::sync::Mutex::new(Vec::<(Vec<i64>, Vec<&str>)>::new());
    let guess = |ids: &[i64], ls: &[&'static str]| {
        asked.lock().unwrap().push((ids.to_vec(), ls.to_vec()));
        vec![("TOP", Some(DARIUS)), ("MIDDLE", Some(SYNDRA))]
    };
    let ranked = parse_champ_select(&session("middle", &[(DARIUS, ""), (SYNDRA, ""), (JINX, "")]), 39, &guess);
    assert_eq!(ranked["myPosition"], json!("MIDDLE"));
    assert_eq!(ranked["isIrelia"], json!(true));
    assert_eq!(ranked["suggestedOpponents"], json!({ "TOP": DARIUS, "MIDDLE": SYNDRA }));
    assert_eq!(ranked["suggestedOpponentId"], json!(SYNDRA), "follows the assigned lane");
    assert_eq!(*asked.lock().unwrap(), vec![(vec![DARIUS, SYNDRA, JINX], vec!["TOP", "MIDDLE"])], "both lanes guessed together");
    // With the real guess, a pick both lanes would take is suggested for one lane only.
    let c = classes();
    let f = faced_both();
    let real = |ids: &[i64], ls: &[&'static str]| guess_lane_opponents(ids, ls, &f, &|id| c.get(&id));
    assert_eq!(parse_champ_select(&session("top", &[(YASUO, ""), (DARIUS, ""), (JINX, "")]), 39, &real)["suggestedOpponents"], json!({ "TOP": DARIUS, "MIDDLE": YASUO }));
}

#[test]
fn blind_pick_has_no_lane_nothing_is_assumed() {
    let guess = |_: &[i64], _: &[&'static str]| vec![("TOP", Some(DARIUS)), ("MIDDLE", Some(SYNDRA))];
    let blind = parse_champ_select(&session("", &[(DARIUS, ""), (SYNDRA, "")]), 39, &guess);
    assert_eq!(blind["myPosition"], json!(""), "no silent TOP default");
    assert_eq!(blind["suggestedOpponents"], json!({ "TOP": DARIUS, "MIDDLE": SYNDRA }));
    assert_eq!(blind["suggestedOpponentId"], json!(DARIUS), "legacy field keeps its top default");
}

#[test]
fn a_visible_enemy_role_beats_the_database_guess_for_that_lane() {
    let both = |_: &[i64], _: &[&'static str]| vec![("TOP", Some(YASUO)), ("MIDDLE", Some(YASUO))];
    let draft = parse_champ_select(&session("top", &[(ZED, "MIDDLE"), (DARIUS, "TOP"), (YASUO, "")]), 39, &both);
    assert_eq!(draft["suggestedOpponents"], json!({ "TOP": DARIUS, "MIDDLE": ZED }));
    let darius = |_: &[i64], _: &[&'static str]| vec![("TOP", Some(DARIUS)), ("MIDDLE", Some(DARIUS))];
    let no_picks = parse_champ_select(&session("top", &[(0, ""), (0, "")]), 39, &darius);
    assert_eq!(no_picks["suggestedOpponents"], json!({ "TOP": null, "MIDDLE": null }));
}

#[test]
fn an_enemy_shown_in_another_role_is_never_guessed_for_a_lane() {
    let asked = std::sync::Mutex::new(Vec::<(Vec<i64>, Vec<&str>)>::new());
    let guess = |ids: &[i64], ls: &[&'static str]| {
        asked.lock().unwrap().push((ids.to_vec(), ls.to_vec()));
        vec![("TOP", ids.first().copied())]
    };
    let draft = parse_champ_select(&session("top", &[(ZED, "MIDDLE"), (LEESIN, "jungle"), (DARIUS, ""), (YASUO, "")]), 39, &guess);
    assert_eq!(*asked.lock().unwrap(), vec![(vec![DARIUS, YASUO], vec!["TOP"])], "only hidden roles, only for the lane still open");
    assert_eq!(draft["suggestedOpponents"], json!({ "TOP": DARIUS, "MIDDLE": ZED }));
}

// ---- Live game: the simulator and the tracker pair you with your lane opponent ---------------------------

#[test]
fn the_live_simulator_plays_either_lane_and_the_tracker_pairs_you_with_that_lane_opponent() {
    for (lane, opponent) in [("TOP", "Darius"), ("MIDDLE", "Syndra")] {
        let first = SimulatedGame::new(600.0, lane, true).tick();
        let me = first["allPlayers"].as_array().unwrap().iter().find(|p| p["riotIdGameName"] == "You").unwrap();
        assert_eq!((me["championName"].clone(), me["position"].clone()), (json!("Irelia"), json!(lane)));
        let perf = PerformanceTracker::default().update(&first, &|_| 0.0, None).unwrap();
        assert_eq!(perf["opponent"]["championName"], json!(opponent));
        assert!(!perf["diff"].is_null(), "{lane}: CS / vision / gold differences against {opponent}");
    }
}
