//! Port of server/backfill.test.ts: upgrading a v0.3.0 data folder. The schema migration, the backfill from the disk
//! cache (idempotent, resumable), purging and archiving old patches, and the read-only audit, all on a fixture folder
//! made with the v2 schema and real-shaped cache files.

use irelia::capture::CAPTURE_VERSION;
use irelia::db::{Db, PilotUpdate, SCHEMA_VERSION};
use irelia::engine::parse_match::extract_irelia_game;
use irelia::patches::{region_patch, Fallback, RegionPatch};
use irelia::riot::disk_cache::DiskCache;
use irelia::riot::rate_limiter::Signal;
use irelia::scraper::backfill::{run_backfill, BackfillEnv, BackfillProgress};
use irelia::testkit::{make_match, make_timeline, MatchSpec, TimelineSpec, IRELIA};
use irelia_server::audit::{audit_data, format_audit};
use parking_lot::Mutex;
use rusqlite::types::ValueRef;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const DAY: i64 = 24 * 3_600_000;
const SYNDRA: i64 = 134;
const DARIUS: i64 = 122;

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

/// The database schema as v0.3.0 (schema v2) created it, from the TS tests' fixture.
fn schema_v2() -> String {
    let ts = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../server/test/schemaV2.ts")).unwrap();
    ts.split('`').nth(1).unwrap().to_string()
}

struct Folder {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    now: i64,
}

impl Folder {
    fn db_file(&self) -> PathBuf {
        self.dir.join("irelia.db")
    }
}

