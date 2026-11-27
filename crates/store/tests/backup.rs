//! Export and import end to end: a store with real (synthetic) games and learner checkpoints is exported, staged on
//! another "PC" (another folder), applied at its next start, and reopened with the same data. Archives that are not
//! Build Fetcher's, from a newer version, damaged or trying to write outside the staging folder are refused.

use bf_core::build::{DataSource, RankBand};
use bf_core::testkit::{load_test_static_data, Synth, SynthOptions};
use bf_core::zip::{read_entry, read_zip, ZipWriter};
use bf_store::backup::{apply_pending_import, cancel_import, export, pending_import, stage_import, Manifest, APP, BEFORE_DIR, FORMAT, PENDING_DIR};
use bf_store::{Ingest, Store, StoreOptions};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const KEY_A: &str = "RGAPI-00000000-aaaa";
const KEY_B: &str = "RGAPI-00000000-bbbb";

/// One PC's folders: settings.json, data/ (database, checkpoints).
struct Pc {
    _dir: tempfile::TempDir,
    config: PathBuf,
    data: PathBuf,
    settings: PathBuf,
}

fn pc(settings: Value) -> Pc {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("build-fetcher");
    let data = config.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let settings_file = config.join("settings.json");
    std::fs::write(&settings_file, serde_json::to_vec_pretty(&settings).unwrap()).unwrap();
    Pc { _dir: dir, config, data, settings: settings_file }
}

fn open(data: &Path) -> Store {
    Store::open(data, load_test_static_data(), StoreOptions::default()).unwrap()
}

/// A store with 300 synthetic games and a few checkpoint files (one half-written).
fn filled(p: &Pc) -> Store {
    let store = open(&p.data);
    let s = Synth::new(SynthOptions { timeline_share: 0.5, ..Default::default() }, load_test_static_data());
    let games: Vec<_> = (0..300).map(|i| s.game(i)).collect();
    let items: Vec<Ingest> = games.iter().map(|g| Ingest { m: &g.m, timeline: g.timeline.as_ref(), rank: RankBand::MasterPlus, me: None }).collect();
    store.ingest(&items).unwrap();
    let ck = p.data.join("checkpoints");
    std::fs::create_dir_all(&ck).unwrap();
    std::fs::write(ck.join("manifest.json"), br#"{"active":"v000001"}"#).unwrap();
    std::fs::write(ck.join("v000001.logistic.bin"), vec![7u8; 5000]).unwrap();
    std::fs::write(ck.join("learning.log"), b"v000001 accepted\n").unwrap();
    std::fs::write(ck.join("manifest.json.123.tmp"), b"half").unwrap();
    store
}

fn summary(store: &Store) -> (usize, usize, u64) {
    let records = store.records_after(0, 100_000).unwrap().len();
    let validation = store.validation_records().unwrap().len();
    let games: u64 = store.volumes(&[]).unwrap().iter().map(|v| v.games as u64).sum();
    (records, validation, games)
}

fn settings_of(p: &Pc) -> Value {
    serde_json::from_slice(&std::fs::read(&p.settings).unwrap()).unwrap()
}

#[test]
fn export_and_import_move_everything_but_the_key_by_default() {
    let a = pc(json!({ "riotApiKey": KEY_A, "regions": ["EUW1"], "tierA": [39], "dataDir": "D:/A-data", "firstRunComplete": true }));
    let store = filled(&a);
    let before = summary(&store);
    let zip = a.config.join("backup.zip");
    let rep = export(&store, &a.settings, &a.data.join("checkpoints"), &zip, false).unwrap();
    assert!(!rep.includes_key && rep.bytes > 0);

    // The archive: no key, no local data folder, no half-written checkpoint, the database and the checkpoints.
    let entries = read_zip(&zip).unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"data/build-fetcher.db") && names.contains(&"data/checkpoints/v000001.logistic.bin"), "{names:?}");
    assert!(!names.iter().any(|n| n.ends_with(".tmp")), "{names:?}");
    let s: Value = serde_json::from_slice(&read_entry(&zip, entries.iter().find(|e| e.name == "settings.json").unwrap()).unwrap()).unwrap();
    assert!(s.get("riotApiKey").is_none() && s.get("dataDir").is_none(), "{s}");
    assert_eq!(s["regions"], json!(["EUW1"]));
    let raw = std::fs::read(&zip).unwrap();
    assert!(!raw.windows(KEY_A.len()).any(|w| w == KEY_A.as_bytes()), "the key is somewhere in the archive");

    // Another PC with its own key, data folder and an older database.
    let b = pc(json!({ "riotApiKey": KEY_B, "regions": ["KR"], "dataDir": "E:/B-data" }));
    drop(open(&b.data));
    let staged = stage_import(&zip, &b.config).unwrap();
    assert!(!staged.includes_key && staged.checkpoint_files == 3, "{staged:?}");
    assert_eq!(pending_import(&b.config), Some(staged.clone()));
    // Nothing in use changed yet.
    assert_eq!(settings_of(&b)["regions"], json!(["KR"]));

    // Next start.
    let applied = apply_pending_import(&b.config, &b.data, &b.settings).unwrap().unwrap();
    assert_eq!(applied.summary, staged);
    assert!(!b.config.join(PENDING_DIR).exists());
    assert!(b.config.join(BEFORE_DIR).join("build-fetcher.db").exists(), "the replaced data is kept aside");
    let s = settings_of(&b);
    assert_eq!(s["regions"], json!(["EUW1"]));
    assert_eq!(s["tierA"], json!([39]));
    assert_eq!(s["riotApiKey"], json!(KEY_B), "no key in the archive: this PC's key stays");
    assert_eq!(s["dataDir"], json!("E:/B-data"), "this PC's data folder stays");
    assert_eq!(std::fs::read(b.data.join("checkpoints").join("v000001.logistic.bin")).unwrap(), vec![7u8; 5000]);
    let reopened = open(&b.data);
    assert_eq!(summary(&reopened), before);
    // Applied once: a second start has nothing to do.
    assert!(apply_pending_import(&b.config, &b.data, &b.settings).unwrap().is_none());
}

