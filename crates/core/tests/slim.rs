//! Port of the core parts of server/slim.test.ts (the flows through the app are crates/server/tests/slim.rs): a Slim
//! run interrupted at each step comes back at the next start (recover_slim, before the app opens the database), one
//! cut short itself is done again from the start, slimming on three threads writes the same bytes as on one, and an
//! archived game's raw timeline goes only when the archive holds its capture at today's version.

use flate2::read::GzDecoder;
use irelia::capture::CAPTURE_VERSION;
use irelia::db::Db;
use irelia::riot::disk_cache::{cache_db_file, close_cache_stores, CacheStore, DiskCache};
use irelia::slim;
use irelia::testkit::load_test_static_data;
use irelia::testkit_model::{ap_ad_games, store_games, synth_match, ApAd, SynthGame};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
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

fn keys(cache_dir: &Path) -> Vec<(String, String, String)> {
    let c = rusqlite::Connection::open(cache_db_file(cache_dir)).unwrap();
    let mut st = c.prepare("SELECT kind, scope, key FROM entries ORDER BY kind, key").unwrap();
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(|x| x.unwrap()).collect();
    rows
}

fn entries(cache_dir: &Path) -> BTreeMap<String, String> {
    let s = CacheStore::open_default(cache_dir).unwrap();
    keys(cache_dir).into_iter().map(|(k, sc, key)| (format!("{k}:{key}"), String::from_utf8(s.read(&k, &sc, &key).unwrap().unwrap().1).unwrap())).collect()
}

/// The stored blobs as they are (compressed), to compare what different runs wrote byte for byte.
fn blobs(cache_dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let c = rusqlite::Connection::open(cache_db_file(cache_dir)).unwrap();
    let mut st = c.prepare("SELECT kind, key, data FROM entries").unwrap();
    let rows: Vec<(String, String, Vec<u8>)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(|x| x.unwrap()).collect();
    rows.into_iter().map(|(k, key, d)| (format!("{k}:{key}"), d)).collect()
}

fn archive_rows(db: &Db) -> BTreeMap<String, String> {
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

fn gzip(s: &str) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(s.as_bytes()).unwrap();
    e.finish().unwrap()
}

fn wins(db: &Db) -> Vec<(String, i64)> {
    let mut st = db.conn.prepare("SELECT match_id, win FROM irelia_games ORDER BY match_id").unwrap();
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|x| x.unwrap()).collect();
    rows
}