/// A v0.3.0 data folder: one OTP (P) with an Irelia game whose timeline shows a lane swap and a leaver, a remade
/// Irelia game, a match where another player (X) played Irelia top, an older-patch match, a match whose cache file is
/// gone, and a cached match the database never recorded.
fn v2_folder() -> Folder {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let now = now();
    let start = now - 9 * DAY;
    let raw = Connection::open(dir.join("irelia.db")).unwrap();
    raw.execute_batch(&schema_v2()).unwrap();
    let mut cache = DiskCache::new(&dir.join("cache"));
    {
        let mut player = raw.prepare("INSERT INTO players(puuid, platform, tier, league_points, is_otp, irelia_mastery, last_checked, updated_at) VALUES(?, ?, ?, ?, ?, ?, ?, ?)").unwrap();
        player.execute(params!["P", "KR", "CHALLENGER", 1200, 1, 400_000, now - DAY, now]).unwrap();
        player.execute(params!["Q", "KR", "CHALLENGER", 900, 0, 0, now - DAY, now]).unwrap();
        player.execute(params!["R", "KR", "GRANDMASTER", 500, None::<i64>, None::<i64>, None::<i64>, now]).unwrap();
    }
    cache.set("mastery", "KR", "P", &json!([{ "championId": IRELIA, "championPoints": 400_000, "lastPlayTime": now - DAY }])).unwrap();

    let mut store = |m: &Value, irelia: bool, timeline: Option<Option<Value>>, file: bool| {
        let id = m["metadata"]["matchId"].as_str().unwrap();
        let version = m["info"]["gameVersion"].as_str().unwrap();
        let patch = version.split('.').take(2).collect::<Vec<_>>().join(".");
        raw.execute(
            "INSERT INTO matches(match_id, platform, patch, game_version, queue_id, game_creation, game_duration, has_irelia, fetched_at) VALUES(?, ?, ?, ?, 420, ?, ?, ?, ?)",
            params![id, "KR", patch, version, m["info"]["gameCreation"].as_i64(), m["info"]["gameDuration"].as_i64(), irelia as i64, now],
        )
        .unwrap();
        if file {
            cache.set("match", "KR", id, m).unwrap();
        }
        let tl = timeline.unwrap_or_else(|| Some(make_timeline(m, &TimelineSpec::default())));
        if let (true, Some(tl)) = (irelia, tl) {
            cache.set("timeline", "KR", id, &tl).unwrap();
            let g = extract_irelia_game(m, &tl, &m["info"]["participants"][0]);
            raw.execute(
                "INSERT INTO irelia_games(match_id, puuid, patch, win, position, opponent_champion_id, summoner1, summoner2, primary_style,
                   sub_style, perks, stat_perks, skill_order, purchases, final_items, kills, deaths, assists, game_duration, created_at)
                 VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    g.match_id, g.puuid, g.patch, g.win, g.position, g.opponent_champion_id, g.summoner1, g.summoner2, g.primary_style,
                    g.sub_style, g.perks, g.stat_perks, g.skill_order, g.purchases, g.final_items, g.kills, g.deaths, g.assists,
                    g.game_duration, g.created_at
                ],
            )
            .unwrap();
        }
    };
    let puuids = |id: &str| -> Vec<String> { (0..10).map(|i| if i == 0 { "P".to_string() } else { format!("{id}-p{}", i + 1) }).collect() };

    store(&make_match(&MatchSpec { version: Some("16.18.700.1".into()), creation: Some(start - DAY), puuids: Some(puuids("KR_1")), ..MatchSpec::id("KR_1") }), false, None, true);
    let swap = make_match(&MatchSpec { creation: Some(now - 2 * DAY), puuids: Some(puuids("KR_2")), ..MatchSpec::id("KR_2") });
    let swap_tl = make_timeline(&swap, &TimelineSpec { spots: vec![(6, (7400.0, 7400.0)), (8, (1500.0, 12500.0))], leaver: Some((9, 20)), ..Default::default() });
    store(&swap, true, Some(Some(swap_tl)), true);
    store(&make_match(&MatchSpec { creation: Some(now - DAY), minutes: Some(4), remake_vote: Some(true), puuids: Some(puuids("KR_3")), ..MatchSpec::id("KR_3") }), true, None, true);
    let mut harvest = make_match(&MatchSpec {
        creation: Some(now - 3 * DAY),
        puuids: Some(puuids("KR_4")),
        champions: Some(vec![103, 64, 7, 222, 412, IRELIA, 121, SYNDRA, 21, 89]),
        ..MatchSpec::id("KR_4")
    });
    harvest["info"]["participants"][5]["puuid"] = json!("X");
    store(&harvest, false, None, true);
    store(&make_match(&MatchSpec { creation: Some(now - 4 * DAY), puuids: Some(puuids("KR_5")), ..MatchSpec::id("KR_5") }), false, None, false);
    // On disk, not in the database.
    cache.set("match", "KR", "KR_9", &make_match(&MatchSpec { creation: Some(now - 5 * DAY), ..MatchSpec::id("KR_9") })).unwrap();
    cache.close();
    drop(raw);
    Folder { _tmp: tmp, dir, now }
}

fn counts(db: &Db) -> Vec<(String, i64)> {
    db.row_counts().unwrap().into_iter().filter(|(k, _)| k != "players").collect()
}

fn count_map(pairs: &[(&str, i64)]) -> Vec<(String, i64)> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

fn rows(db: &Db, sql: &str) -> Vec<Value> {
    let mut st = db.conn.prepare(sql).unwrap();
    let names: Vec<String> = st.column_names().map(|s| s.to_string()).collect();
    let mut out = vec![];
    let mut q = st.query([]).unwrap();
    while let Some(r) = q.next().unwrap() {
        let mut o = serde_json::Map::new();
        for (i, n) in names.iter().enumerate() {
            let v = match r.get_ref(i).unwrap() {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(x) => json!(x),
                ValueRef::Real(x) => json!(x),
                ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                ValueRef::Blob(b) => json!(b),
            };
            o.insert(n.clone(), v);
        }
        out.push(Value::Object(o));
    }
    out
}

struct Env {
    db: Arc<Mutex<Db>>,
    cache: Arc<Mutex<DiskCache>>,
    now: i64,
}