#[test]
fn the_key_moves_only_when_asked() {
    let a = pc(json!({ "riotApiKey": KEY_A }));
    let store = filled(&a);
    let zip = a.config.join("with-key.zip");
    assert!(export(&store, &a.settings, &a.data.join("checkpoints"), &zip, true).unwrap().includes_key);
    let b = pc(json!({ "riotApiKey": KEY_B }));
    assert!(stage_import(&zip, &b.config).unwrap().includes_key);
    apply_pending_import(&b.config, &b.data, &b.settings).unwrap().unwrap();
    assert_eq!(settings_of(&b)["riotApiKey"], json!(KEY_A));
}

#[test]
fn extra_keys_never_leave_this_pc() {
    const EXTRA_A: &str = "RGAPI-11111111-aaaa";
    const EXTRA_B: &str = "RGAPI-22222222-bbbb";
    let a = pc(json!({ "riotApiKey": KEY_A, "riotExtraApiKeys": [EXTRA_A] }));
    let store = filled(&a);
    for include_key in [false, true] {
        let zip = a.config.join(format!("extra-{include_key}.zip"));
        export(&store, &a.settings, &a.data.join("checkpoints"), &zip, include_key).unwrap();
        let raw = std::fs::read(&zip).unwrap();
        assert!(!raw.windows(EXTRA_A.len()).any(|w| w == EXTRA_A.as_bytes()), "an extra key is in the archive (include_key {include_key})");
        // Importing on a PC with its own extra keys keeps them.
        let b = pc(json!({ "riotApiKey": KEY_B, "riotExtraApiKeys": [EXTRA_B] }));
        stage_import(&zip, &b.config).unwrap();
        apply_pending_import(&b.config, &b.data, &b.settings).unwrap().unwrap();
        assert_eq!(settings_of(&b)["riotExtraApiKeys"], json!([EXTRA_B]));
    }
}

