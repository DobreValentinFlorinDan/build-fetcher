//! Port of server/storage.test.ts.

use irelia::db::{Db, MatchRow};
use irelia::riot::disk_cache::{cache_store, close_cache_stores, legacy_file, CacheStore, DiskCache};
use irelia::storage::*;
use irelia::util::now_ms;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

fn set_mtime(p: &Path, t: SystemTime) {
    filetime::set_file_mtime(p, filetime::FileTime::from_system_time(t)).unwrap();
}

fn kinds(parts: &Value) -> Vec<String> {
    let mut v: Vec<String> = parts.as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap().to_string()).filter(|n| n != "index and free space").collect();
    v.sort();
    v
}

fn setup(root: &Path) -> (PathBuf, SystemTime) {
    let from = root.join("data");
    let old = SystemTime::now() - Duration::from_secs(30 * 24 * 3600);
    let db = Db::open_default(&from.join("irelia.db")).unwrap();
    db.upsert_player("puuid-1", "KR", "CHALLENGER", 1200).unwrap();
    db.insert_match(&MatchRow { id: "KR_1".into(), platform: "KR".into(), patch: "16.19".into(), version: "16.19.1".into(), queue: 420, creation: 0, duration: 1800, has_irelia: false }).unwrap();
    drop(db);
    let mut cache = DiskCache::new(&from.join("cache"));
    cache.set("match", "KR", "KR_1", &json!({"info": {"gameVersion": "16.19.1"}})).unwrap();
    cache.close();
    let store = CacheStore::open_default(&from.join("cache")).unwrap();
    store.write("league", "KR", "CHALLENGER", br#"{"entries":[]}"#, now_ms() - 30 * 24 * 3600_000).unwrap();
    drop(store);
    let legacy = legacy_file(&from.join("cache"), "mastery", "KR", "p1");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(&legacy, irelia::db::gzip(b"[]")).unwrap();
    set_mtime(&legacy, old);
    std::fs::create_dir_all(from.join("ddragon").join("16.19.1")).unwrap();
    std::fs::write(from.join("ddragon").join("16.19.1").join("item.json"), r#"{"data":{}}"#).unwrap();
    std::fs::write(from.join("notes.txt"), "not ours").unwrap();
    (from, old)
}

#[test]
fn storage_suite() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (from, old) = setup(&root);

    // stats split the folder into database, Riot cache, Data Dragon and other files
    let s = storage_stats(&from, &StatsOptions { default_data_dir: from.clone(), config_file: "x".into(), locked_by: None, ddragon_version: "16.19.1".into(), busy: false }).unwrap();
    let cat = |k: &str| s["categories"].as_array().unwrap().iter().find(|c| c["key"] == k).unwrap().clone();
    assert!(cat("database")["bytes"].as_u64().unwrap() > 0);
    assert_eq!(kinds(&cat("riotCache")["parts"]), vec!["league", "mastery", "match"]);
    assert_eq!(kinds(&cat("ddragon")["parts"]), vec!["16.19.1"]);
    assert_eq!(kinds(&cat("other")["parts"]), vec!["notes.txt"]);
    let sum: u64 = s["categories"].as_array().unwrap().iter().map(|c| c["bytes"].as_u64().unwrap()).sum();
    assert_eq!(s["totalBytes"].as_u64().unwrap(), sum);

    // pre-flight rejects relative, nested and parent folders
    let probs = |v: Value| v["problems"].as_array().unwrap().iter().map(|p| p.as_str().unwrap().to_string()).collect::<Vec<_>>().join(",");
    assert!(probs(inspect_target("relative\\dir", &from)).contains("absolute"));
    assert!(probs(inspect_target(from.to_str().unwrap(), &from)).contains("already"));
    assert!(probs(inspect_target(from.join("sub").to_str().unwrap(), &from)).contains("inside"));
    assert!(probs(inspect_target(root.to_str().unwrap(), &from)).contains("contain"));
    let ok = inspect_target(root.join("new-home").to_str().unwrap(), &from);
    assert_eq!(ok["problems"], json!([]));
    assert_eq!(ok["exists"], json!(false));
    assert_eq!(ok["hasData"], json!(false));
    assert!(ok["requiredBytes"].as_u64().unwrap() > 0);
    assert!(!root.join("new-home").exists());
    assert!(is_inside(&root.join("a").join("b"), &root.join("a")));
    assert!(!is_inside(&root.join("ab"), &root.join("a")));

    // copy + verify moves only the app files, keeps cache mtimes, then old copy is removed
    let to = root.join("D-drive").join("IreliaData");
    let db = Db::open_default(&from.join("irelia.db")).unwrap();
    let rows = db.row_counts().unwrap();
    let mut phases: Vec<String> = vec![];
    let copy = copy_data_dir(&from, &to, &|f| db.vacuum_into(f), &mut |p, _, _, _| phases.push(p.to_string())).unwrap();
    drop(db);
    close_cache_stores();
    verify_copy(&from, &to, &copy.files, Some(&rows), copy.cache_entries).unwrap();
    assert_eq!(copy.cache_entries, Some(2), "the cache database, as a database");
    let expect: Vec<(String, i64)> = [("players", 1), ("matches", 1), ("irelia_games", 0), ("participants", 0), ("timelines", 0), ("game_versions", 1), ("champion_patch_stats", 0), ("match_queue", 0), ("archive", 0)]
        .iter().map(|(a, b)| (a.to_string(), *b)).collect();
    assert_eq!(rows, expect);
    assert!(phases.contains(&"database".to_string()) && phases.contains(&"copying".to_string()));
    let copied = std::fs::metadata(legacy_file(&to.join("cache"), "mastery", "KR", "p1")).unwrap().modified().unwrap();
    let diff = copied.duration_since(old).unwrap_or_else(|e| e.duration()).as_secs_f64();
    assert!(diff < 0.001, "mtime differs by {diff}");
    assert!(!to.join("notes.txt").exists());
    assert_eq!(inspect_target(to.to_str().unwrap(), &from)["hasData"], json!(true));
    let moved = Db::open_default(&to.join("irelia.db")).unwrap();
    assert_eq!(moved.row_counts().unwrap(), rows);
    drop(moved);
    assert_eq!(remove_owned_data(&from).unwrap(), vec!["notes.txt"]);
    assert_eq!(std::fs::read_to_string(from.join("notes.txt")).unwrap(), "not ours");

    // verify fails loudly on a truncated copy
    let broken = root.join("broken");
    let db = Db::open_default(&to.join("irelia.db")).unwrap();
    let copy = copy_data_dir(&to, &broken, &|f| db.vacuum_into(f), &mut |_, _, _, _| {}).unwrap();
    drop(db);
    std::fs::write(legacy_file(&broken.join("cache"), "mastery", "KR", "p1"), "x").unwrap();
    let e = verify_copy(&to, &broken, &copy.files, None, None).unwrap_err();
    assert!(e.to_string().contains("Copy check failed"));

    // expired-only pruning keeps immutable matches and drops stale ladders
    let cache_root = to.join("cache");
    let (files, _) = DiskCache::prune(&cache_root, true, &mut |_| {}).unwrap();
    assert_eq!(files, 2, "the stale ladder and the stale mastery file");
    let usage: Vec<String> = { let mut v: Vec<String> = DiskCache::usage(&cache_root).unwrap().into_iter().map(|p| p.0).filter(|n| n != "index and free space").collect(); v.sort(); v };
    assert_eq!(usage, vec!["match"]);

    // old-patch cleanup deletes cached matches and timelines older than the window, and nothing it can't place
    let cache_root = root.join("old-patches").join("cache");
    let mut cache = DiskCache::new(&cache_root);
    let m = |v: &str| json!({"info": {"gameVersion": v}});
    cache.set("match", "asia", "KR_1", &m("16.10.1.1")).unwrap();
    cache.set("timeline", "asia", "KR_1", &json!({"frames": []})).unwrap();
    cache.set("match", "asia", "KR_2", &m("16.11.1.1")).unwrap();
    cache.set("match", "asia", "KR_3", &m("16.19.1.1")).unwrap();
    cache.set("timeline", "asia", "KR_4", &json!({"frames": []})).unwrap();
    cache.set("match", "asia", "KR_5", &m("16.19.1.1")).unwrap();
    let stored = [("KR_1", "16.10"), ("KR_5", "16.19")];
    let (files, _) = DiskCache::prune_matches(&cache_root, &mut |id, version| {
        let patch = stored.iter().find(|s| s.0 == id).map(|s| s.1.to_string())
            .or_else(|| version().map(|v| v.split('.').take(2).collect::<Vec<_>>().join(".")));
        match patch { Some(p) => p.split('.').nth(1).unwrap().parse::<i64>().unwrap() < 14, None => false }
    }).unwrap();
    assert_eq!(files, 3);
    let left = |kind: &str| { let mut v: Vec<String> = cache_store(&cache_root).unwrap().lock().match_entries().unwrap().into_iter().filter(|e| e.kind == kind).map(|e| e.key).collect(); v.sort(); v };
    assert_eq!(left("match"), vec!["KR_3", "KR_5"]);
    assert_eq!(left("timeline"), vec!["KR_4"]);
    cache.close();
    close_cache_stores();
}
