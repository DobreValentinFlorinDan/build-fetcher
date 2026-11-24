//! Port of server/sourcing.test.ts (the patch window, pilots, the per-host lanes, the new patch's recheck) and
//! server/account.test.ts, against the in-memory Riot API.

use irelia::db::{Db, MatchRow, PilotUpdate};
use irelia::patches::Fallback;
use irelia::riot::client::MasteryDto;
use irelia::riot::rate_limiter::Signal;
use irelia::scope::{default_scope, parse_scope, ScrapeScope};
use irelia::scraper::account::{is_client_puuid, resolve_account, ClientAccount, Resolved};
use irelia::scraper::pilots::{classify_pilot, is_active};
use irelia::scraper::sourcing::{LaneKey, Sourcing, SourcingEnv};
use irelia::testkit::{make_match, FakeRiot, MatchSpec, IRELIA};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;

const DAY: i64 = 24 * 3_600_000;
const AHRI: i64 = 103;
const YASUO: i64 = 157;
const SYNDRA: i64 = 134;
const DARIUS: i64 = 122;

fn now() -> i64 {
    // Fixed for the whole test run (Date.now() at the start of the TS file).
    static NOW: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *NOW.get_or_init(irelia::util::now_ms)
}
fn patch_start() -> i64 {
    now() - 9 * DAY
}

struct Setup {
    db: Arc<Mutex<Db>>,
    riot: Arc<FakeRiot>,
    s: Arc<Sourcing<FakeRiot>>,
    logs: Arc<Mutex<Vec<String>>>,
    _dir: tempfile::TempDir,
}

fn scope_with(extra: Value) -> ScrapeScope {
    let mut input = json!({ "platforms": ["KR"], "tiers": ["CHALLENGER"] });
    for (k, v) in extra.as_object().unwrap() {
        input[k] = v.clone();
    }
    parse_scope(input.as_object().unwrap(), &default_scope()).unwrap()
}

fn setup(extra: Value) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Mutex::new(Db::open_default(&dir.path().join("s.db")).unwrap()));
    let riot = Arc::new(FakeRiot::default());
    let logs = Arc::new(Mutex::new(vec![]));
    let l2 = logs.clone();
    let scope = scope_with(extra);
    let s = Sourcing::new(SourcingEnv {
        db: db.clone(),
        riot: riot.clone(),
        scope: Arc::new(move || scope.clone()),
        fallback_patch: Arc::new(|| Fallback { patch: "16.19".into(), previous_patch: "16.18".into() }),
        me: Arc::new(|| None),
        concurrency: 2,
        log: Arc::new(move |t| l2.lock().push(t.to_string())),
        now: Some(Arc::new(now)),
        recheck_pilots_ms: None,
        recheck_others_ms: None,
        reseed_ms: None,
        rescan_pilot_ms: None,
    });
    Setup { db, riot, s, logs, _dir: dir }
}

fn insert_match(db: &Db, id: &str, patch: &str, version: &str, creation: i64) {
    db.insert_match(&MatchRow { id: id.into(), platform: "KR".into(), patch: patch.into(), version: version.into(), queue: 420, creation, duration: 1800, has_irelia: false }).unwrap();
}

fn seed_region(db: &Db) {
    insert_match(db, "KR_100", "16.18", "16.18.700.1", patch_start() - 3_600_000);
    insert_match(db, "KR_101", "16.19", "16.19.712.4471", patch_start() + 3_600_000);
}

#[derive(Default)]
struct G {
    champion: Option<i64>,
    ago: Option<f64>,
    version: Option<&'static str>,
    position: Option<&'static str>,
}

