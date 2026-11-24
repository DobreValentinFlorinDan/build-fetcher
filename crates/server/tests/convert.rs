//! Port of server/convert.test.ts: the background conversion (0.14.0). A data folder of an earlier version (one gzip
//! file per API response, plain timelines) comes out with every response in riot-cache.db and every timeline
//! compressed, each one reading back exactly as it was, the old copies gone; champ select stops it and it carries on
//! after. Also: a new timeline is stored compressed and archived as JSON; the cache's size index is built once.

mod common;

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use irelia::capture::capture_timeline;
use irelia::db::{Db, MatchRow, OpenOptions};
use irelia::riot::disk_cache::{legacy_file, CacheStore, DiskCache};
use irelia::testkit::{make_match, make_timeline, MatchSpec, TimelineSpec, IRELIA};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};

const CHAMPS: [i64; 10] = [IRELIA, 64, 103, 222, 412, 122, 121, 134, 21, 89];

fn gzip(s: &str) -> Vec<u8> {
    let mut e = GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(s.as_bytes()).unwrap();
    e.finish().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conversion() {
    let now = irelia::util::now_ms();
    let files: std::sync::Mutex<Vec<(String, String, String, i64)>> = std::sync::Mutex::new(vec![]);
    let app = common::start(|db, data_dir| {
        // An earlier version's folder: gzip files with their times, and timelines stored as plain JSON.
        let cache_dir = data_dir.join("cache");
        for i in 0..30 {
            let id = format!("KR_{}", 9000 + i);
            let m = make_match(&MatchSpec { champions: Some(CHAMPS.to_vec()), ..MatchSpec::id(&id) });
            let tl = make_timeline(&m, &TimelineSpec::default());
            for (kind, value) in [("match", &m), ("timeline", &tl)] {
                let file = legacy_file(&cache_dir, kind, "asia", &id);
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                let text = irelia::js::stringify(value);
                std::fs::write(&file, gzip(&text)).unwrap();
                let mtime = now - i * 60_000;
                let t = filetime::FileTime::from_unix_time(mtime.div_euclid(1000), (mtime.rem_euclid(1000) * 1_000_000) as u32);
                filetime::set_file_mtime(&file, t).unwrap();
                files.lock().unwrap().push((kind.to_string(), id.clone(), text, mtime));
            }
            db.insert_match(&MatchRow {
                id: id.clone(),
                platform: "KR".into(),
                patch: "16.19".into(),
                version: m["info"]["gameVersion"].as_str().unwrap().into(),
                queue: 420,
                creation: m["info"]["gameCreation"].as_i64().unwrap(),
                duration: m["info"]["gameDuration"].as_i64().unwrap(),
                has_irelia: true,
            })
            .unwrap();
            // As 0.13.0 stored it: the capture in `data`.
            let cap = capture_timeline(&tl);
            db.conn.execute("INSERT INTO timelines(match_id, frame_interval, data) VALUES(?, ?, ?)", rusqlite::params![id, cap.interval as i64, cap.to_json_string()]).unwrap();
        }
        let league = legacy_file(&cache_dir, "league", "KR", "CHALLENGER");
        std::fs::create_dir_all(league.parent().unwrap()).unwrap();
        std::fs::write(&league, gzip(r#"{"entries":[1,2,3]}"#)).unwrap();
    })
    .await;
    let dir = app.data_dir.clone();
    let cache_dir = dir.join("cache");
    let files = files.into_inner().unwrap();
    let open = || Db::open(&dir.join("irelia.db"), OpenOptions { migrate: false, busy_timeout_ms: 5000 }).unwrap();
    let plain: HashMap<String, String> = {
        let db = open();
        let mut st = db.conn.prepare("SELECT match_id, data FROM timelines").unwrap();
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|x| x.unwrap()).collect();
        rows
    };

    // ---- every API response and timeline re-encoded, read back the same, old copies gone; champ select stops it
    let parts = irelia_server::extra::parts_for_tests(&app.app).unwrap();
    let conv = &parts.conversion;
    assert!(conv.needed());
    conv.check();
    let status = || conv.status.lock().clone().unwrap_or(Value::Null);
    let until = |f: &dyn Fn(&Value) -> bool| {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let s = status();
            if !s.is_null() && f(&s) {
                return s;
            }
            assert!(std::time::Instant::now() < end, "timed out: {s}");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };
    // Champ select as soon as it has begun: it stops, and says it is paused.
    until(&|s| s["cache"]["done"].as_f64().unwrap_or(0.0) > 0.0 || !s["timelines"].is_null() || s["running"] == json!(false));
    conv.set_busy(true);
    assert_eq!(status()["paused"], json!(true), "{}", status());
    conv.set_busy(false);
    let done = until(&|s| s["running"] == json!(false) && (s["done"] == json!(true) || !s["error"].is_null()));
    assert_eq!(done["error"], Value::Null, "{done}");
    assert_eq!(done["done"], json!(true));
    assert!(!conv.needed());

    // The old files and their folders are gone; each response reads back as it was, with its file's time.
    for k in ["match", "timeline", "league"] {
        assert!(!cache_dir.join(k).exists(), "{k} folder");
    }
    {
        let store = CacheStore::open_default(&cache_dir).unwrap();
        for (kind, id, text, mtime) in &files {
            let (stored_at, raw) = store.read(kind, "asia", id).unwrap().unwrap_or_else(|| panic!("{kind}:{id}"));
            assert_eq!(String::from_utf8(raw).unwrap(), *text, "{kind}:{id}");
            assert_eq!(stored_at, *mtime, "{kind}:{id}: its file's time");
        }
        assert_eq!(store.dicts.current().map(|d| d.id), Some(1), "compressed with a dictionary made from a match and a timeline");
    }
    let mut cache = DiskCache::new(&cache_dir);
    assert_eq!(cache.get("league", "KR", "CHALLENGER", None), Some(json!({ "entries": [1, 2, 3] })));
    cache.close();

    // Timelines: compressed, the plain copy empty, and the same JSON through timelines_of.
    let db = open();
    let rows: Vec<(String, String, Option<Vec<u8>>)> = {
        let mut st = db.conn.prepare("SELECT match_id, data, data_z FROM timelines").unwrap();
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(|x| x.unwrap()).collect();
        rows
    };
    assert!(rows.iter().all(|r| r.1.is_empty() && r.2.is_some()), "every timeline compressed");
    let ids: Vec<String> = plain.keys().cloned().collect();
    assert_eq!(db.timelines_of(&ids).unwrap(), plain);
    let z: usize = rows.iter().map(|r| r.2.as_ref().unwrap().len()).sum();
    let p: usize = plain.values().map(|d| d.len()).sum();
    assert!(z < p / 4, "compressed {z} of {p} bytes");

    // ---- a new timeline is stored compressed, and the archive keeps it as JSON
    let m = make_match(&MatchSpec { champions: Some(CHAMPS.to_vec()), ..MatchSpec::id("KR_9999") });
    db.insert_match(&MatchRow {
        id: "KR_9999".into(),
        platform: "KR".into(),
        patch: "16.10".into(),
        version: "16.10.1.1".into(),
        queue: 420,
        creation: m["info"]["gameCreation"].as_i64().unwrap(),
        duration: m["info"]["gameDuration"].as_i64().unwrap(),
        has_irelia: true,
    })
    .unwrap();
    let cap = capture_timeline(&make_timeline(&m, &TimelineSpec::default())).to_json_string();
    db.insert_timeline("KR_9999", 60_000, &cap).unwrap();
    let one = |db: &Db| -> (String, Option<Vec<u8>>) { db.conn.query_row("SELECT data, data_z FROM timelines WHERE match_id = 'KR_9999'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap() };
    assert_eq!(one(&db).0, "");
    assert_eq!(db.timelines_of(&["KR_9999".into()]).unwrap()["KR_9999"], cap);
    assert_eq!(db.archive_batch(&["16.19".into()], 200).unwrap(), 1);
    let blob: Vec<u8> = db.conn.query_row("SELECT data FROM archive WHERE match_id = 'KR_9999'", [], |r| r.get(0)).unwrap();
    let mut s = String::new();
    GzDecoder::new(&blob[..]).read_to_string(&mut s).unwrap();
    let archived: Value = serde_json::from_str(&s).unwrap();
    assert_eq!(archived["timelines"][0]["data"], json!(cap), "archived as JSON");
    assert_eq!(db.restore_batch(None, 200).unwrap(), 1);
    assert_eq!(db.timelines_of(&["KR_9999".into()]).unwrap()["KR_9999"], cap, "restored compressed, the same");
    let back = one(&db);
    assert_eq!(back.0, "");
    assert!(back.1.is_some());

    // ---- cache size index (0.14.1): built once; the sizes and the keys read from it, the same as before
    let store = CacheStore::open_default(&cache_dir).unwrap();
    let usage = "SELECT kind, COUNT(*) AS files, SUM(LENGTH(data)) AS bytes FROM entries GROUP BY kind";
    let plan = || -> String {
        let mut st = store.db.prepare(&format!("EXPLAIN QUERY PLAN {usage}")).unwrap();
        let rows: Vec<String> = st.query_map([], |r| r.get::<_, String>(3)).unwrap().map(|x| x.unwrap()).collect();
        rows.join(" ")
    };
    let version = || -> String { store.db.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0)).unwrap() };
    let sorted = |v: Vec<String>| {
        let mut v = v;
        v.sort();
        v
    };
    let keys = || sorted(store.match_entries().unwrap().iter().map(|e| format!("{e:?}")).collect());
    let was_built = plan().contains("entries_size");
    let before = (format!("{:?}", store.usage().unwrap()), keys());
    let built = store.ensure_size_index().unwrap();
    assert_eq!(built, !was_built, "built unless the conversion already did");
    assert!(!store.ensure_size_index().unwrap(), "once");
    assert!(plan().contains("COVERING INDEX entries_size"), "{}", plan());
    assert_eq!((format!("{:?}", store.usage().unwrap()), keys()), before);
    assert_eq!(version(), "2");
    drop(CacheStore::open_default(&cache_dir).unwrap());
    assert_eq!(version(), "2", "another opening of the file leaves the version as it is");
}
