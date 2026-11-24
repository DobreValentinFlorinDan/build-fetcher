//! Port of server/dbLocks.test.ts: "database is locked" while fetching (0.8.5). Another thread's writes must not make
//! fetching's writes fail: every write transaction takes the lock first (Db::tx, BEGIN IMMEDIATE) and waits for it,
//! and archive / restore batches leave the lock free long enough between batches (150 ms, as the app's old patches
//! do) for a waiting write to get in. A connection opened without migrating writes nothing on open.

use irelia::db::{Db, OpenOptions, QueuedMatch};
use irelia::model::run_eval::model_patches;
use irelia::testkit::load_test_static_data;
use irelia::testkit_model::{ap_ad_games, store_games, ApAd, SynthGame};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn games(n: usize, from: usize) -> Vec<SynthGame> {
    ap_ad_games(n, 20, from as u32, ApAd::default()).into_iter().enumerate().map(|(i, g)| SynthGame { id: format!("KR_{from}{i:06}"), ..g }).collect()
}

struct Seen {
    writes: usize,
    errors: Vec<String>,
    during: usize,
}

/// Runs `work` while a writer shaped like fetching writes every 20 ms on its own connection (30 s busy timeout).
fn while_writing(file: &Path, work: impl FnOnce()) -> Seen {
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let f: PathBuf = file.to_path_buf();
    let writer = std::thread::spawn(move || {
        let db = Db::open(&f, OpenOptions { migrate: false, busy_timeout_ms: 30_000 }).unwrap();
        let (mut n, mut at, mut errors) = (0usize, Vec::<Instant>::new(), Vec::<String>::new());
        while !s2.load(Ordering::SeqCst) {
            let id = format!("LOCK_{n}");
            n += 1;
            let t = Instant::now();
            let r = db
                .tx(|| {
                    if !db.has_match(&id) {
                        db.enqueue(&QueuedMatch { match_id: id.clone(), platform: "KR".into(), kind: "match".into(), puuid: "lock".into(), source: "otp".into(), score: 1.0 })?;
                    }
                    db.set_scanned("lock")
                })
                .and_then(|_| db.dequeue(&id));
            match r {
                Ok(()) => at.push(Instant::now()),
                Err(e) => errors.push(format!("{e} after {} ms", t.elapsed().as_millis())),
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        (at, errors)
    });
    std::thread::sleep(Duration::from_millis(200));
    let from = Instant::now();
    work();
    let to = Instant::now();
    std::thread::sleep(Duration::from_millis(200));
    stop.store(true, Ordering::SeqCst);
    let (at, errors) = writer.join().unwrap();
    Seen { writes: at.len(), errors, during: at.iter().filter(|t| **t >= from && **t <= to).count() }
}

#[test]
fn database_locks() {
    let root = tempfile::tempdir().unwrap();
    std::env::set_var("IRELIA_CONFIG_DIR", root.path().join("config"));
    let file = root.path().join("irelia.db");
    let data = load_test_static_data();
    let db = Db::open_default(&file).unwrap();
    store_games(&db, &games(40, 1), Some(&data.patch));
    store_games(&db, &games(1200, 2), Some("16.8")); // older than the six-patch window: six batches to archive

    // ---- fetching keeps writing while the archive switch archives and restores old patches
    let keep = model_patches(&db, &data, 6.0).unwrap();
    let seen = while_writing(&file, || {
        let mut moved = 0;
        loop {
            let n = db.archive_batch(&keep, 200).unwrap();
            if n == 0 {
                break;
            }
            moved += n;
            std::thread::sleep(Duration::from_millis(150));
        }
        assert_eq!(moved, 1200);
        assert_eq!(db.archive_summary().unwrap().iter().map(|a| (a.0.clone(), a.1)).collect::<Vec<_>>(), [("16.8".to_string(), 1200)]);
        while db.restore_batch(None, 200).unwrap() > 0 {
            std::thread::sleep(Duration::from_millis(150));
        }
        assert!(db.archive_summary().unwrap().is_empty());
    });
    assert_eq!(seen.errors, Vec::<String>::new(), "no \"database is locked\"");
    assert!(seen.during > 0, "the writer kept writing ({} of {} writes during it)", seen.during, seen.writes);

    // ---- fetching keeps writing while games of old patches are deleted
    let seen = while_writing(&file, || {
        assert_eq!(db.purge_stale(&[data.patch.clone(), data.previous_patch.clone()]).unwrap(), 1200);
    });
    assert_eq!(seen.errors, Vec::<String>::new());
    assert!(seen.writes > 0);

    // ---- the other threads write nothing when they open the database
    let holder = Db::open(&file, OpenOptions { migrate: false, busy_timeout_ms: 5000 }).unwrap();
    holder.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let t = Instant::now();
    let reader = Db::open(&file, OpenOptions { migrate: false, busy_timeout_ms: 2000 }).unwrap();
    assert!(t.elapsed() < Duration::from_millis(1000), "opened without waiting for the lock");
    assert!(!reader.data_version().is_empty());
    drop(reader);
    holder.conn.execute_batch("ROLLBACK").unwrap();
}