impl Env {
    fn open(f: &Folder) -> Env {
        Env { db: Arc::new(Mutex::new(Db::open_default(&f.db_file()).unwrap())), cache: Arc::new(Mutex::new(DiskCache::new(&f.dir.join("cache")))), now: f.now }
    }
    fn run(&self, signal: &Signal, force: bool, batch: Option<i64>, on: &mut dyn FnMut(&BackfillProgress)) -> anyhow::Result<BackfillProgress> {
        let now = self.now;
        let fallback = Fallback { patch: "16.19".into(), previous_patch: "16.18".into() };
        let region = move |p: &str| -> RegionPatch { region_patch(p, &[], &fallback, now) };
        let env = BackfillEnv { db: &self.db, cache: &self.cache, champion_id: IRELIA, harvest: true, region: &region, me_puuid: None };
        run_backfill(&env, signal, on, force, batch)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.cache.lock().close();
    }
}

#[test]
fn migration_a_v2_database_gains_the_v3_tables_and_columns_in_place_idempotently_keeping_every_row() {
    let f = v2_folder();
    let now = f.now;
    let db = Db::open_default(&f.db_file()).unwrap();
    assert_eq!(db.get_meta("schema_version"), Some(SCHEMA_VERSION.to_string()));
    let all: Vec<(String, i64)> = db.row_counts().unwrap();
    for (k, v) in [("players", 3), ("matches", 5), ("irelia_games", 2), ("participants", 0), ("timelines", 0), ("game_versions", 0), ("champion_patch_stats", 0), ("match_queue", 0), ("archive", 0)] {
        assert_eq!(all.iter().find(|(n, _)| n == k).map(|x| x.1), Some(v), "{k}");
    }
    let games = rows(&db, "SELECT match_id, pilot_source, position_opponent_id, opponent_champion_id FROM irelia_games ORDER BY match_id");
    assert_eq!(
        games,
        vec![
            json!({ "match_id": "KR_2", "pilot_source": "otp", "position_opponent_id": DARIUS, "opponent_champion_id": DARIUS }),
            json!({ "match_id": "KR_3", "pilot_source": "otp", "position_opponent_id": DARIUS, "opponent_champion_id": DARIUS }),
        ]
    );
    let classes: Vec<Option<String>> = ["P", "Q", "R"].iter().map(|p| db.player(p).unwrap().pilot_class).collect();
    assert_eq!(classes, vec![Some("otp".into()), Some("none".into()), None]);
    assert_eq!(db.count_pending_detail(CAPTURE_VERSION), 5);
    // Checked before v3 (top-1 mastery, which can't tell a main): rechecked once, ranked with the unchecked players.
    let due = |db: &Db, lo: i64, hi: i64| {
        let mut v: Vec<String> = db.mastery_work("KR", now - 3 * DAY, now - 14 * DAY, lo, hi, 10).unwrap().into_iter().map(|p| p.puuid).collect();
        v.sort();
        v
    };
    assert_eq!(due(&db, 2, 2), vec!["P", "Q", "R"]);
    db.set_pilot("Q", &PilotUpdate { pilot_class: "none".into(), irelia_points: 0, irelia_last_play: None, irelia_rank: None }).unwrap();
    assert_eq!(due(&db, 2, 3), vec!["P", "R"], "once classified by v3, Q waits for its 14-day recheck");
    drop(db);
    let again = Db::open_default(&f.db_file()).unwrap(); // a second start changes nothing
    assert_eq!(again.row_counts().unwrap().iter().find(|(n, _)| n == "irelia_games").unwrap().1, 2);
}

fn after_backfill() -> Vec<(String, i64)> {
    count_map(&[("matches", 5), ("irelia_games", 2), ("participants", 40), ("timelines", 2), ("game_versions", 2), ("champion_patch_stats", 0), ("match_queue", 1), ("archive", 0)])
}

