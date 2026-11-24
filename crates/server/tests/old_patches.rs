//! Port of server/oldPatches.test.ts, through the running app: the archive switch archives what is older than patch
//! memory's window and keeps doing so as patches leave it (not during champ select or a game), off restores everything;
//! the cache switch deletes cached files of old patches once per window; nothing runs while the data folder is busy.

mod common;

use irelia::db::{Db, OpenOptions};
use irelia::riot::disk_cache::{cache_db_file, DiskCache};
use irelia::testkit_model::{ap_ad_games, store_games, ApAd, SynthGame};
use serde_json::{json, Value};
use std::path::Path;

fn games(n: usize, from: usize) -> Vec<SynthGame> {
    ap_ad_games(n, 5, from as u32, ApAd::default()).into_iter().enumerate().map(|(i, g)| SynthGame { id: format!("KR_{}", from * 1000 + i), ..g }).collect()
}

fn patches_stored(data_dir: &Path) -> Vec<String> {
    let db = Db::open(&data_dir.join("irelia.db"), OpenOptions { migrate: false, busy_timeout_ms: 5000 }).unwrap();
    let mut st = db.conn.prepare("SELECT patch, COUNT(*) FROM matches GROUP BY patch ORDER BY patch").unwrap();
    let rows = st.query_map([], |r| Ok(format!("{}:{}", r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).unwrap().map(|x| x.unwrap()).collect();
    rows
}

fn archived(s: &Value) -> Vec<(String, i64)> {
    s["archived"].as_array().unwrap().iter().map(|a| (a["patch"].as_str().unwrap().to_string(), a["games"].as_i64().unwrap())).collect()
}

fn cache_keys(cache_dir: &Path) -> Vec<String> {
    let c = rusqlite::Connection::open(cache_db_file(cache_dir)).unwrap();
    let mut st = c.prepare("SELECT kind, key FROM entries WHERE kind IN ('match', 'timeline') ORDER BY kind, key").unwrap();
    let rows = st.query_map([], |r| Ok(format!("{}:{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?))).unwrap().map(|x| x.unwrap()).collect();
    rows
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_patches() {
    let app = common::start(|db, _| {
        store_games(db, &games(20, 1), Some("16.19"));
        store_games(db, &games(6, 2), Some("16.16")); // within the window (six patches): kept
        store_games(db, &games(4, 3), Some("16.10")); // older: archived
    })
    .await;
    let dir = app.data_dir.clone();
    let maintain = || irelia_server::extra::maintain_old_patches_now(&app.app);

    // ---- the archive switch archives what is older than the window, keeps doing so, and off restores it all
    let s = app.get("/api/old-patches").await;
    assert_eq!((s["archive"].clone(), s["trimCache"].clone()), (json!(false), json!(false)));
    maintain().await;
    assert_eq!(patches_stored(&dir), ["16.10:4", "16.16:6", "16.19:20"], "off: nothing happens by itself");

    let (_, on) = app.put("/api/old-patches", json!({ "archive": true })).await;
    assert_eq!(on["archive"], json!(true), "{on}");
    assert_eq!(archived(&on), [("16.10".to_string(), 4)]);
    assert_eq!(patches_stored(&dir), ["16.16:6", "16.19:20"]);

    // A game of an old patch arrives (or a patch leaves the window): the next check archives it, unless champ select
    // or a game is on (then the one after).
    {
        let db = Db::open(&dir.join("irelia.db"), OpenOptions { migrate: false, busy_timeout_ms: 5000 }).unwrap();
        store_games(&db, &games(1, 4), Some("16.09"));
    }
    app.app.inner.lock().matchup.phase = "game";
    maintain().await;
    assert_eq!(archived(&app.get("/api/old-patches").await), [("16.10".to_string(), 4)]);
    app.app.inner.lock().matchup.phase = "idle";
    maintain().await;
    assert_eq!(archived(&app.get("/api/old-patches").await), [("16.09".to_string(), 1), ("16.10".to_string(), 4)]);

    let (_, off) = app.put("/api/old-patches", json!({ "archive": false })).await;
    assert_eq!((off["archive"].clone(), off["archived"].as_array().unwrap().len(), off["error"].clone()), (json!(false), 0, Value::Null));
    assert_eq!(patches_stored(&dir), ["16.09:1", "16.10:4", "16.16:6", "16.19:20"]);

    // ---- the cache switch deletes cached files of old patches, once per window
    let cache_dir = dir.join("cache");
    {
        let mut c = DiskCache::new(&cache_dir);
        c.set("match", "KR", "KR_3000", &json!({ "info": { "gameVersion": "16.10.1.1" } })).unwrap(); // stored, old
        c.set("timeline", "KR", "KR_3000", &json!({ "frames": [] })).unwrap();
        c.set("match", "KR", "KR_1000", &json!({ "info": { "gameVersion": "16.19.1.1" } })).unwrap(); // stored, today's
        c.set("match", "KR", "KR_9999", &json!({ "info": { "gameVersion": "16.08.1.1" } })).unwrap(); // not stored, old by its version
        c.close();
    }
    let (_, on) = app.put("/api/old-patches", json!({ "trimCache": true })).await;
    assert_eq!(on["trimCache"], json!(true), "{on}");
    assert!(on["last"].as_str().unwrap_or("").contains("Deleted 3 cached files"), "{on}");
    assert_eq!(cache_keys(&cache_dir), ["match:KR_1000"]);
    // The window hasn't moved: the next check leaves the cache alone.
    {
        let mut c = DiskCache::new(&cache_dir);
        c.set("match", "KR", "KR_3001", &json!({ "info": { "gameVersion": "16.10.1.1" } })).unwrap();
        c.close();
    }
    maintain().await;
    assert_eq!(cache_keys(&cache_dir), ["match:KR_1000", "match:KR_3001"]);

    // ---- nothing runs while the data folder is moved or compacted
    irelia_server::extra::set_storage_busy(&app.app, true);
    let (_, s) = app.put("/api/old-patches", json!({ "archive": true })).await;
    assert!(s["error"].as_str().unwrap_or("").contains("being moved"), "{s}");
    // The request is turned away before anything runs (the API's guard while the folder moves): nothing archived.
    assert_eq!(patches_stored(&dir), ["16.09:1", "16.10:4", "16.16:6", "16.19:20"]);
    irelia_server::extra::set_storage_busy(&app.app, false);
    let (_, again) = app.put("/api/old-patches", json!({ "archive": true })).await;
    assert_eq!(again["error"], Value::Null, "{again}");
    assert!(!again["archived"].as_array().unwrap().is_empty());
}
