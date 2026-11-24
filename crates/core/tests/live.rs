//! Port of server/model/live.test.ts: Stage B in game and after it. Live re-ranking (B5: the next item from Live Client
//! data, its stability rule, items in progress, the time budget at ten times today's data) and the personal layer (B4:
//! timings, options shrunk toward high-elo, rec_log linked to your games).

use irelia::db::{Db, MatchRow};
use irelia::ddragon::StaticData;
use irelia::engine::exclusivity::{build_exclusivity_index, ExclusivityIndex};
use irelia::model::dataset::{prepare_games, score_games, train_state, PrepareOptions, PreparedGame, STATE_MAX_ROWS};
use irelia::model::draft::{build_draft_model, draft_defaults, DraftModel};
use irelia::model::live::{LiveHint, LiveNext, LiveReranker};
use irelia::model::personal::{link_recommendations, personal_report, PERSONAL_K};
use irelia::model::profiles::{ChampionProfiles, PROFILE_DEFAULTS};
use irelia::model::weights::RegionPatchFn;
use irelia::testkit::load_test_static_data;
use irelia::testkit_model::{ap_ad_games, model_inputs, ApAd, AD_TEAM, ALLIES, AP_TEAM};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};

struct Built {
    data: Arc<StaticData>,
    excl: ExclusivityIndex,
    m: DraftModel,
    profiles: Arc<ChampionProfiles>,
}

fn prepare(games: &[irelia::testkit_model::SynthGame], me: Option<&str>) -> (Vec<Arc<PreparedGame>>, Arc<ChampionProfiles>) {
    let data = load_test_static_data();
    let (rows, participants, totals) = model_inputs(games);
    let rows: Vec<Arc<_>> = rows
        .into_iter()
        .map(|mut r| {
            if Some(r.puuid.as_str()) == me {
                r.pilot_source = Some("me".into());
            }
            Arc::new(r)
        })
        .collect();
    let profiles = Arc::new(ChampionProfiles::new(&totals, &[], data.clone(), PROFILE_DEFAULTS));
    let prepared = prepare_games(&rows, &participants, &data, &profiles, &PrepareOptions { starting_item_window_ms: 60_000.0 }).into_iter().map(Arc::new).collect();
    (prepared, profiles)
}

fn built() -> &'static Built {
    static B: OnceLock<Built> = OnceLock::new();
    B.get_or_init(|| {
        let data = load_test_static_data();
        let excl = build_exclusivity_index(&data);
        let (prepared, profiles) = prepare(&ap_ad_games(800, 40, 7, ApAd::default()), None);
        let state = train_state(prepared.iter().map(|g| g.as_ref()), 1.0, STATE_MAX_ROWS).unwrap();
        let m = build_draft_model(score_games(&prepared, state.as_ref(), None), state, profiles.clone());
        Built { data, excl, m, profiles }
    })
}

/// A live snapshot: you (Irelia, top) with `items`, your allies, and `enemies` (the first is your lane opponent).
fn live(enemies: &[i64], items: &[i64], minute: f64) -> Value {
    let data = &built().data;
    let seat = |id: i64, i: usize, team: &str| {
        let own: Vec<i64> = if team == "ORDER" && i == 0 { items.to_vec() } else { vec![1055] };
        let position = ["TOP", "JUNGLE", "MIDDLE", "BOTTOM", "UTILITY"][i];
        json!({
            "championName": data.champions.get(&id).unwrap().key, "riotId": format!("{team}{i}#T"), "riotIdGameName": format!("{team}{i}"), "team": team,
            "position": position, "level": 8,
            "items": own.iter().map(|&id| json!({ "itemID": id })).collect::<Vec<_>>(),
            "scores": { "kills": 0, "deaths": 0, "assists": 0, "creepScore": 60 },
        })
    };
    let players: Vec<Value> = ALLIES.iter().enumerate().map(|(i, &id)| seat(id, i, "ORDER")).chain(enemies.iter().enumerate().map(|(i, &id)| seat(id, i, "CHAOS"))).collect();
    json!({ "activePlayer": { "riotId": players[0]["riotId"], "currentGold": 500 }, "allPlayers": players, "gameData": { "gameTime": minute * 60.0 } })
}

fn snap(enemies: &[i64]) -> Value {
    live(enemies, &[1055], 9.0)
}

