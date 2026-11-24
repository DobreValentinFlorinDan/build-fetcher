//! Port of server/slim.test.ts: Slim stored data (0.14.0) on a synthetic data folder, through the app as the Storage
//! manager drives it. A run whose results stay the same keeps its backup, and Roll back and Keep the slimmer data both
//! work; a difference rolls it back by itself; too little free space is refused; Stop ends a run and undoes it; a run
//! interrupted at any step comes back at the next start (slim::recover_slim, which the app runs before it opens the
//! database).

mod common;

use flate2::read::GzDecoder;
use irelia::db::{Db, OpenOptions};
use irelia::riot::disk_cache::{cache_db_file, CacheStore, DiskCache};
use irelia::slim;
use irelia::testkit::load_test_static_data;
use irelia::testkit_model::{ap_ad_games, store_games, synth_match, ApAd, SynthGame};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

fn renumber(games: Vec<SynthGame>, prefix: &str) -> Vec<SynthGame> {
    games.into_iter().enumerate().map(|(i, g)| SynthGame { id: format!("KR_{prefix}{i:04}"), ..g }).collect()
}

fn cache_them(cache_dir: &Path, games: &[SynthGame]) {
    let mut c = DiskCache::new(cache_dir);
    for g in games {
        let (m, t) = synth_match(g);
        c.set("match", "asia", &g.id, &m).unwrap();
        c.set("timeline", "asia", &g.id, &t).unwrap();
    }
    c.close();
}

/// Every cache entry, decoded.
fn entries(cache_dir: &Path) -> BTreeMap<String, String> {
    let s = CacheStore::open_default(cache_dir).unwrap();
    let keys: Vec<(String, String, String)> = {
        let c = rusqlite::Connection::open(cache_db_file(cache_dir)).unwrap();
        let mut st = c.prepare("SELECT kind, scope, key FROM entries").unwrap();
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(|x| x.unwrap()).collect();
        rows
    };
    keys.into_iter().map(|(k, sc, key)| (format!("{k}:{key}"), String::from_utf8(s.read(&k, &sc, &key).unwrap().unwrap().1).unwrap())).collect()
}

fn read_db(data_dir: &Path) -> Db {
    Db::open(&data_dir.join("irelia.db"), OpenOptions { migrate: false, busy_timeout_ms: 5000 }).unwrap()
}

fn archive_rows(data_dir: &Path) -> BTreeMap<String, String> {
    let db = read_db(data_dir);
    let mut st = db.conn.prepare("SELECT match_id, data FROM archive").unwrap();
    let rows: Vec<(String, Vec<u8>)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|x| x.unwrap()).collect();
    rows.into_iter()
        .map(|(id, data)| {
            let mut s = String::new();
            GzDecoder::new(&data[..]).read_to_string(&mut s).unwrap();
            (id, s)
        })
        .collect()
}