#[test]
fn slim_recovery_and_the_archive_rule() {
    let root = tempfile::tempdir().unwrap();
    std::env::set_var("IRELIA_CONFIG_DIR", root.path().join("config"));
    let dir = root.path().join("data");
    let cache_dir = dir.join("cache");
    std::fs::create_dir_all(&dir).unwrap();
    let data = load_test_static_data();
    let file = dir.join("irelia.db");
    let mut db = Db::open_default(&file).unwrap();
    let today = ap_ad_games(120, 30, 7, ApAd::default());
    store_games(&db, &today, Some(&data.patch));
    cache_them(&cache_dir, &today);
    let old = renumber(ap_ad_games(12, 6, 11, ApAd::default()), "12");
    store_games(&db, &old, Some("16.12"));
    cache_them(&cache_dir, &old);
    store_games(&db, &renumber(ap_ad_games(12, 6, 13, ApAd::default()), "10"), Some("16.10"));
    assert!(db.archive_batch(&[data.patch.clone(), data.previous_patch.clone(), "16.12".into()], 200).unwrap() > 0);
    let original_entries = entries(&cache_dir);
    let original_archive = archive_rows(&db);
    let before = wins(&db);
    let no_tick = |_: &str, _: &str, _: Option<f64>, _: Option<Value>| {};

    // ---- a run interrupted at each step comes back at the next start
    let mut slimmed: BTreeMap<&str, BTreeMap<String, Vec<u8>>> = BTreeMap::new();
    for stop in ["backup", "baseline", "apply", "verify"] {
        {
            let s = CacheStore::open_default(&cache_dir).unwrap();
            let plan = slim::plan_slim(&db, &s, &data).unwrap();
            let now = irelia::util::now_ms();
            let mut m = json!({ "version": 1, "startedAt": now, "updatedAt": now, "step": "backup", "plan": plan.plan, "db": null, "cache": null });
            slim::write_manifest(&dir, &mut m, Some(&db)).unwrap();
            slim::backup(&dir, &db, &s, &plan, &mut m, &no_tick).unwrap();
            if stop != "backup" {
                m["step"] = json!("baseline");
                slim::write_manifest(&dir, &mut m, Some(&db)).unwrap();
            }
            if stop == "apply" || stop == "verify" {
                m["step"] = json!("apply");
                slim::write_manifest(&dir, &mut m, Some(&db)).unwrap();
                slim::apply(&dir, &db, &s, &mut m, &no_tick, if stop == "apply" { 1 } else { 3 }, &|| false).unwrap();
                slimmed.insert(stop, blobs(&cache_dir));
                // A crash in the middle: the database part-way through as well.
                db.conn.execute_batch("UPDATE irelia_games SET win = 1 - win WHERE rowid = 1").unwrap();
                if stop == "verify" {
                    m["step"] = json!("verify");
                    slim::write_manifest(&dir, &mut m, Some(&db)).unwrap();
                }
            }
        }
        // The app closes (or crashes) here; the next start runs recovery before it opens the database.
        drop(db);
        close_cache_stores();
        if stop == "apply" {
            // 0.14.3: a recovery cut short itself (the app closed again) is done again from the start at the next one.
            let cut = std::panic::catch_unwind(|| {
                slim::recover_slim(&dir, &cache_dir, &|t| {
                    if t.starts_with("putting the cache entries back: ") && !t.ends_with(": 0 of 0") {
                        panic!("closed");
                    }
                })
            });
            assert!(cut.is_err(), "cut short");
            close_cache_stores();
        }
        let said = std::sync::Mutex::new(Vec::<String>::new());
        let r = slim::recover_slim(&dir, &cache_dir, &|t| said.lock().unwrap().push(t.to_string())).unwrap();
        let said = said.into_inner().unwrap();
        if stop == "apply" || stop == "verify" {
            let first = &said[0];
            assert!(first.starts_with("copying the database's backup (") && first.ends_with(" MB)"), "{first}");
            let last = said.last().unwrap();
            let (done, total) = last.trim_start_matches("putting the cache entries back: ").split_once(" of ").unwrap();
            assert_eq!(done, total, "{}", said.join(" | "));
        }
        db = Db::open_default(&file).unwrap();
        let step = r.map(|m| m["step"].clone()).unwrap_or(Value::Null);
        assert_eq!(step, json!(if stop == "backup" || stop == "baseline" { "cancelled" } else { "rolled-back" }), "interrupted at {stop}");
        assert!(!slim::slim_dir(&dir).exists(), "{stop}: no backup left");
        assert_eq!(entries(&cache_dir), original_entries, "{stop}: the cache as it was");
        assert_eq!(archive_rows(&db), original_archive, "{stop}: the archive as it was");
        assert_eq!(wins(&db), before, "{stop}: the games as they were");
    }
    assert!(!slimmed["apply"].is_empty());
    assert_eq!(slimmed["verify"], slimmed["apply"], "three threads write the same bytes as one");

    // ---- (a): an archived game's raw timeline goes only when the archive holds its capture at today's version
    let id = original_archive.keys().next().unwrap().clone();
    {
        let mut c = DiskCache::new(&cache_dir);
        c.set("timeline", "asia", &id, &json!({ "metadata": { "matchId": id }, "info": { "frames": [] } })).unwrap();
        c.close();
    }
    let plan = |db: &Db| {
        let s = CacheStore::open_default(&cache_dir).unwrap();
        slim::plan_slim(db, &s, &data).unwrap()
    };
    assert!(plan(&db).timeline_keys.iter().any(|k| k.key == id), "captured at today's version: its raw timeline can go");
    let mut rows: Value = serde_json::from_str(&original_archive[&id]).unwrap();
    rows["matches"][0]["detail_version"] = json!(CAPTURE_VERSION - 1);
    db.conn.execute("UPDATE archive SET data = ? WHERE match_id = ?", rusqlite::params![gzip(&rows.to_string()), id]).unwrap();
    let older = plan(&db);
    assert!(!older.timeline_keys.iter().any(|k| k.key == id), "an older capture: kept, a later version may capture it again");
    assert!(older.slim_keys.iter().any(|k| k.kind == "timeline" && k.key == id));
}