#[test]
fn backfill_participants_flags_timelines_and_lane_opponents_from_the_disk_cache_idempotent_harvests_cached_matches() {
    let f = v2_folder();
    let e = Env::open(&f);
    let mut seen: Vec<BackfillProgress> = vec![];
    let p = e.run(&Signal::new(), false, Some(2), &mut |x| seen.push(x.clone())).unwrap();
    assert_eq!([p.done, p.total, p.games, p.missing, p.harvested], [5, 5, 2, 1, 1]);
    assert!(p.finished_at.is_some() && seen.iter().any(|x| x.done == 2), "progress per batch");

    let db = e.db.lock();
    let first = counts(&db);
    assert_eq!(first, after_backfill());
    let swap = rows(&db, "SELECT opponent_champion_id, position_opponent_id, timeline_opponent_id, participant_id, pilot_tier, pilot_mastery FROM irelia_games WHERE match_id = 'KR_2'");
    assert_eq!(
        swap[0],
        json!({ "opponent_champion_id": SYNDRA, "position_opponent_id": DARIUS, "timeline_opponent_id": SYNDRA, "participant_id": 1, "pilot_tier": "CHALLENGER", "pilot_mastery": 400_000 })
    );
    let flags = rows(&db, "SELECT match_id, remake, leaver, detail_version FROM matches ORDER BY match_id");
    let flag = |id: &str| flags.iter().find(|f| f["match_id"] == id).unwrap().clone();
    assert_eq!(flag("KR_2"), json!({ "match_id": "KR_2", "remake": 0, "leaver": 1, "detail_version": CAPTURE_VERSION }));
    assert_eq!(flag("KR_3"), json!({ "match_id": "KR_3", "remake": 1, "leaver": 0, "detail_version": CAPTURE_VERSION }));
    assert_eq!(flag("KR_5")["remake"], Value::Null, "no cache file: nothing to capture");
    let q = db.next_queued(&["KR".to_string()]).unwrap();
    assert_eq!((q.match_id.as_str(), q.platform.as_str(), q.kind.as_str(), q.puuid.as_str(), q.source.as_str(), q.score), ("KR_4", "KR", "timeline", "X", "harvested", 1.0));
    assert_eq!(db.player("X").unwrap().found_by, "harvest");
    assert_eq!(db.player("P").unwrap().irelia_last_play, Some(f.now - DAY), "activity read from the cached mastery answer");
    drop(db);

    let again = e.run(&Signal::new(), false, None, &mut |_| {}).unwrap();
    assert_eq!([again.total, again.done], [0, 0]);
    let forced = e.run(&Signal::new(), true, None, &mut |_| {}).unwrap();
    assert_eq!(forced.done, 5);
    assert_eq!(counts(&e.db.lock()), first, "rerunning rewrites the same rows");
}

#[test]
fn backfill_interrupted_after_a_batch_resumes_where_it_stopped_and_ends_with_the_same_data() {
    let f = v2_folder();
    let e = Env::open(&f);
    let stop = Signal::new();
    let s2 = stop.clone();
    assert!(e.run(&stop, false, Some(2), &mut |x| if x.done >= 2 { s2.abort("stop") }).is_err());
    assert_eq!(e.db.lock().count_pending_detail(CAPTURE_VERSION), 3);
    let rest = e.run(&Signal::new(), false, Some(2), &mut |_| {}).unwrap();
    assert_eq!([rest.total, rest.done], [3, 3]);
    assert_eq!(counts(&e.db.lock()), after_backfill());
}

fn kept_syndra(db: &Db) -> Vec<Value> {
    rows(db, &format!("SELECT champion_id, games, dmg_magic FROM champion_patch_stats WHERE patch = '16.18' AND champion_id = {SYNDRA}"))
}

fn participants(db: &Db) -> i64 {
    db.row_counts().unwrap().iter().find(|(n, _)| n == "participants").unwrap().1
}