fn game(id: &str, puuid: &str, o: G) -> Value {
    let mut champions = vec![o.champion.unwrap_or(IRELIA), 64, if o.champion == Some(AHRI) { 7 } else { AHRI }, 222, 412, DARIUS, 121, SYNDRA, 21, 89];
    let mut puuids: Vec<String> = (0..10).map(|i| format!("{id}-p{}", i + 1)).collect();
    let mut positions: Vec<String> = ["TOP", "JUNGLE", "MIDDLE", "BOTTOM", "UTILITY", "TOP", "JUNGLE", "MIDDLE", "BOTTOM", "UTILITY"].iter().map(|s| s.to_string()).collect();
    puuids[0] = puuid.into();
    if let Some(p) = o.position {
        positions[0] = p.into();
    }
    champions.truncate(10);
    make_match(&MatchSpec {
        id: id.into(),
        puuids: Some(puuids),
        champions: Some(champions),
        positions: Some(positions),
        creation: Some(now() - (o.ago.unwrap_or(1.0) * DAY as f64) as i64),
        version: Some(o.version.unwrap_or("16.19.712.4471").into()),
        ..Default::default()
    })
}

async fn run_all(s: &Arc<Sourcing<FakeRiot>>) -> usize {
    for i in 0..200 {
        let jobs: Vec<_> = s.lanes().iter().filter_map(|l| s.next_job(l).unwrap()).collect();
        if jobs.is_empty() {
            return i;
        }
        let runs: Vec<_> = jobs.into_iter().map(|j| (j.run)(Signal::new())).collect();
        for r in futures_join(runs).await {
            r.unwrap();
        }
    }
    panic!("lanes never ran out of work");
}

async fn futures_join<T>(fs: Vec<futures_util::future::BoxFuture<'static, T>>) -> Vec<T> {
    futures_util::future::join_all(fs).await
}

fn mastery(champion: i64, points: i64, last: Option<i64>) -> MasteryDto {
    MasteryDto { champion_id: champion, champion_points: points, last_play_time: last }
}

#[tokio::test]
async fn match_ids_are_paged_from_the_patch_start() {
    let t = setup(json!({}));
    seed_region(&t.db.lock());
    for i in 0..230 {
        t.riot.add(game(&format!("KR_{}", 2000 + i), "pilot", G { ago: Some(0.01 + (i as f64 * 8.0) / 230.0), ..Default::default() }), None);
    }
    for i in 0..20 {
        t.riot.add(game(&format!("KR_{}", 1000 + i), "pilot", G { ago: Some(10.0 + i as f64 * 0.1), version: Some("16.18.700.1"), ..Default::default() }), None);
    }
    assert_eq!(t.s.region("KR").start, patch_start() - 3_600_000);
    let sig = Signal::new();
    let ids = t.s.list_history("pilot", "KR", &sig, None, None, None, false).await.unwrap();
    assert_eq!(ids.len(), 230);
    let pages: Vec<Value> = t.riot.calls.lock().iter().filter(|c| c.0 == "matchIdsPage").map(|c| c.1[2].clone()).collect();
    assert_eq!(pages.iter().map(|p| (p["start"].as_i64().unwrap(), p["count"].as_i64().unwrap())).collect::<Vec<_>>(), vec![(0, 100), (100, 100), (200, 100)]);
    assert!(pages.iter().all(|p| p["startTime"].as_i64() == Some((patch_start() - 3_600_000) / 1000)));

    let capped = setup(json!({ "maxMatchesPerPilot": 150 }));
    seed_region(&capped.db.lock());
    *capped.riot.histories.lock() = t.riot.histories.lock().clone();
    assert_eq!(capped.s.list_history("pilot", "KR", &sig, None, None, None, false).await.unwrap().len(), 150);

    for id in &ids[..100] {
        insert_match(&t.db.lock(), id, "16.19", "16.19.712.4471", now());
    }
    t.riot.calls.lock().clear();
    t.s.list_history("pilot", "KR", &sig, None, None, None, false).await.unwrap();
    assert_eq!(t.riot.count("matchIdsPage"), 1);

    let newest = setup(json!({ "historyWindow": "newest" }));
    *newest.riot.histories.lock() = t.riot.histories.lock().clone();
    assert_eq!(newest.s.list_history("pilot", "KR", &sig, None, None, None, false).await.unwrap().len(), 20);
}

