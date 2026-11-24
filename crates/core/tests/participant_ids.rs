//! Port of server/participantIds.test.ts: participants by integer ids (normalize_participants, 0.14.0), rebuilt only
//! when they are most of the database and it comes out smaller; afterwards every reader reads the same rows through the
//! view, and inserting, archiving, restoring and deleting matches work as before.

use irelia::capture::participant_rows;
use irelia::db::{Db, MatchRow};
use irelia::testkit::{make_match, MatchSpec, IRELIA};
use rusqlite::types::ValueRef;
use serde_json::{json, Value};
use std::path::Path;

const CHAMPS: [i64; 10] = [IRELIA, 64, 103, 222, 412, 122, 121, 134, 21, 89];

fn match_row(id: &str, patch: &str, m: &Value) -> MatchRow {
    MatchRow {
        id: id.into(),
        platform: "KR".into(),
        patch: patch.into(),
        version: m["info"]["gameVersion"].as_str().unwrap().into(),
        queue: 420,
        creation: m["info"]["gameCreation"].as_i64().unwrap(),
        duration: m["info"]["gameDuration"].as_i64().unwrap(),
        has_irelia: false,
    }
}

/// `n` matches, their players drawn from `pool` long PUUIDs (high elo: the same players meet again and again).
fn fill(file: &Path, n: usize, pool: usize) -> Db {
    let db = Db::open_default(file).unwrap();
    let puuid = |k: usize| format!("{}{:08}", "x".repeat(70), k);
    db.tx(|| {
        for i in 0..n {
            let id = format!("KR_{}", 100_000 + i);
            let m = make_match(&MatchSpec {
                champions: Some(CHAMPS.to_vec()),
                puuids: Some((0..10).map(|k| puuid((i * 7 + k * 13) % pool)).collect()),
                ..MatchSpec::id(&id)
            });
            db.insert_match(&match_row(&id, if i % 2 == 1 { "16.19" } else { "16.10" }, &m))?;
            db.insert_participants(&id, &participant_rows(&m))?;
        }
        Ok(())
    })
    .unwrap();
    db
}

fn rows(db: &Db, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Vec<Value> {
    let mut st = db.conn.prepare(sql).unwrap();
    let names: Vec<String> = st.column_names().map(|s| s.to_string()).collect();
    let mut out = vec![];
    let mut q = st.query(params).unwrap();
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

fn read(db: &Db) -> Value {
    json!({
        "rows": rows(db, "SELECT * FROM participants ORDER BY match_id, participant_id", &[]),
        "totals": format!("{:?}", db.champion_totals(&["16.19".to_string()]).unwrap()),
        "model": rows(db, "SELECT match_id, participant_id, champion_id, team_id, position FROM participants WHERE match_id IN (SELECT match_id FROM matches WHERE patch = ?) ORDER BY 1, 2", &[&"16.19"]),
        "counts": format!("{:?}", db.row_counts().unwrap()),
    })
}

fn kind(db: &Db) -> String {
    db.conn.query_row("SELECT type FROM sqlite_master WHERE name = 'participants'", [], |r| r.get(0)).unwrap()
}

fn count(db: &Db, sql: &str) -> i64 {
    db.conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[test]
fn the_same_rows_for_every_reader_smaller_and_writes_the_archive_and_deletes_work_as_before() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("ids.db");
    let db = fill(&file, 300, 40);
    let before = read(&db);
    let r = db.normalize_participants().unwrap();
    assert_eq!(r["done"], json!(true), "{r}");
    assert!(r["after"].as_f64().unwrap() < r["before"].as_f64().unwrap(), "smaller: {r}");
    drop(db);
    let db = Db::open_default(&file).unwrap();
    assert_eq!(kind(&db), "view");
    assert_eq!(read(&db), before, "every reader reads the same");
    let layout: Value = serde_json::from_str(&db.get_meta("participants_layout").unwrap()).unwrap();
    assert_eq!(layout["done"], json!(true));

    // A new match's players, and the same match stored again (idempotent).
    let m = make_match(&MatchSpec { champions: Some(CHAMPS.to_vec()), ..MatchSpec::id("KR_999999") });
    db.insert_match(&match_row("KR_999999", "16.19", &m)).unwrap();
    db.insert_participants("KR_999999", &participant_rows(&m)).unwrap();
    db.insert_participants("KR_999999", &participant_rows(&m)).unwrap();
    let of = |id: &str| rows(&db, "SELECT * FROM participants WHERE match_id = ? ORDER BY participant_id", &[&id]);
    assert_eq!(of("KR_999999").len(), 10);
    let puuids: Vec<Value> = of("KR_999999").iter().map(|x| x["puuid"].clone()).collect();
    assert_eq!(Value::Array(puuids), m["metadata"]["participants"]);

    // Archived and restored: the same rows.
    let kept = of("KR_100000");
    assert!(db.archive_batch(&["16.19".to_string()], 200).unwrap() > 0);
    assert_eq!(of("KR_100000").len(), 0, "archived: out of the tables");
    while db.restore_batch(None, 200).unwrap() > 0 {}
    assert_eq!(of("KR_100000"), kept, "restored as they were");

    // Deleting a patch's matches takes their players with them (the trigger in place of ON DELETE CASCADE).
    db.purge_stale(&["16.19".to_string()]).unwrap();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM participants WHERE match_id IN ('KR_100000', 'KR_100002')"), 0);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM participant_rows"), count(&db, "SELECT COUNT(*) FROM participants"));
    assert_eq!(count(&db, "SELECT COUNT(*) FROM match_ids WHERE match_id NOT IN (SELECT match_id FROM matches)"), 0, "their ids go too");
    assert_eq!(db.normalize_participants().unwrap(), json!({ "done": false, "why": "already by ids" }));
}

#[test]
fn not_done_when_the_table_is_a_small_part_of_the_database_nor_when_it_would_not_be_smaller() {
    // Every PUUID different and short: by ids would only add an index.
    let root = tempfile::tempdir().unwrap();
    let db = fill(&root.path().join("unique.db"), 300, 3000);
    let before = read(&db);
    let r = db.normalize_participants().unwrap();
    assert_eq!(r["done"], json!(false));
    let why = r["why"].as_str().unwrap();
    assert!(why.contains("not be smaller") || why.contains("of the database"), "{why}");
    assert_eq!(kind(&db), "table");
    assert_eq!(read(&db), before);
}
