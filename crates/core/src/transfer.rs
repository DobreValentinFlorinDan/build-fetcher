//! Port of `server/transfer.ts`: backing up the high-elo pool. An export is a ZIP with manifest.json and
//! irelia-pool.db (other players' IDs blanked), written as <file>.partial and renamed when complete. Imports only add
//! games that are not there yet. 0.22.0's exports import here and 1.0's into 0.22.0 (same manifest, same database).

use crate::db::{Db, MatchRecord, OpenOptions, PilotUpdate, SCHEMA_VERSION};
use crate::model::eval_travel::{evaluation_to_export, stage_incoming_evaluation, Entry, EVAL_ENTRIES};
use crate::zip::{extract_entry, read_entry, read_zip, ZipEntry, ZipWriter};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const EXPORT_FORMAT: &str = "irelia-companion-export";
pub const POOL_EXPORT: &str = "irelia-pool";
const BATCH: usize = 200;
const ALL_SOURCES: [&str; 4] = ["otp", "main", "harvested", "me"];

pub type Progress<'a> = &'a dyn Fn(&str, Option<f64>);

/// "irelia-pool_2026-10-30_1830.zip"
pub fn export_file_name(name: &str, at: i64) -> String {
    let t = chrono::DateTime::from_timestamp_millis(at).unwrap_or_default();
    format!("{name}_{}.zip", t.format("%Y-%m-%d_%H%M"))
}

/// The name an export's file belongs to (the pool), or None: "<name>_YYYY-MM-DD_HHMM.zip".
pub fn export_owner(file: &str) -> Option<String> {
    let base = Path::new(file).file_name()?.to_string_lossy().to_string();
    let stem = base.strip_suffix(".zip")?;
    if stem.len() < 17 || !stem.is_char_boundary(stem.len() - 15) {
        return None;
    }
    let (prefix, stamp) = stem.split_at(stem.len() - 15);
    let name = prefix.strip_suffix('_')?;
    let b = stamp.as_bytes();
    let digits = |r: std::ops::Range<usize>| r.into_iter().all(|i| b[i].is_ascii_digit());
    let ok = digits(0..4) && b[4] == b'-' && digits(5..7) && b[7] == b'-' && digits(8..10) && b[10] == b'_' && digits(11..15);
    (ok && name == POOL_EXPORT).then(|| name.to_string())
}

fn matches_pattern(f: &str, name: &str) -> bool {
    f.strip_prefix(&format!("{name}_")).map(|_| export_owner(f).as_deref() == Some(name)).unwrap_or(false)
}

/// A scratch folder next to the target (same disk), removed when dropped.
struct Scratch(PathBuf);
impl Scratch {
    fn new(near: &Path) -> Result<Scratch> {
        let dir = near.join(format!(".irelia-transfer-{}", crate::model::run_eval::random_hex(4)));
        std::fs::create_dir_all(&dir)?;
        Ok(Scratch(dir))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn need_space(dir: &Path, bytes: f64, what: &str) -> Result<()> {
    if let Some((free, _)) = crate::storage::free_space(dir) {
        let want = bytes + 256.0 * 1024.0 * 1024.0;
        if (free as f64) < want {
            return Err(anyhow!("Not enough free space for {what}: about {} GB is needed on that drive.", (want / 1024f64.powi(3)).ceil()));
        }
    }
    Ok(())
}

fn summary(db: &Db) -> Result<(i64, Vec<Value>)> {
    let sources: Vec<String> = ALL_SOURCES.iter().map(|s| s.to_string()).collect();
    let patches = db.patch_breakdown(&sources)?;
    let games = patches.iter().map(|p| p.1).sum();
    Ok((
        games,
        patches
            .into_iter()
            .map(|(p, g, lanes)| {
                let mut l = serde_json::Map::new();
                for (pos, n) in lanes {
                    l.insert(pos, json!(n));
                }
                json!({ "patch": p, "games": g, "lanes": l })
            })
            .collect(),
    ))
}

pub fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()))
        .unwrap_or_else(|| "this computer".into())
}