fn rerank_with(r: &mut LiveReranker, raw: &Value, m: &DraftModel) -> Option<LiveNext> {
    let b = built();
    let none: RegionPatchFn = Arc::new(|_| None);
    r.rerank(m, "v1", raw, &b.data, &b.excl, &none, &draft_defaults(), &LiveHint::default())
}

fn rerank(r: &mut LiveReranker, raw: &Value) -> Option<LiveNext> {
    rerank_with(r, raw, &built().m)
}

fn item(n: &Option<LiveNext>) -> i64 {
    n.as_ref().map(|x| x.item_id).unwrap_or(0)
}

#[test]
fn live_the_next_item_follows_the_enemy_damage_type_labelled_with_what_drove_it() {
    let vs_ap = rerank(&mut LiveReranker::default(), &snap(&AP_TEAM));
    let vs_ad = rerank(&mut LiveReranker::default(), &snap(&AD_TEAM));
    assert_eq!(item(&vs_ap), 3053);
    assert_eq!(item(&vs_ad), 3153);
    let vs_ap = vs_ap.unwrap();
    assert_eq!(vs_ap.slot, 1);
    assert!(vs_ap.stats.lift > 0.0);
    assert!(vs_ap.reason.clone().unwrap_or_default().contains("magic damage"), "{:?}", vs_ap.reason);
    // When one enemy's items are worth far more than the others', the reason names them.
    let mut fed_syndra = snap(&AP_TEAM);
    fed_syndra["allPlayers"][5]["items"] = json!([{ "itemID": 6655 }, { "itemID": 3020 }, { "itemID": 1056 }]);
    assert_eq!(rerank(&mut LiveReranker::default(), &fed_syndra).unwrap().reason.as_deref(), Some("fed Syndra (AP)"));
    // Owning one completed item moves "Next" to the second slot, never offering what you own.
    let second = rerank(&mut LiveReranker::default(), &live(&AP_TEAM, &[3053, 1055], 9.0)).unwrap();
    assert_eq!(second.slot, 2);
    assert_ne!(second.item_id, 3053);
    assert!(second.alternatives.iter().all(|a| a.item_id != 3053));
}

#[test]
fn live_next_changes_only_after_another_item_leads_for_three_polls_in_a_row() {
    let mut r = LiveReranker::default();
    assert_eq!(item(&rerank(&mut r, &snap(&AP_TEAM))), 3053);
    assert_eq!(item(&rerank(&mut r, &snap(&AD_TEAM))), 3053); // BotRK leads: 1st poll
    assert_eq!(item(&rerank(&mut r, &snap(&AD_TEAM))), 3053); // 2nd
    assert_eq!(item(&rerank(&mut r, &snap(&AD_TEAM))), 3153); // 3rd: switch
    assert_eq!(item(&rerank(&mut r, &snap(&AP_TEAM))), 3153); // a single poll back does not flip it
    assert_eq!(item(&rerank(&mut r, &snap(&AD_TEAM))), 3153);
}

#[test]
fn live_components_in_progress_keep_their_item_as_next() {
    // Against AP, Sterak's leads; holding a Vampiric Scepter (BotRK component) keeps BotRK.
    let next = rerank(&mut LiveReranker::default(), &live(&AP_TEAM, &[1055, 1053], 9.0)).unwrap();
    assert_eq!(next.item_id, 3153);
    assert!(next.in_progress);
}

