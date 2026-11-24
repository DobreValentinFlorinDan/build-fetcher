//! The on-disk format: one SQLite file, `data/build-fetcher.db`.
//!
//! | table | what | size driver |
//! |---|---|---|
//! | `meta` | schema version, drop counters, small state | tiny |
//! | `matches` | kept `MatchRecord`s (tier A games, the timeline sample, the user's own games), zstd blobs | records |
//! | `options` | each distinct (kind, key) once; counts refer to it by id (rune page keys are ~45 bytes) | small |
//! | `counts` | tier B counts per patch × champion × role × rank × context × option | counts |
//! | `pairs` | games, wins and newest game per patch × champion × role × rank (volumes, data age, baselines) | small |
//! | `seen` | every match id processed (counted, kept or dropped): dedupe across players and regions | ~30 B/match |
//! | `validation` | the fixed-size held-out sample (never in `matches`, never in counts) | capped |
//! | `players` | the sourcing pool (puuid, platform, rank, when last checked) | pool |
//! | `player_champs` | who was seen playing a tier A or thin champion-role (targeted sourcing) | capped per pair |
//! | `codec_dicts` | the zstd dictionary records are packed with (`bf_core::codec`) | ~64 KB |
//!
//! Context columns use 0 for "any": `opponent` is the lane opponent's champion id, `profile` 1 physical, 2 magic,
//! 3 mixed. Patches are `major * 100 + minor`. Raw API answers are never stored.
//!
//! `auto_vacuum = INCREMENTAL` is set before the first table so clean-up can hand freed pages back to the file
//! system without a full VACUUM (which needs as much free disk again as the database).

use anyhow::{bail, Result};
use bf_core::build::DamageProfile;
use rusqlite::Connection;

pub const SCHEMA_VERSION: i64 = 1;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS matches (
  seq          INTEGER PRIMARY KEY AUTOINCREMENT,
  match_id     TEXT NOT NULL UNIQUE,
  patch        INTEGER NOT NULL,
  build        INTEGER NOT NULL,
  platform     INTEGER NOT NULL,
  rank         INTEGER NOT NULL,
  start_ms     INTEGER NOT NULL,
  has_timeline INTEGER NOT NULL,
  blob         BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS matches_patch ON matches(patch);
CREATE TABLE IF NOT EXISTS options (
  id   INTEGER PRIMARY KEY,
  kind INTEGER NOT NULL,
  key  TEXT NOT NULL,
  UNIQUE(kind, key)
);
CREATE TABLE IF NOT EXISTS counts (
  champion      INTEGER NOT NULL,
  role          INTEGER NOT NULL,
  patch         INTEGER NOT NULL,
  rank          INTEGER NOT NULL,
  opponent      INTEGER NOT NULL,
  profile       INTEGER NOT NULL,
  option        INTEGER NOT NULL,
  games         INTEGER NOT NULL,
  wins          INTEGER NOT NULL,
  expected_wins REAL NOT NULL,
  PRIMARY KEY (champion, role, patch, rank, opponent, profile, option)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS counts_patch ON counts(patch);
CREATE TABLE IF NOT EXISTS pairs (
  champion  INTEGER NOT NULL,
  role      INTEGER NOT NULL,
  patch     INTEGER NOT NULL,
  rank      INTEGER NOT NULL,
  games     INTEGER NOT NULL,
  wins      INTEGER NOT NULL,
  newest_ms INTEGER NOT NULL,
  PRIMARY KEY (champion, role, patch, rank)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS seen (
  match_id TEXT PRIMARY KEY,
  patch    INTEGER NOT NULL,
  outcome  INTEGER NOT NULL           -- 0 counted, 1 counted and kept, 2 validation, 3 dropped
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS validation (
  match_id TEXT PRIMARY KEY,
  patch    INTEGER NOT NULL,
  start_ms INTEGER NOT NULL,
  blob     BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS players (
  puuid           TEXT PRIMARY KEY,
  platform        INTEGER NOT NULL,
  rank            INTEGER NOT NULL,
  lp              INTEGER NOT NULL DEFAULT 0,
  last_checked_ms INTEGER NOT NULL DEFAULT 0,
  newest_game_ms  INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS players_due ON players(platform, rank, last_checked_ms);
CREATE TABLE IF NOT EXISTS player_champs (
  champion INTEGER NOT NULL,
  role     INTEGER NOT NULL,
  puuid    TEXT NOT NULL,
  platform INTEGER NOT NULL,
  rank     INTEGER NOT NULL,
  games    INTEGER NOT NULL,
  last_ms  INTEGER NOT NULL,
  PRIMARY KEY (champion, role, puuid)
) WITHOUT ROWID;
";

/// Opens (creating when new) with the pragmas the store relies on. Refuses a database from a newer schema.
pub fn open(path: &std::path::Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let db = Connection::open(path)?;
    db.busy_timeout(std::time::Duration::from_secs(10))?;
    // Must come before any table exists to take effect on a new file.
    db.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;
    db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = OFF;")?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        bail!("This database was written by a newer Build Fetcher (schema {version}; this version reads {SCHEMA_VERSION})");
    }
    db.execute_batch(SCHEMA)?;
    db.execute_batch(bf_core::codec::CODEC_SCHEMA)?;
    if version < SCHEMA_VERSION {
        db.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
        db.execute("INSERT OR REPLACE INTO meta(key, value) VALUES('schema', ?)", [SCHEMA_VERSION.to_string()])?;
    }
    Ok(db)
}

pub fn profile_code(p: Option<DamageProfile>) -> i64 {
    match p {
        None => 0,
        Some(DamageProfile::Physical) => 1,
        Some(DamageProfile::Magic) => 2,
        Some(DamageProfile::Mixed) => 3,
    }
}
pub fn profile_from(c: i64) -> Option<DamageProfile> {
    match c {
        1 => Some(DamageProfile::Physical),
        2 => Some(DamageProfile::Magic),
        3 => Some(DamageProfile::Mixed),
        _ => None,
    }
}

pub fn meta_get(db: &Connection, key: &str) -> Option<String> {
    db.query_row("SELECT value FROM meta WHERE key = ?", [key], |r| r.get(0)).ok()
}
pub fn meta_set(db: &Connection, key: &str, value: &str) -> Result<()> {
    db.execute("INSERT INTO meta(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value", [key, value])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn opens_new_and_refuses_newer() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("data").join("build-fetcher.db");
        let db = super::open(&p).unwrap();
        let av: i64 = db.query_row("PRAGMA auto_vacuum", [], |r| r.get(0)).unwrap();
        assert_eq!(av, 2, "incremental");
        assert_eq!(super::meta_get(&db, "schema").as_deref(), Some("1"));
        db.execute_batch("PRAGMA user_version = 99").unwrap();
        drop(db);
        assert!(super::open(&p).unwrap_err().to_string().contains("newer"));
    }
}