/// The high-elo pool as a ZIP in `folder`; (file, bytes, games, exportedAt, evaluation).
pub fn export_pool(data_dir: &Path, folder: &Path, app: &str, progress: Progress) -> Result<Value> {
    let src = data_dir.join("irelia.db");
    std::fs::create_dir_all(folder)?;
    let size = std::fs::metadata(&src)?.len() as f64;
    need_space(folder, size * 1.6, "the pool export")?;
    let tmp = Scratch::new(folder)?;
    progress("Copying the pool database (this takes a while on a large one)", Some(0.02));
    let copy = tmp.0.join("irelia-pool.db");
    let evaluation = {
        let db = Db::open(&src, OpenOptions { migrate: false, busy_timeout_ms: 30_000 })?;
        db.vacuum_into(&copy)?;
        evaluation_to_export(&db).unwrap_or(None)
    };
    progress("Removing other players' IDs", Some(0.2));
    let (games, patches) = {
        let c = Db::open_default(&copy)?;
        let keep: Vec<String> = {
            let mut st = c.conn.prepare("SELECT puuid FROM players WHERE pilot_class IN ('otp', 'main') UNION SELECT DISTINCT puuid FROM irelia_games")?;
            let r = st.query_map([], |r| r.get(0))?.flatten().collect();
            r
        };
        c.strip_other_players(&keep)?;
        c.conn.execute_batch("DELETE FROM players WHERE COALESCE(pilot_class, 'none') NOT IN ('otp', 'main'); DELETE FROM meta WHERE key LIKE 'seeded_at:%';")?;
        c.conn.execute_batch("PRAGMA journal_mode = DELETE")?;
        summary(&c)?
    };
    let at = crate::util::now_ms();
    let mut manifest = json!({
        "format": EXPORT_FORMAT, "version": 1, "kind": "pool", "name": POOL_EXPORT, "exportedAt": at, "machine": hostname(), "app": app,
        "schema": SCHEMA_VERSION, "games": games, "patches": patches, "stripped": true, "database": "irelia-pool.db",
    });
    if let Some((info, _)) = &evaluation {
        manifest["evaluation"] = info.clone();
    }
    let file = folder.join(export_file_name(POOL_EXPORT, at));
    let mut zip = ZipWriter::new(&file);
    let r: Result<(PathBuf, u64)> = (|| {
        zip.add_buffer("manifest.json", format!("{}\n", serde_json::to_string_pretty(&manifest)?).as_bytes())?;
        let db_size = std::fs::metadata(&copy)?.len() as f64;
        zip.add_file("irelia-pool.db", &copy, &mut |n| progress("Compressing", Some(0.3 + 0.65 * (n as f64 / db_size.max(1.0)))))?;
        if let Some((_, entries)) = &evaluation {
            for (name, e) in entries {
                match e {
                    Entry::Bytes(b) => zip.add_buffer(name, b)?,
                    Entry::File(f) => zip.add_file(name, f, &mut |_| {})?,
                }
            }
        }
        Ok((file.clone(), 0))
    })();
    if let Err(e) = r {
        zip.abort();
        return Err(e);
    }
    let (path, bytes) = zip.finish()?;
    Ok(json!({ "file": path.to_string_lossy(), "bytes": bytes, "games": games, "exportedAt": at, "evaluation": evaluation.is_some() }))
}

/// What a ZIP holds, from its manifest; a readable message for anything that isn't one of ours.
pub fn inspect_export(file: &Path) -> Result<(Value, Vec<ZipEntry>)> {
    let entries = read_zip(file)?;
    let m = entries.iter().find(|e| e.name == "manifest.json").ok_or_else(|| anyhow!("This ZIP is not an Irelia Companion export (no manifest.json)."))?;
    let manifest: Value = serde_json::from_slice(&read_entry(file, m)?)?;
    if manifest["format"] != EXPORT_FORMAT {
        return Err(anyhow!("This ZIP is not an Irelia Companion export."));
    }
    if manifest["kind"] != "pool" {
        return Err(anyhow!("This export holds a player data set, which this version no longer imports. Only pool exports can be imported."));
    }
    if manifest["schema"].as_i64().unwrap_or(0) > SCHEMA_VERSION {
        return Err(anyhow!("This export comes from a newer Irelia Companion: update the app first."));
    }
    let db_name = manifest["database"].as_str().unwrap_or("");
    if !entries.iter().any(|e| e.name == db_name) {
        return Err(anyhow!("This export is incomplete (its database is missing)."));
    }
    Ok((manifest, entries))
}

/// Copies every Irelia game of `from` into `to` (those not there yet), BATCH at a time.
pub fn copy_games(from: &Db, to: &Db, source: Option<&str>, progress: &dyn Fn(usize, usize)) -> Result<usize> {
    let ids = from.irelia_match_ids()?;
    let mut added = 0;
    for (i, chunk) in ids.chunks(BATCH).enumerate() {
        let records: Vec<MatchRecord> = chunk.iter().filter(|id| !to.has_match(id)).map(|id| from.match_record(id)).collect::<Result<_>>()?;
        if !records.is_empty() {
            added += to.import_records(records, source)?;
        }
        progress(((i + 1) * BATCH).min(ids.len()), ids.len());
    }
    Ok(added)
}