fn wins(data_dir: &Path) -> Vec<(String, i64)> {
    let db = read_db(data_dir);
    let mut st = db.conn.prepare("SELECT match_id, win FROM irelia_games ORDER BY match_id").unwrap();
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|x| x.unwrap()).collect();
    rows
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slim_stored_data() {
    let data = load_test_static_data();
    let patch = data.patch.clone();
    let previous = data.previous_patch.clone();
    let app = common::start(|db, data_dir| {
        let cache_dir = data_dir.join("cache");
        // Today's games (the model's, cached: slimmed by (b)); 16.12's (older than two patches: their raw timelines go,
        // (a)); 16.10's, archived (older than patch memory's window: (c)).
        let today = ap_ad_games(120, 30, 7, ApAd::default());
        store_games(db, &today, Some(&patch));
        cache_them(&cache_dir, &today);
        let old = renumber(ap_ad_games(12, 6, 11, ApAd::default()), "12");
        store_games(db, &old, Some("16.12"));
        cache_them(&cache_dir, &old);
        store_games(db, &renumber(ap_ad_games(12, 6, 13, ApAd::default()), "10"), Some("16.10"));
        assert!(db.archive_batch(&[patch.clone(), previous.clone(), "16.12".into()], 200).unwrap() > 0);
    })
    .await;
    let dir = app.data_dir.clone();
    let cache_dir = dir.join("cache");
    let original_entries = entries(&cache_dir);
    let original_archive = archive_rows(&dir);
    let before_wins = wins(&dir);
    let settled = |v: &Value| v["running"] == json!(false);
    let start = || app.post("/api/storage/slim", json!({ "action": "start" }));

    // ---- refused without the free space for its backup, and nothing changes
    std::env::set_var("IRELIA_SLIM_FREE_BYTES", "1000");
    let (code, _) = start().await;
    assert_eq!(code, 200);
    let s = app.until("/api/storage/slim", 60, settled).await;
    std::env::remove_var("IRELIA_SLIM_FREE_BYTES");
    assert_eq!(s["manifest"]["step"], json!("cancelled"), "{s}");
    let note = s["manifest"]["note"].as_str().unwrap_or("");
    assert!(note.starts_with("Not enough free space for the backup: ") && note.contains("needed"), "{note}");
    assert!(!slim::slim_dir(&dir).exists(), "no backup folder");
    assert_eq!(entries(&cache_dir), original_entries);

    // ---- a difference in the results rolls it back by itself, checked by SHA-256
    std::env::set_var("IRELIA_SLIM_TEST_DIFFERENCE", "1");
    start().await;
    let s = app.until("/api/storage/slim", 240, settled).await;
    std::env::remove_var("IRELIA_SLIM_TEST_DIFFERENCE");
    assert_eq!(s["manifest"]["step"], json!("rolled-back"), "{s}");
    assert!(s["manifest"]["differences"].as_array().map(|d| !d.is_empty()).unwrap_or(false), "what differed");
    let note = s["manifest"]["note"].as_str().unwrap_or("");
    assert!(note.starts_with("Rolled back by itself: ") && note.contains("SHA-256"), "{note}");
    assert_eq!(wins(&dir), before_wins, "the database as it was");
    assert_eq!(entries(&cache_dir), original_entries, "the cache as it was");
    assert_eq!(archive_rows(&dir), original_archive);
    assert!(!slim::slim_dir(&dir).exists());

    // ---- the same results keep the backup; Roll back puts everything back, Keep the slimmer data deletes the backup
    start().await;
    let s = app.until("/api/storage/slim", 240, settled).await;
    assert_eq!(s["manifest"]["step"], json!("verified"), "{s}");
    assert_eq!((s["progress"].clone(), s["count"].clone(), s["error"].clone()), (Value::Null, Value::Null, Value::Null));
    assert!(slim::slim_dir(&dir).exists(), "the backup is kept");
    assert!(s["manifest"]["backupBytes"].as_f64().unwrap_or(0.0) > 0.0);
    assert_eq!(s["manifest"]["plan"]["timelines"], json!(12), "(a) 16.12's raw timelines");
    assert!(s["manifest"]["applied"]["slimmed"].as_f64().unwrap_or(0.0) > 0.0, "(b) matches and timelines slimmed");
    assert_eq!(s["manifest"]["applied"]["archived"], json!(12), "(c) 16.10's archived matches");
    let slimmed = entries(&cache_dir);
    assert_eq!(slimmed.keys().filter(|k| k.starts_with("timeline:KR_12")).count(), 0, "(a) gone from the cache");
    let first_match = original_entries.keys().find(|k| k.starts_with("match:")).unwrap();
    let one: Value = serde_json::from_str(&slimmed[first_match]).unwrap();
    assert!(one["info"]["participants"][0]["challenges"].get("unrelatedCosmetic").is_none(), "(b) a field no capture reads is gone");
    let archived: Value = serde_json::from_str(archive_rows(&dir).values().next().unwrap()).unwrap();
    assert_eq!(archived["participants"].as_array().unwrap().iter().filter(|p| p["puuid"] == "").count(), 9, "(c) the other nine players cleared");
    // Another run waits for your choice.
    let (code, body) = start().await;
    assert_eq!(code, 409);
    assert!(body.to_string().contains("Roll back or Keep"), "{body}");

    // Roll back: the cache entries and the archive as they were.
    let (_, back) = app.post("/api/storage/slim", json!({ "action": "rollback" })).await;
    assert_eq!(back["manifest"]["step"], json!("rolled-back"), "{back}");
    assert_eq!(entries(&cache_dir), original_entries);
    assert_eq!(archive_rows(&dir), original_archive);
    assert!(!slim::slim_dir(&dir).exists());

    // ---- Stop ends a run at once and undoes it (0.15.0)
    start().await;
    let (_, s0) = app.post("/api/storage/slim", json!({ "action": "stop" })).await;
    assert_eq!(s0["stopping"], json!(true), "{s0}");
    assert!(s0["label"].as_str().unwrap_or("").starts_with("Stopping"), "{s0}");
    let s = app.until("/api/storage/slim", 240, settled).await;
    assert_eq!(s["manifest"]["step"], json!("cancelled"), "{s}");
    assert!(s["manifest"]["note"].as_str().unwrap_or("").contains("you stopped it"), "{s}");
    assert_eq!(s["stopping"], json!(false));
    assert!(!slim::slim_dir(&dir).exists(), "no backup left");
    assert_eq!(entries(&cache_dir), original_entries, "the cache as it was");
    let (code, body) = app.post("/api/storage/slim", json!({ "action": "stop" })).await;
    assert!(code == 409 && body.to_string().contains("not running"), "{code} {body}");

    // ---- again, then Keep the slimmer data: the backup goes, the slim data stays
    start().await;
    assert_eq!(app.until("/api/storage/slim", 240, settled).await["manifest"]["step"], json!("verified"));
    let (_, kept) = app.post("/api/storage/slim", json!({ "action": "keep" })).await;
    assert_eq!(kept["manifest"]["step"], json!("kept"), "{kept}");
    assert!(!slim::slim_dir(&dir).exists());
    assert_ne!(entries(&cache_dir), original_entries);
    let (code, body) = app.post("/api/storage/slim", json!({ "action": "keep" })).await;
    assert!(code == 409 && body.to_string().contains("no verified run"), "{code} {body}");
}
