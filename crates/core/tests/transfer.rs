//! Port of server/transfer.test.ts: backing up the pool (0.18.0). ZIP export and import of the pool, other players' IDs
//! left out, blending two computers' pools (0.20.1), and keeping only the newest exports (each computer its own three).

use irelia::db::{Db, MatchRow, OpenOptions, PilotUpdate};
use irelia::patches::Fallback;
use irelia::riot::rate_limiter::Signal;
use irelia::scope::{default_scope, parse_scope};
use irelia::scraper::fetch::{fetch_match, IngestContext, IngestEnv};
use irelia::scraper::ingest::store_match;
use irelia::scraper::sourcing::{Sourcing, SourcingEnv};
use irelia::testkit::{date_utc, make_match, FakeRiot, MatchSpec, IRELIA};
use irelia::transfer::{export_file_name, export_owner, export_pool, import_export, inspect_export, prune_exports, POOL_EXPORT};
use irelia::zip::ZipWriter;
use parking_lot::Mutex;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const DAY: i64 = 24 * 3_600_000;

fn now() -> i64 {
    static NOW: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *NOW.get_or_init(irelia::util::now_ms)
}

fn quiet(_: &str, _: Option<f64>) {}

fn open(dir: &Path) -> Db {
    Db::open(&dir.join("irelia.db"), OpenOptions { migrate: false, busy_timeout_ms: 5000 }).unwrap()
}

/// A pool (its own folder) with `games` Irelia games of the one-trick P, and a ladder player no game mentions.
async fn pool(root: &Path, dir: &str, games: usize) -> PathBuf {
    let data_dir = root.join(dir);
    std::fs::create_dir_all(&data_dir).unwrap();
    let db = Arc::new(Mutex::new(Db::open_default(&data_dir.join("irelia.db")).unwrap()));
    let riot = Arc::new(FakeRiot::default());
    let scope = parse_scope(json!({ "platforms": ["KR"], "tiers": ["CHALLENGER"] }).as_object().unwrap(), &default_scope()).unwrap();
    let s = Sourcing::new(SourcingEnv {
        db: db.clone(),
        riot: riot.clone(),
        scope: Arc::new(move || scope.clone()),
        fallback_patch: Arc::new(|| Fallback { patch: "16.19".into(), previous_patch: "16.18".into() }),
        me: Arc::new(|| None),
        concurrency: 1,
        log: Arc::new(|_| {}),
        now: Some(Arc::new(now)),
        recheck_pilots_ms: None,
        recheck_others_ms: None,
        reseed_ms: None,
        rescan_pilot_ms: None,
    });
    {
        let d = db.lock();
        // Brackets the KR patch, as the sourcing tests do.
        for (id, patch, version, ago) in [("KR_100", "16.18", "16.18.700.1", 12), ("KR_101", "16.19", "16.19.712.4471", 8)] {
            d.insert_match(&MatchRow { id: id.into(), platform: "KR".into(), patch: patch.into(), version: version.into(), queue: 420, creation: now() - ago * DAY, duration: 1800, has_irelia: false })
                .unwrap();
        }
        d.upsert_player("P", "KR", "CHALLENGER", 900).unwrap();
        d.set_pilot("P", &PilotUpdate { pilot_class: "otp".into(), irelia_points: 800_000, irelia_last_play: Some(now()), irelia_rank: Some(1) }).unwrap();
        d.upsert_player("LADDER-ONLY", "KR", "MASTER", 1).unwrap();
    }
    let region = |p: &str| s.region(p);
    let env = IngestEnv { db: &db, riot: &*riot, champion_id: IRELIA, region: &region, harvest: false, me_puuid: None };
    for i in 0..games {
        let id = format!("KR_{}", 500 + i);
        let champions = vec![IRELIA, 64, 103, 222, 412, 122, 121, 134, 21, 89];
        let puuids = (0..champions.len()).map(|k| if k == 0 { "P".to_string() } else { format!("{id}-p{}", k + 1) }).collect();
        let creation = now() - ((0.5 + i as f64 * 0.3) * DAY as f64) as i64;
        riot.add_with_timeline(make_match(&MatchSpec { champions: Some(champions), puuids: Some(puuids), creation: Some(creation), version: Some("16.19.712.4471".into()), ..MatchSpec::id(&id) }));
        let ctx = IngestContext { pilot_puuid: Some("P".into()), source: "otp".into(), keep_previous_patch: false };
        let fetched = fetch_match(&env, "KR", &id, &ctx, &Signal::new()).await.unwrap();
        assert_eq!(store_match(&db.lock(), &id, &fetched).unwrap(), "irelia");
    }
    data_dir
}