#[test]
fn pilot_classes() {
    let scope = scope_with(json!({}));
    let n = now();
    let otp = classify_pilot(&[mastery(IRELIA, 400_000, Some(n))], None, IRELIA, &scope);
    assert_eq!(otp, PilotUpdate { pilot_class: "otp".into(), irelia_points: 400_000, irelia_last_play: Some(n), irelia_rank: Some(1) });
    let main = classify_pilot(&[mastery(YASUO, 900_000, None), mastery(IRELIA, 120_000, Some(n - DAY))], None, IRELIA, &scope);
    assert_eq!((main.pilot_class.as_str(), main.irelia_rank), ("main", Some(2)));
    assert_eq!(classify_pilot(&[mastery(IRELIA, 90_000, None)], None, IRELIA, &scope).pilot_class, "none");
    let deep = classify_pilot(&[mastery(YASUO, 900_000, None)], Some(&mastery(IRELIA, 300_000, Some(n))), IRELIA, &scope);
    assert_eq!((deep.pilot_class.as_str(), deep.irelia_rank, deep.irelia_points), ("main", None, 300_000));
    let mut p = irelia::db::PlayerRow { puuid: "x".into(), platform: "KR".into(), irelia_mastery: None, tier: "".into(), league_points: 0, is_otp: None, last_checked: None, last_match_scan: None, pilot_class: None, irelia_points: None, irelia_last_play: Some(n - DAY), irelia_rank: None, found_by: "ladder".into(), history_matches: 0, history_irelia: 0, rank_checked: None, prio: None };
    assert!(is_active(&p, patch_start()));
    p.irelia_last_play = Some(patch_start() - DAY);
    assert!(!is_active(&p, patch_start()));
    p.irelia_last_play = None;
    assert!(is_active(&p, patch_start()));
}

#[tokio::test]
async fn per_host_lanes_scan_otps_and_active_mains() {
    let t = setup(json!({ "pilotClasses": ["otp", "main"] }));
    seed_region(&t.db.lock());
    t.riot.ladders.lock().insert("KR:CHALLENGER".into(), ["A", "B", "C", "D"].iter().enumerate().map(|(i, p)| irelia::riot::client::LeagueEntry { puuid: p.to_string(), league_points: 1000 - i as i64 }).collect());
    let n = now();
    t.riot.masteries.lock().insert("A".into(), vec![mastery(IRELIA, 400_000, Some(n - DAY))]);
    t.riot.masteries.lock().insert("B".into(), vec![mastery(YASUO, 500_000, None), mastery(IRELIA, 200_000, Some(n - 2 * DAY))]);
    t.riot.masteries.lock().insert("C".into(), vec![mastery(AHRI, 800_000, None)]);
    t.riot.masteries.lock().insert("D".into(), vec![mastery(IRELIA, 300_000, Some(patch_start() - 5 * DAY))]);
    for i in 0..3 {
        t.riot.add_with_timeline(game(&format!("KR_{}", 3000 + i), "A", G { ago: Some(1.0 + i as f64), ..Default::default() }));
    }
    t.riot.add_with_timeline(game("KR_3010", "A", G { champion: Some(AHRI), ago: Some(0.5), ..Default::default() }));
    t.riot.add_with_timeline(game("KR_2990", "A", G { ago: Some(12.0), version: Some("16.18.700.1"), ..Default::default() }));
    for i in 0..2 {
        t.riot.add_with_timeline(game(&format!("KR_{}", 3100 + i), "B", G { ago: Some(2.0 + i as f64), position: Some("MIDDLE"), ..Default::default() }));
    }
    t.riot.add_with_timeline(game("KR_2900", "D", G { ago: Some(20.0), version: Some("16.18.700.1"), ..Default::default() }));

    let lanes = t.s.lanes();
    assert_eq!(lanes, vec![LaneKey::Platform("KR".into()), LaneKey::Regional { region: "ASIA".into(), platforms: vec!["KR".into()] }]);
    assert_eq!(t.s.next_job(&lanes[0]).unwrap().unwrap().label, "Seeding KR Challenger ladder");
    run_all(&t.s).await;

    let db = t.db.lock();
    let mut st = db.conn.prepare("SELECT match_id, pilot_source FROM irelia_games ORDER BY match_id").unwrap();
    let games: Vec<(String, String)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|x| x.unwrap()).collect();
    assert_eq!(games, vec![
        ("KR_3000".to_string(), "otp".to_string()), ("KR_3001".into(), "otp".into()), ("KR_3002".into(), "otp".into()),
        ("KR_3100".into(), "main".into()), ("KR_3101".into(), "main".into()),
    ]);
    let mut st = db.conn.prepare("SELECT puuid, pilot_class FROM players ORDER BY puuid").unwrap();
    let classes: Vec<(String, String)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|x| x.unwrap()).collect();
    assert_eq!(classes, vec![("A".to_string(), "otp".to_string()), ("B".into(), "main".into()), ("C".into(), "none".into()), ("D".into(), "otp".into())]);
    let mut listed: Vec<String> = t.riot.calls.lock().iter().filter(|c| c.0 == "matchIdsPage").map(|c| c.1[1].as_str().unwrap().to_string()).collect();
    listed.sort();
    listed.dedup();
    assert_eq!(listed, vec!["A", "B"]);
    assert_eq!(t.riot.count("match"), 6);
    assert_eq!(t.riot.count("timeline"), 5);
    assert_eq!(db.player("A").unwrap().history_matches, 4);
    assert_eq!(db.player("A").unwrap().history_irelia, 3);
    assert!(t.logs.lock().iter().any(|l| l.contains("Irelia main found")));
}