#[test]
fn live_a_rerank_takes_under_50_ms_with_ten_times_todays_games() {
    // ~1,000 games today: 12 copies of the 800 synthetic games, each a different pilot.
    let b = built();
    let many: Vec<_> = (0..12)
        .flat_map(|k| {
            b.m.scored.iter().map(move |s| {
                let mut g = (*s.g).clone();
                g.match_id = format!("{}-{k}", g.match_id);
                g.pilot = format!("{}-{k}", g.pilot);
                let mut s = s.clone();
                s.g = Arc::new(g);
                s
            })
        })
        .collect();
    let n = many.len();
    let big = build_draft_model(many, b.m.state.clone(), b.profiles.clone());
    let mut r = LiveReranker::default();
    rerank_with(&mut r, &snap(&AP_TEAM), &big); // first poll: builds the observation cache and searches the reason
    let mut times = vec![];
    for i in 0..15 {
        rerank_with(&mut r, &snap(if i % 2 == 1 { &AP_TEAM } else { &AD_TEAM }), &big);
        times.push(r.last_ms);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = times[times.len() / 2];
    println!("# re-rank over {n} games: median {median:.1} ms, max {:.1} ms", times[times.len() - 1]);
    assert!(median < 50.0, "median {median} ms");
}

// ------------------------------------------------------------------------------------------- B4 personal layer

#[test]
fn personal_your_timings_against_the_high_elo_median_and_options_shrunk_toward_high_elo() {
    let mine: Vec<_> = ap_ad_games(10, 1, 11, ApAd::default())
        .into_iter()
        .enumerate()
        .map(|(i, mut g)| {
            g.id = format!("KR_{}", 90_000 + i);
            g.pilot = "ME".into();
            g.items = vec![3153, 3078, 6333];
            g.win = true;
            g
        })
        .collect();
    let mut games = ap_ad_games(400, 40, 7, ApAd::default());
    games.extend(mine);
    let (prepared, _) = prepare(&games, Some("ME"));
    let state = train_state(prepared.iter().map(|g| g.as_ref()), 1.0, STATE_MAX_ROWS).unwrap();
    let report = personal_report(&score_games(&prepared, state.as_ref(), None), Some("TOP"));
    assert_eq!(report["games"], 10);
    assert_eq!(report["wins"], 10);
    let botrk = report["timings"].as_array().unwrap().iter().find(|t| t["itemId"] == 3153).unwrap();
    let you = botrk["you"].as_f64().unwrap();
    assert!((you - 10.0).abs() < 0.05); // bought at 10:00 in the fixture, like every high-elo game
    assert_eq!(botrk["highElo"].as_f64(), Some(you));
    let opt = report["options"].as_array().unwrap().iter().find(|o| o["key"] == "core1" && o["value"] == "3153").unwrap();
    assert_eq!(opt["games"], 10);
    let (yours, blended, high) = (opt["yourLift"].as_f64().unwrap(), opt["blended"].as_f64().unwrap(), opt["highEloLift"].as_f64().unwrap());
    // Ten wins pull the estimate up, but 30 high-elo pseudo-games hold it: blended sits between, nearer high-elo.
    assert!(yours > blended && blended > high);
    assert!((blended - (10.0 * yours + PERSONAL_K * high) / (10.0 + PERSONAL_K)).abs() < 1e-12);
}

#[test]
fn personal_what_was_shown_is_linked_to_the_game_it_was_shown_for() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open_default(&tmp.path().join("personal.db")).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let created = now - 3_600_000;
    let mut one = ap_ad_games(1, 1, 7, ApAd::default());
    one[0].id = "KR_777".into();
    one[0].pilot = "ME".into();
    one[0].creation = Some(created);
    let (rows, _, _) = model_inputs(&one);
    let mut g = rows.into_iter().next().unwrap();
    db.insert_match(&MatchRow {
        id: "KR_777".into(),
        platform: "KR".into(),
        patch: g.patch.clone(),
        version: g.game_version.clone().unwrap(),
        queue: 420,
        creation: created,
        duration: g.game_duration,
        has_irelia: true,
    })
    .unwrap();
    g.pilot_source = Some("me".into());
    db.insert_game(&g).unwrap();
    db.log_recommendation("draft:1", "draft", Some("TOP"), Some(134), "draft", &json!({ "keystone": 8010, "core": [3053, 3078, 6333], "boots": 3047, "items": [] })).unwrap();
    db.conn.execute("UPDATE rec_log SET created_at = ? WHERE session = ?", rusqlite::params![created - 5 * 60_000, "draft:1"]).unwrap();
    db.log_recommendation("draft:2", "draft", Some("TOP"), Some(134), "baseline", &json!({ "keystone": 8010, "core": [], "boots": null, "items": [] })).unwrap();
    db.conn.execute("UPDATE rec_log SET created_at = ? WHERE session = ?", rusqlite::params![created - 3 * 3_600_000, "draft:2"]).unwrap();
    assert_eq!(link_recommendations(&db, now).unwrap(), 1);
    let recs = db.recommendations_for("KR_777").unwrap();
    assert_eq!(recs[0].session, "draft:1");
    assert_eq!(recs[0].win, Some(g.win));
    let bought: Value = serde_json::from_str(recs[0].bought.as_deref().unwrap()).unwrap();
    assert_eq!(bought["keystone"], 8010);
    assert_eq!(db.recommendation("draft:2").unwrap().match_id, None); // three hours earlier: another game
    assert_eq!(link_recommendations(&db, now).unwrap(), 0); // idempotent
}