#[test]
fn purging_old_patches_keeps_per_champion_totals_of_the_purged_games() {
    let f = v2_folder();
    let e = Env::open(&f);
    e.run(&Signal::new(), false, None, &mut |_| {}).unwrap();
    let db = e.db.lock();
    assert_eq!(db.purge_stale(&["16.19".to_string()]).unwrap(), 1);
    assert_eq!(kept_syndra(&db), vec![json!({ "champion_id": SYNDRA, "games": 1, "dmg_magic": 25_000 })]);
    assert_eq!(participants(&db), 30);
}

#[test]
fn archiving_old_patches_stores_each_match_whole_and_compressed_restoring_brings_back_exactly_what_was_there() {
    let f = v2_folder();
    let e = Env::open(&f);
    e.run(&Signal::new(), false, None, &mut |_| {}).unwrap();
    let db = e.db.lock();
    let tables = ["matches", "irelia_games", "participants", "timelines", "champion_patch_stats"];
    let dump = |db: &Db| -> Vec<Vec<Value>> { tables.iter().map(|t| rows(db, &format!("SELECT * FROM {t} ORDER BY 1, 2, 3"))).collect() };
    let before = dump(&db);
    assert_eq!(before[3].len(), 2);
    let keep = ["16.19".to_string()];

    assert_eq!(db.archive_batch(&keep, 1).unwrap(), 1);
    assert_eq!(db.archive_batch(&keep, 1).unwrap(), 0, "nothing left to archive");
    assert_eq!(db.archive_summary().unwrap().iter().map(|a| (a.0.clone(), a.1)).collect::<Vec<_>>(), vec![("16.18".to_string(), 1)]);
    // As after purging: the match is out of the working tables, its per-champion totals are kept.
    assert_eq!(kept_syndra(&db), vec![json!({ "champion_id": SYNDRA, "games": 1, "dmg_magic": 25_000 })]);
    assert_eq!(participants(&db), 30);
    assert_eq!(db.row_counts().unwrap().iter().find(|(n, _)| n == "archive").unwrap().1, 1);

    assert_eq!(db.restore_batch(Some(&["16.17".to_string()]), 500).unwrap(), 0, "another patch: nothing to restore");
    assert_eq!(db.restore_batch(None, 500).unwrap(), 1);
    assert_eq!(dump(&db), before, "every row back as it was, the kept totals gone again");
    assert_eq!(db.row_counts().unwrap().iter().find(|(n, _)| n == "archive").unwrap().1, 0);
    // Archived and restored again: the totals don't add up twice.
    db.archive_batch(&keep, 500).unwrap();
    db.restore_batch(Some(&["16.18".to_string()]), 500).unwrap();
    assert_eq!(dump(&db), before);
}