#[tokio::test]
async fn a_new_patch_rechecks_every_pilot_at_once() {
    let t = setup(json!({ "pilotClasses": ["otp", "main"] }));
    let n = now();
    {
        let db = t.db.lock();
        insert_match(&db, "KR_100", "16.18", "16.18.700.1", n - 13 * 3_600_000);
        insert_match(&db, "KR_101", "16.19", "16.19.712.4471", n - 11 * 3_600_000);
        db.set_meta("seeded_at:KR:CHALLENGER", &n.to_string()).unwrap();
        for p in ["E", "F"] {
            db.upsert_player(p, "KR", "CHALLENGER", 900).unwrap();
            db.set_pilot(p, &PilotUpdate { pilot_class: "otp".into(), irelia_points: 500_000, irelia_last_play: Some(n - 2 * DAY), irelia_rank: Some(1) }).unwrap();
        }
        db.conn.execute("UPDATE players SET last_checked = ?, is_otp = 1 WHERE puuid = 'E'", [n - DAY]).unwrap();
        db.conn.execute("UPDATE players SET last_checked = ?, is_otp = 1, irelia_last_play = ? WHERE puuid = 'F'", [n - 3_600_000, n - 3 * 3_600_000]).unwrap();
    }
    let lanes = t.s.lanes();
    let job = t.s.next_job(&lanes[0]).unwrap().unwrap();
    assert_eq!(job.label, "Refreshing pilot activity");
    t.riot.masteries.lock().insert("E".into(), vec![mastery(IRELIA, 500_000, Some(n - 2 * 3_600_000))]);
    t.riot.add_with_timeline(game("KR_4000", "E", G { ago: Some(2.0 / 24.0), ..Default::default() }));
    let before = t.riot.calls.lock().len();
    (job.run)(Signal::new()).await.unwrap();
    let mut checked: Vec<String> = t.riot.calls.lock()[before..].iter().filter(|c| c.0 == "topMasteries" || c.0 == "championMastery").map(|c| c.1[1].as_str().unwrap().to_string()).collect();
    checked.dedup();
    assert_eq!(checked, vec!["E"]);
    assert_eq!(t.s.next_job(&lanes[1]).unwrap().unwrap().label, "Reading KR OTP match history");
    run_all(&t.s).await;
    let stored: i64 = t.db.lock().conn.query_row("SELECT COUNT(*) FROM irelia_games WHERE match_id = 'KR_4000'", [], |r| r.get(0)).unwrap();
    assert_eq!(stored, 1);
}