/// Imports a pool export: the games it has that this pool doesn't, and the pilots this computer didn't follow yet.
pub fn import_export(data_dir: &Path, file: &Path, confirmed: bool, progress: Progress) -> Result<Value> {
    let (manifest, entries) = inspect_export(file)?;
    if !confirmed {
        return Err(anyhow!("Importing a pool export changes the pool: confirm it first."));
    }
    let entry = entries.iter().find(|e| Some(e.name.as_str()) == manifest["database"].as_str()).unwrap().clone();
    std::fs::create_dir_all(data_dir)?;
    need_space(data_dir, entry.size as f64 * 1.2, "the import")?;
    let tmp = Scratch::new(data_dir)?;
    let copy = tmp.0.join("import.db");
    extract_entry(file, &entry, &copy, &mut |n| progress("Unpacking", Some(0.4 * (n as f64 / (entry.size.max(1)) as f64))))?;
    let from = Db::open_default(&copy)?;
    let pool = Db::open(&data_dir.join("irelia.db"), OpenOptions { migrate: false, busy_timeout_ms: 30_000 })?;
    let added = copy_games(&from, &pool, None, &|done, total| {
        progress(&format!("Adding games ({} of {})", crate::js::thousands(done as f64), crate::js::thousands(total as f64)), Some(0.4 + 0.6 * (done as f64 / total.max(1) as f64)))
    })?;
    let pilots: Vec<String> = {
        let mut st = from.conn.prepare("SELECT puuid FROM players WHERE pilot_class IN ('otp', 'main')")?;
        let r = st.query_map([], |r| r.get(0))?.flatten().collect();
        r
    };
    pool.tx(|| {
        for p in &pilots {
            if pool.player(p).is_some() {
                continue;
            }
            let Some(row) = from.player(p) else { continue };
            pool.upsert_player(&row.puuid, &row.platform, &row.tier, row.league_points)?;
            pool.set_pilot(&row.puuid, &PilotUpdate {
                pilot_class: row.pilot_class.clone().unwrap_or_else(|| "otp".into()),
                irelia_points: row.irelia_points.unwrap_or(0),
                irelia_last_play: row.irelia_last_play,
                irelia_rank: row.irelia_rank,
            })?;
        }
        Ok(())
    })?;
    let mut evaluation = false;
    if !manifest["evaluation"].is_null() && EVAL_ENTRIES.iter().all(|n| entries.iter().any(|e| e.name == *n)) {
        stage_incoming_evaluation(
            &mut |name, dest| {
                let e = entries.iter().find(|e| e.name == name).ok_or_else(|| anyhow!("missing {name}"))?;
                extract_entry(file, e, dest, &mut |_| {})
            },
            manifest["machine"].as_str().unwrap_or("another computer"),
            manifest["exportedAt"].as_f64().unwrap_or(0.0),
        )?;
        evaluation = true;
    }
    Ok(json!({ "name": POOL_EXPORT, "added": added, "games": manifest["games"], "exportedAt": manifest["exportedAt"], "evaluation": evaluation }))
}

/// The computer an export was made on, or None.
pub fn export_machine(file: &Path) -> Option<String> {
    let entries = read_zip(file).ok()?;
    let m = entries.iter().find(|e| e.name == "manifest.json")?;
    let manifest: Value = serde_json::from_slice(&read_entry(file, m).ok()?).ok()?;
    (manifest["format"] == EXPORT_FORMAT).then(|| manifest["machine"].as_str().map(String::from)).flatten()
}

/// Keeps the `keep` newest exports of `name` in `folder`; with `machine`, only that computer's count and go.
pub fn prune_exports(folder: &Path, name: &str, keep: usize, machine: Option<&str>) -> usize {
    let mut files: Vec<String> = std::fs::read_dir(folder)
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|f| matches_pattern(f, name)).collect())
        .unwrap_or_default();
    files.sort();
    files.reverse();
    if let Some(m) = machine {
        files.retain(|f| export_machine(&folder.join(f)).as_deref() == Some(m));
    }
    let gone: Vec<String> = files.into_iter().skip(keep).collect();
    for f in &gone {
        let _ = std::fs::remove_file(folder.join(f));
    }
    gone.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn export_names() {
        assert_eq!(export_owner("irelia-pool_2026-10-30_1830.zip").as_deref(), Some("irelia-pool"));
        assert_eq!(export_owner("/x/irelia-pool_2026-10-30_1830.zip").as_deref(), Some("irelia-pool"));
        assert_eq!(export_owner("irelia-pool_2026-10-30_18300.zip"), None);
        assert_eq!(export_owner("irelia-me_2026-10-30_1830.zip"), None);
        assert_eq!(export_file_name("irelia-pool", 1_793_998_096_170), "irelia-pool_2026-11-06_2048.zip");
    }
}
