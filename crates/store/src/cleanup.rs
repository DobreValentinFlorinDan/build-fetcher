//! Clean-up after each training cycle, and the storage numbers the Settings page shows.
//!
//! What stays: the counts (all patches, while the budget allows), the validation sample, match records inside the
//! learner's window, the user's own games, and the learner's checkpoints (its own folder: measured, never touched
//! here). What goes: records outside the window, then, while the data folder is over the disk budget, the oldest
//! patch's records, counts, volumes and dedupe ids, one patch at a time, never the newest patch.
//!
//! Space is handed back with `PRAGMA incremental_vacuum` (the file is `auto_vacuum = INCREMENTAL`), which needs no
//! spare disk, unlike `VACUUM`, which writes a whole copy. Sizes are measured from the files on disk.

use crate::record::{patch_code, patch_from};
use crate::store::Store;
use anyhow::Result;
use bf_core::build::{Patch, RankBand, StorageStatus};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Instant;

/// The learner's window: records on these patches are what it still trains on.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LearnWindow {
    /// Empty: the newest `DEFAULT_WINDOW_PATCHES` patches with data.
    pub patches: Vec<Patch>,
}

pub const DEFAULT_WINDOW_PATCHES: usize = 3;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupReport {
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub freed_bytes: u64,
    pub records_deleted: u64,
    pub count_rows_deleted: u64,
    /// Patches whose data went to stay under the budget, oldest first.
    pub patches_trimmed: Vec<Patch>,
    /// Still over the budget with only the newest patch left (or other parts, such as checkpoints, take it all).
    pub over_budget: bool,
    pub millis: u64,
}

/// Other folders counted in the budget and shown as parts (checkpoints, ddragon, log).
#[derive(Clone, Debug, Default)]
pub struct OtherParts {
    pub parts: Vec<(String, PathBuf)>,
}

impl OtherParts {
    /// The usual layout: data/checkpoints, data/ddragon, and the logs folder.
    pub fn standard(data_dir: &std::path::Path, logs_dir: Option<&std::path::Path>) -> OtherParts {
        let mut parts = vec![("checkpoints".to_string(), data_dir.join("checkpoints")), ("ddragon".to_string(), data_dir.join("ddragon"))];
        if let Some(l) = logs_dir {
            parts.push(("log".into(), l.to_path_buf()));
        }
        OtherParts { parts }
    }
    fn bytes(&self) -> Vec<(String, u64)> {
        self.parts.iter().map(|(n, p)| (n.clone(), dir_bytes(p))).collect()
    }
}

fn dir_bytes(p: &std::path::Path) -> u64 {
    if !p.exists() {
        return 0;
    }
    bf_core::util::walk_files(p).iter().map(|f| f.size).sum()
}

impl Store {
    /// The database's files on disk (main file, WAL and shared memory).
    pub fn db_bytes(&self) -> u64 {
        ["", "-wal", "-shm"].iter().map(|s| std::fs::metadata(format!("{}{s}", self.db_path.display())).map(|m| m.len()).unwrap_or(0)).sum()
    }

    fn used_bytes(&self, other: &OtherParts) -> u64 {
        self.db_bytes() + other.bytes().iter().map(|x| x.1).sum::<u64>()
    }

    /// Hands free pages back to the file system and empties the WAL.
    fn compact(db: &Connection) -> Result<()> {
        // The pragma frees pages as it is stepped: run it to the end (one step frees one page).
        let mut st = db.prepare("PRAGMA incremental_vacuum")?;
        let mut rows = st.query([])?;
        while rows.next()?.is_some() {}
        db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }

    pub fn cleanup(&self, window: &LearnWindow, budget_bytes: u64, other: &OtherParts) -> Result<CleanupReport> {
        let t0 = Instant::now();
        let mut rep = CleanupReport { before_bytes: self.used_bytes(other), ..Default::default() };
        let keep_n = self.opts.player_champs_per_pair as i64;
        let mine = crate::record::rank_code(Some(RankBand::Mine));
        self.with_writer(|db| {
            let all: Vec<i64> = patches_with_data(db)?;
            let mut window: Vec<i64> = window.patches.iter().map(|p| patch_code(*p)).collect();
            if window.is_empty() {
                window = all.iter().rev().take(DEFAULT_WINDOW_PATCHES).copied().collect();
            }
            let list = |v: &[i64]| if v.is_empty() { "-1".to_string() } else { v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",") };
            rep.records_deleted += db.execute(&format!("DELETE FROM matches WHERE patch NOT IN ({}) AND rank != ?", list(&window)), [mine])? as u64;
            // Targeted sourcing remembers only the most recent players per pair.
            db.execute(
                "DELETE FROM player_champs WHERE (champion, role, puuid) IN (
                   SELECT champion, role, puuid FROM (
                     SELECT champion, role, puuid, ROW_NUMBER() OVER (PARTITION BY champion, role ORDER BY last_ms DESC) AS rn FROM player_champs)
                   WHERE rn > ?)",
                [keep_n],
            )?;
            Self::compact(db)?;
            Ok(())
        })?;
        // Over the budget: the oldest patch goes, one at a time, never the newest.
        loop {
            if self.used_bytes(other) <= budget_bytes {
                break;
            }
            let trimmed = self.with_writer(|db| {
                let all = patches_with_data(db)?;
                if all.len() <= 1 {
                    return Ok(None);
                }
                let oldest = all[0];
                let tx = db.transaction()?;
                let r = tx.execute("DELETE FROM matches WHERE patch = ?", [oldest])? as u64;
                let c = tx.execute("DELETE FROM counts WHERE patch = ?", [oldest])? as u64;
                tx.execute("DELETE FROM pairs WHERE patch = ?", [oldest])?;
                tx.execute("DELETE FROM seen WHERE patch = ? AND outcome != 2", [oldest])?;
                tx.commit()?;
                Self::compact(db)?;
                Ok(Some((oldest, r, c)))
            })?;
            match trimmed {
                Some((p, r, c)) => {
                    rep.patches_trimmed.push(patch_from(p));
                    rep.records_deleted += r;
                    rep.count_rows_deleted += c;
                }
                None => {
                    rep.over_budget = true;
                    break;
                }
            }
        }
        rep.after_bytes = self.used_bytes(other);
        rep.over_budget |= rep.after_bytes > budget_bytes;
        rep.freed_bytes = rep.before_bytes.saturating_sub(rep.after_bytes);
        rep.millis = t0.elapsed().as_millis() as u64;
        *self.last_cleanup_freed.lock() = Some(rep.freed_bytes);
        self.log(format!(
            "clean-up: freed {} bytes ({} records, {} count rows, patches trimmed {:?}){}",
            rep.freed_bytes,
            rep.records_deleted,
            rep.count_rows_deleted,
            rep.patches_trimmed.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
            if rep.over_budget { "; still over the disk budget" } else { "" }
        ));
        Ok(rep)
    }

    /// Sizes measured on disk: the database split by part (SQLite's dbstat), plus the other folders.
    pub fn storage_status(&self, budget_bytes: u64, other: &OtherParts) -> Result<StorageStatus> {
        let db_total = self.db_bytes();
        let mut parts: Vec<(String, u64)> = vec![];
        let by_table: Vec<(String, i64)> = self.with_reader(|db| {
            let mut st = db.prepare("SELECT name, SUM(pgsize) FROM dbstat GROUP BY name")?;
            let v = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(v)
        })?;
        let mut sums = std::collections::BTreeMap::<&str, u64>::new();
        for (name, bytes) in &by_table {
            let part = if name.starts_with("counts") || name.starts_with("options") || name.starts_with("pairs") || name.contains("autoindex_options") {
                "counts"
            } else if name.starts_with("matches") || name.contains("autoindex_matches") {
                "records"
            } else if name.starts_with("validation") || name.contains("autoindex_validation") {
                "validation"
            } else {
                // seen, players, player_champs, meta, codec_dicts, schema.
                "pool"
            };
            *sums.entry(part).or_default() += *bytes as u64;
        }
        let pages: u64 = sums.values().sum();
        for part in ["counts", "records", "validation", "pool"] {
            parts.push((part.to_string(), sums.get(part).copied().unwrap_or(0)));
        }
        // Free pages and the WAL: on disk, in no table.
        parts.push(("free".into(), db_total.saturating_sub(pages)));
        parts.push(("cache".into(), 0));
        parts.extend(other.bytes());
        let used = parts.iter().map(|p| p.1).sum();
        let patches = self.with_reader(|db| Ok(patches_with_data(db)?.into_iter().rev().map(patch_from).collect()))?;
        Ok(StorageStatus { budget_bytes, used_bytes: used, parts, patches, last_cleanup_freed_bytes: *self.last_cleanup_freed.lock() })
    }
}

/// Patches with any data, oldest first.
fn patches_with_data(db: &Connection) -> Result<Vec<i64>> {
    let mut st = db.prepare("SELECT patch FROM pairs UNION SELECT patch FROM matches UNION SELECT patch FROM counts ORDER BY 1")?;
    let v = st.query_map(params![], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(v)
}