// ---- account.test.ts

const CLIENT_PUUID: &str = "5f3e2a1b-7c9d-4e8f-a1b2-c3d4e5f60718";
const API_PUUID: &str = "Xq3-ApiPuuid_78chars-encrypted-for-this-key";

fn account_riot() -> FakeRiot {
    let r = FakeRiot::default();
    r.accounts.lock().insert("irelia main#euw".into(), API_PUUID.into());
    r
}
fn acc(platform: Option<&str>) -> ClientAccount {
    ClientAccount { game_name: Some("Irelia Main".into()), tag_line: Some("EUW".into()), platform: platform.map(String::from) }
}

#[tokio::test]
async fn the_api_puuid_comes_from_the_riot_id() {
    let r = account_riot();
    let s = Signal::new();
    let a = resolve_account(&r, &acc(Some("EUW1")), None, &s).await.unwrap();
    assert_eq!(a, Resolved::Ok { puuid: API_PUUID.into(), platform: "EUW1".into(), riot_id: "Irelia Main#EUW".into() });
    let call = r.calls.lock().iter().find(|c| c.0 == "accountByRiotId").cloned().unwrap();
    assert_eq!(call.1, vec![json!("Irelia Main"), json!("EUW"), json!("europe.api.riotgames.com")]);
    assert!(!r.calls.lock().iter().any(|c| c.1.contains(&json!(CLIENT_PUUID))));
    let eune = resolve_account(&r, &acc(Some("EUNE")), None, &s).await.unwrap();
    assert!(matches!(eune, Resolved::Ok { ref platform, .. } if platform == "EUN1"));
}

#[tokio::test]
async fn without_a_server_from_the_client_riot_then_settings() {
    let r = account_riot();
    let s = Signal::new();
    let none = resolve_account(&r, &acc(None), None, &s).await.unwrap();
    assert!(matches!(none, Resolved::Err(ref e) if e.contains("Could not tell which server Irelia Main#EUW plays on: choose it in Settings")));
    r.regions.lock().insert(API_PUUID.into(), "euw1".into());
    let from_riot = resolve_account(&r, &acc(None), None, &s).await.unwrap();
    assert!(matches!(from_riot, Resolved::Ok { ref platform, .. } if platform == "EUW1"));
    let manual = resolve_account(&r, &acc(Some("EUW1")), Some("EUN1"), &s).await.unwrap();
    assert!(matches!(manual, Resolved::Ok { ref platform, .. } if platform == "EUN1"));
}

#[tokio::test]
async fn an_unknown_riot_id_or_none_yet_says_so() {
    let r = account_riot();
    let s = Signal::new();
    let unknown = resolve_account(&r, &ClientAccount { game_name: Some("Nobody".into()), tag_line: Some("X".into()), platform: Some("KR".into()) }, None, &s).await.unwrap();
    assert!(matches!(unknown, Resolved::Err(ref e) if e.contains("Riot doesn't know the account Nobody#X")));
    let early = resolve_account(&r, &ClientAccount { game_name: None, tag_line: None, platform: Some("KR".into()) }, None, &s).await.unwrap();
    assert!(matches!(early, Resolved::Err(ref e) if e.contains("did not give your Riot ID")));
}

#[test]
fn server_names_and_client_puuids() {
    use irelia::riot::routing::to_platform;
    let got: Vec<Option<String>> = ["EUW1", "euw1", "EUW", "EUNE", "LAN", "oce", "KR", "PBE", ""].iter().map(|v| to_platform(Some(v))).collect();
    let want: Vec<Option<String>> = [Some("EUW1"), Some("EUW1"), Some("EUW1"), Some("EUN1"), Some("LA1"), Some("OC1"), Some("KR"), None, None].iter().map(|v| v.map(String::from)).collect();
    assert_eq!(got, want);
    assert!(is_client_puuid(CLIENT_PUUID));
    assert!(!is_client_puuid(API_PUUID));
}