#[test]
fn audit_read_only_on_a_v2_folder_it_measures_coverage_quality_lane_opponents_harvest_potential_and_the_yield_model() {
    let f = v2_folder();
    let dir = &f.dir;
    // The Data Dragon files the app caches in the folder (here: the tests' snapshot), for the build comparison.
    let snap: Value = serde_json::from_str(&std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../server/test/ddragon-snapshot.json")).unwrap()).unwrap();
    let dd = dir.join("ddragon").join(snap["version"].as_str().unwrap());
    std::fs::create_dir_all(&dd).unwrap();
    for (file, key) in [("item.json", "item"), ("champion.json", "champion"), ("runesReforged.json", "runesReforged"), ("summoner.json", "summoner"), ("Irelia.json", "champ")] {
        std::fs::write(dd.join(file), serde_json::to_string(&snap["raw"][key]).unwrap()).unwrap();
    }
    let before = std::fs::metadata(f.db_file()).unwrap().modified().unwrap();
    let r = audit_data(dir, f.now as f64, IRELIA, &|_, _| {}).unwrap();
    let n = |v: &Value| v.as_f64().unwrap();
    assert_eq!(n(&r["schemaVersion"]), 2.0);
    assert_eq!([n(&r["matches"]["total"]), n(&r["ireliaShare"]["ireliaGames"]), n(&r["ireliaShare"]["share"])], [5.0, 2.0, 0.4]);
    assert_eq!([n(&r["players"]["total"]), n(&r["players"]["checked"]), n(&r["players"]["unchecked"])], [3.0, 2.0, 1.0]);
    let q = &r["quality"];
    assert_eq!([n(&q["matchesRead"]), n(&q["remakes"]), n(&q["ireliaRemakes"]), n(&q["ireliaLeavers"]), n(&q["timelinesRead"])], [4.0, 1.0, 1.0, 1.0, 2.0]);
    let lo = &r["laneOpponent"];
    assert_eq!([n(&lo["compared"]), n(&lo["disagree"]), n(&lo["share"])], [1.0, 1.0, 1.0]);
    let h = &r["harvest"];
    assert_eq!([n(&h["cachedMatchesWithOtherIrelia"]), n(&h["currentPatchNotRemake"]), n(&h["players"])], [1.0, 1.0, 1.0]);
    assert_eq!(r["cache"], json!({ "matchFiles": 5, "timelineFiles": 2, "masteryFiles": 1, "matchesWithoutFile": 1, "gamesWithoutTimeline": 0, "matchFilesNotInDb": 1, "timelineFilesNotInDb": 0 }));
    let builds: Vec<(String, String)> = r["builds"].as_array().unwrap().iter().map(|b| (b["platform"].as_str().unwrap().into(), b["patch"].as_str().unwrap().into())).collect();
    assert_eq!(builds, vec![("KR".to_string(), "16.19".to_string()), ("KR".to_string(), "16.18".to_string())]);
    assert_eq!([n(&r["yield"]["pilots"]), n(&r["yield"]["activeShare"])], [1.0, 1.0]);
    assert!(n(&r["disk"]["avgTimelineBytes"]) > 0.0 && n(&r["disk"]["per1000Games"]["totalMB"]) > 0.0);
    assert_eq!(r["projection"][0]["option"], "as now (20 newest ids)");
    let text = format_audit(&r);
    let at = text.find("Yield model").expect("yield model section");
    assert!(text[at..].contains("whole-patch pagination"));
    let top: Vec<&Value> = r["buildImpact"].as_array().unwrap().iter().filter(|b| b["lane"] == "TOP").collect();
    let variants: Vec<&str> = top.iter().map(|b| b["variant"].as_str().unwrap()).collect();
    assert_eq!(variants, vec!["v0.3.0", "+ remakes", "+ leavers", "+ timeline opponent", "+ patch per region"]);
    let v = |name: &str| *top.iter().find(|b| b["variant"] == name).unwrap();
    assert_eq!([n(&v("v0.3.0")["used"]), n(&v("+ remakes")["used"]), n(&v("+ leavers")["used"])], [2.0, 1.0, 0.0], "the remake, then the leaver game drop out");
    assert_eq!(n(&v("+ remakes")["discarded"]["remake"]), 1.0);
    assert_eq!(v("v0.3.0")["topMatchups"], json!([{ "id": DARIUS, "games": 2 }]));
    let mut tl: Vec<(i64, i64)> = v("+ timeline opponent")["topMatchups"].as_array().unwrap().iter().map(|m| (m["id"].as_i64().unwrap(), m["games"].as_i64().unwrap())).collect();
    tl.sort();
    assert_eq!(tl, vec![(DARIUS, 1), (SYNDRA, 1)], "the lane swap counts against Syndra");

    // Nothing was written: no v3 tables, no schema version.
    let raw = Connection::open_with_flags(f.db_file(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut st = raw.prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name").unwrap();
    let tables: Vec<String> = st.query_map([], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect();
    assert_eq!(tables, vec!["irelia_games", "matches", "meta", "players"]);
    assert_eq!(std::fs::metadata(f.db_file()).unwrap().modified().unwrap(), before);
}