fn count(db: &Db, sql: &str) -> i64 {
    db.conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[tokio::test]
async fn the_pool_exported_on_one_computer_and_imported_on_another_same_games_other_players_stripped_nothing_replaced() {
    let root = tempfile::tempdir().unwrap();
    let a = pool(root.path(), "pc-a", 6).await;
    let out = root.path().join("drive");
    let ex = export_pool(&a, &out, "test", &quiet).unwrap();
    let file = PathBuf::from(ex["file"].as_str().unwrap());
    assert_eq!(file.file_name().unwrap().to_string_lossy(), export_file_name(POOL_EXPORT, ex["exportedAt"].as_i64().unwrap()));
    assert_eq!(ex["games"], json!(6));
    assert!(
        !std::fs::read_dir(&out).unwrap().any(|e| {
            let n = e.unwrap().file_name().to_string_lossy().into_owned();
            n.contains(".partial") || n.starts_with(".irelia-transfer")
        }),
        "nothing half-written left"
    );
    let (manifest, entries) = inspect_export(&file).unwrap();
    assert_eq!((manifest["kind"].clone(), manifest["name"].clone(), manifest["stripped"].clone()), (json!("pool"), json!(POOL_EXPORT), json!(true)));
    let mut names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
    names.sort();
    assert_eq!(names, ["irelia-pool.db", "manifest.json"]);

    // Another computer: a fresh pool. Importing changes its builds, so it has to be confirmed.
    let b_dir = root.path().join("pc-b");
    std::fs::create_dir_all(&b_dir).unwrap();
    drop(Db::open_default(&b_dir.join("irelia.db")).unwrap());
    let e = import_export(&b_dir, &file, false, &quiet).unwrap_err().to_string();
    assert!(e.contains("confirm it first"), "{e}");
    let r = import_export(&b_dir, &file, true, &quiet).unwrap();
    assert_eq!((r["name"].clone(), r["added"].clone()), (json!(POOL_EXPORT), json!(6)));
    {
        let b = open(&b_dir);
        assert_eq!(b.irelia_match_ids().unwrap().len(), 6);
        assert_eq!(b.player("P").and_then(|p| p.pilot_class), Some("otp".into()), "the pilot it follows comes along");
        assert!(b.player("LADDER-ONLY").is_none(), "ladder players that are not pilots stay behind");
        let mut st = b.conn.prepare("SELECT DISTINCT puuid FROM participants ORDER BY puuid").unwrap();
        let ids: Vec<String> = st.query_map([], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect();
        assert_eq!(ids, ["", "P"], "only the pilot keeps an ID");
    }
    // The original keeps every ID: stripping happens on the export copy only.
    assert!(count(&open(&a), "SELECT COUNT(DISTINCT puuid) FROM participants") > 2);

    // Importing again adds nothing.
    assert_eq!(import_export(&b_dir, &file, true, &quiet).unwrap()["added"], json!(0));
}

#[tokio::test]
async fn two_computers_blend_their_pools_each_import_adds_only_the_games_the_other_has_both_ways() {
    let root = tempfile::tempdir().unwrap();
    // The same pilot's games: KR_500 to KR_505 on the first computer, KR_500 to KR_508 on the second (6 in common).
    let a = pool(root.path(), "blend-a", 6).await;
    let c = pool(root.path(), "blend-c", 9).await;
    let from_c = export_pool(&c, &root.path().join("blend-drive"), "test", &quiet).unwrap();
    assert_eq!(import_export(&a, Path::new(from_c["file"].as_str().unwrap()), true, &quiet).unwrap()["added"], json!(3), "only the 3 games the first lacked");
    assert_eq!(open(&a).irelia_match_ids().unwrap().len(), 9);
    // Back the other way: nothing new for the second computer, and nothing it had is touched.
    let from_a = export_pool(&a, &root.path().join("blend-drive-2"), "test", &quiet).unwrap();
    assert_eq!(import_export(&c, Path::new(from_a["file"].as_str().unwrap()), true, &quiet).unwrap()["added"], json!(0));
    assert_eq!(open(&c).irelia_match_ids().unwrap().len(), 9);
}

#[test]
fn only_the_newest_exports_are_kept_and_only_files_named_like_ours_are_touched() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("prune");
    std::fs::create_dir_all(&dir).unwrap();
    let at = |d: u32| date_utc(2026, 9, 1 + d) + 12 * 3_600_000;
    for i in 0..5 {
        std::fs::write(dir.join(export_file_name(POOL_EXPORT, at(i))), "x").unwrap();
    }
    std::fs::write(dir.join(format!("{POOL_EXPORT} notes.zip")), "mine").unwrap();
    std::fs::write(dir.join("holiday.zip"), "mine").unwrap();
    assert_eq!(prune_exports(&dir, POOL_EXPORT, 3, None), 2);
    let left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(left.iter().filter(|f| f.starts_with("irelia-pool_")).count(), 3);
    assert!(left.contains(&format!("{POOL_EXPORT} notes.zip")) && left.contains(&"holiday.zip".to_string()));
    assert!(!left.contains(&export_file_name(POOL_EXPORT, at(0))), "the oldest went");
    assert_eq!(export_owner(&export_file_name(POOL_EXPORT, now())), Some(POOL_EXPORT.to_string()));
    assert_eq!(export_owner("holiday.zip"), None);
    assert_eq!(export_owner(&export_file_name("Agurin-EUW_dataset", now())), None, "0.18.0 data set exports are not ours any more");
}

fn zip_with(file: &Path, manifest: serde_json::Value) {
    let mut w = ZipWriter::new(file);
    w.add_buffer("manifest.json", manifest.to_string().as_bytes()).unwrap();
    w.add_buffer("x.db", b"x").unwrap();
    w.finish().unwrap();
}

#[test]
fn two_computers_backing_up_into_one_folder_each_keep_their_own_newest_three() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("shared-folder");
    std::fs::create_dir_all(&dir).unwrap();
    let write = |machine: &str, day: u32| {
        let name = export_file_name(POOL_EXPORT, date_utc(2026, 10, day) + 12 * 3_600_000);
        zip_with(
            &dir.join(&name),
            json!({ "format": "irelia-companion-export", "version": 1, "kind": "pool", "name": POOL_EXPORT, "machine": machine, "schema": 1, "database": "x.db" }),
        );
        name
    };
    // The large pool's computer backed up once, long ago; the new one four times since.
    let big = write("BIG-PC", 1);
    let fresh = [write("NEW-PC", 2), write("NEW-PC", 3), write("NEW-PC", 4), write("NEW-PC", 5)];
    let unreadable = export_file_name(POOL_EXPORT, date_utc(2026, 9, 1) + 12 * 3_600_000);
    std::fs::write(dir.join(&unreadable), "not a zip").unwrap();
    assert_eq!(prune_exports(&dir, POOL_EXPORT, 3, Some("NEW-PC")), 1);
    let left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(left.contains(&big), "the other computer's backup stays");
    assert!(!left.contains(&fresh[0]) && fresh[1..].iter().all(|f| left.contains(f)), "this computer's newest three stay");
    assert!(left.contains(&unreadable), "a file it can't read is left alone");
}

#[test]
fn a_0_18_0_player_data_set_export_says_it_can_no_longer_be_imported() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("old-dataset.zip");
    zip_with(&file, json!({ "format": "irelia-companion-export", "version": 1, "kind": "dataset", "name": "Agurin-EUW_dataset", "schema": 1, "database": "x.db" }));
    let e = inspect_export(&file).unwrap_err().to_string();
    assert!(e.contains("no longer imports"), "{e}");
    assert!(inspect_export(&root.path().join("nothing.zip")).is_err());
}