#[test]
fn an_import_can_be_cancelled_before_the_restart() {
    let a = pc(json!({}));
    let store = filled(&a);
    let zip = a.config.join("b.zip");
    export(&store, &a.settings, &a.data.join("checkpoints"), &zip, false).unwrap();
    let b = pc(json!({ "regions": ["KR"] }));
    stage_import(&zip, &b.config).unwrap();
    cancel_import(&b.config).unwrap();
    assert!(pending_import(&b.config).is_none());
    assert!(apply_pending_import(&b.config, &b.data, &b.settings).unwrap().is_none());
    assert_eq!(settings_of(&b)["regions"], json!(["KR"]));
}

/// A zip with a manifest and the given extra entries.
fn crafted(path: &Path, manifest: Value, entries: &[(&str, &[u8])]) {
    let mut w = ZipWriter::new(path);
    w.add_buffer("manifest.json", &serde_json::to_vec(&manifest).unwrap()).unwrap();
    for (n, b) in entries {
        w.add_buffer(n, b).unwrap();
    }
    w.finish().unwrap();
}

fn manifest(app: &str, format: u32) -> Value {
    serde_json::to_value(Manifest { app: app.into(), format, version: "1.0.0".into(), created_ms: 0, includes_key: false, files: vec![] }).unwrap()
}

#[test]
fn foreign_newer_damaged_or_escaping_archives_are_refused_and_leave_nothing_staged() {
    let a = pc(json!({}));
    let store = filled(&a);
    let good_db = a.config.join("snap.db");
    store.snapshot_to(&good_db).unwrap();
    let db = std::fs::read(&good_db).unwrap();
    let b = pc(json!({}));
    let dir = tempfile::tempdir().unwrap();
    let cases: Vec<(&str, Box<dyn Fn(&Path)>, &str)> = vec![
        ("not a zip", Box::new(|p: &Path| std::fs::write(p, b"hello").unwrap()), "Not a readable zip"),
        ("another app", Box::new(|p: &Path| crafted(p, manifest("irelia-companion", 1), &[])), "not a Build Fetcher backup"),
        ("newer format", Box::new(|p: &Path| crafted(p, manifest(APP, FORMAT + 1), &[])), "newer Build Fetcher"),
        ("path escape", Box::new({ let db = db.clone(); move |p: &Path| crafted(p, manifest(APP, 1), &[("data/build-fetcher.db", &db), ("data/checkpoints/../../evil.exe", b"x")]) }), "unexpected entry"),
        ("unknown entry", Box::new({ let db = db.clone(); move |p: &Path| crafted(p, manifest(APP, 1), &[("data/build-fetcher.db", &db), ("logs/x.log", b"x")]) }), "unexpected entry"),
        ("no database", Box::new(|p: &Path| crafted(p, manifest(APP, 1), &[("settings.json", b"{}")])), "no database"),
        ("damaged database", Box::new(|p: &Path| crafted(p, manifest(APP, 1), &[("data/build-fetcher.db", &[0x42u8; 8192][..])])), "database"),
        ("bad settings", Box::new({ let db = db.clone(); move |p: &Path| crafted(p, manifest(APP, 1), &[("data/build-fetcher.db", &db), ("settings.json", b"[1,2]")]) }), "not a settings file"),
    ];
    for (what, make, expect) in cases {
        let zip = dir.path().join(format!("{}.zip", what.replace(' ', "-")));
        make(&zip);
        let err = stage_import(&zip, &b.config).expect_err(what).to_string();
        assert!(err.contains(expect), "{what}: {err}");
        assert!(pending_import(&b.config).is_none(), "{what}: something was staged");
        assert!(!b.config.join(format!("{PENDING_DIR}.partial")).exists(), "{what}: staging left behind");
        assert!(!dir.path().join("evil.exe").exists() && !b.config.join("evil.exe").exists());
    }
}
