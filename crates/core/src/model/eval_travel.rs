//! Port of `server/model/evalTravel.ts`: an evaluation that travels with a backup. A pool backup carries the
//! computer's last evaluation; the computer importing it takes it as its own when it is valid there (the same app,
//! Data Dragon and settings; every game it measured in this pool; no evaluation of its own covering as many games).

use super::eval_base::{base_file, current_key, current_portable_key, decode_base, read_base, read_base_header, BaseHeader, ImportedFrom, EVAL_LOGIC, UPDATE_RULES};
use super::evaluate::base_hash;
use super::run_eval::{eval_mark, format_eval, iso_minutes, write_summary};
use super::service::{eval_dir, read_evaluation, EvalSummary, FullRef, Mark};
use crate::db::Db;
use crate::ddragon::StaticData;
use anyhow::Result;
use serde_json::{json, Value};
use std::path::PathBuf;

pub const ENTRY_HEADER: &str = "evaluation/base.json";
pub const ENTRY_BASE: &str = "evaluation/base.bin";
pub const ENTRY_SUMMARY: &str = "evaluation/latest.json";
pub const ENTRY_SEEN_FULL: &str = "evaluation/seen-full.bin";
pub const ENTRY_SEEN_LAST: &str = "evaluation/seen-last.bin";
pub const EVAL_ENTRIES: [&str; 5] = [ENTRY_HEADER, ENTRY_BASE, ENTRY_SUMMARY, ENTRY_SEEN_FULL, ENTRY_SEEN_LAST];

fn hashes(salt: &str, ids: &[String]) -> Vec<f64> {
    let mut v: Vec<f64> = ids.iter().map(|id| base_hash(salt, id)).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v
}

fn f64_bytes(v: &[f64]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// An entry of the backup: bytes, or a file to copy.
pub enum Entry {
    Bytes(Vec<u8>),
    File(PathBuf),
}

/// This computer's last evaluation as a backup of `db` carries it (the manifest's info and the entries), or None.
pub fn evaluation_to_export(db: &Db) -> Result<Option<(Value, Vec<(String, Entry)>)>> {
    let Some(header) = read_base_header() else { return Ok(None) };
    let Some(summary) = read_evaluation() else { return Ok(None) };
    let Some(portable) = header.portable.clone() else { return Ok(None) };
    if summary.evaluated_at != header.last_at {
        return Ok(None);
    }
    let (Some(full_row), Some(last_row)) = (header.full_mark.row, header.last_mark.row) else { return Ok(None) };
    let Some(base) = read_base() else { return Ok(None) };
    if base.header.last_at != header.last_at {
        return Ok(None);
    }
    let ids_up_to = |row: f64| -> Result<Vec<String>> {
        let mut st = db.conn.prepare("SELECT match_id FROM irelia_games WHERE rowid <= ?")?;
        let r = st.query_map([row as i64], |r| r.get(0))?.filter_map(|x| x.ok()).collect();
        Ok(r)
    };
    let salt = base.data.salt.clone();
    let info = json!({ "fullAt": header.full_at, "lastAt": header.last_at, "known": header.known, "fullHeldOut": header.full_held_out, "portable": portable });
    Ok(Some((
        info,
        vec![
            (ENTRY_HEADER.into(), Entry::Bytes(serde_json::to_vec(&header)?)),
            (ENTRY_BASE.into(), Entry::File(base_file())),
            (ENTRY_SUMMARY.into(), Entry::Bytes(serde_json::to_vec(&summary)?)),
            (ENTRY_SEEN_FULL.into(), Entry::Bytes(f64_bytes(&hashes(&salt, &ids_up_to(full_row)?)))),
            (ENTRY_SEEN_LAST.into(), Entry::Bytes(f64_bytes(&hashes(&salt, &ids_up_to(last_row)?)))),
        ],
    )))
}

pub fn incoming_dir() -> PathBuf {
    eval_dir().join("incoming")
}
const ORIGIN: &str = "origin.json";

/// Saves a backup's evaluation entries (each written by `extract` to the file it is given), over any left before.
pub fn stage_incoming_evaluation(extract: &mut dyn FnMut(&str, &std::path::Path) -> Result<()>, machine: &str, exported_at: f64) -> Result<()> {
    let dir = incoming_dir();
    let tmp = eval_dir().join("incoming.tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    for entry in EVAL_ENTRIES {
        let name = entry.rsplit('/').next().unwrap();
        extract(entry, &tmp.join(name))?;
    }
    std::fs::write(tmp.join(ORIGIN), serde_json::to_vec(&json!({ "machine": machine, "exportedAt": exported_at }))?)?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&tmp, &dir)?;
    Ok(())
}

pub fn incoming_evaluation() -> bool {
    incoming_dir().join(ORIGIN).exists()
}

fn count_in(set: &[f64], salt: &str, ids: &[String]) -> usize {
    ids.iter().filter(|id| set.binary_search_by(|x| x.partial_cmp(&base_hash(salt, id)).unwrap()).is_ok()).count()
}

fn read_f64(file: &std::path::Path) -> Result<Vec<f64>> {
    let b = std::fs::read(file)?;
    Ok(b.chunks_exact(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect())
}

/// Takes the evaluation an imported backup left as this computer's own when it is valid here, else turns it down;
/// either way it is gone afterwards. None: none waiting. (adopted, message)
pub fn adopt_incoming_evaluation(db: &Db, data: &StaticData) -> Option<(bool, String)> {
    if !incoming_evaluation() {
        return None;
    }
    let dir = incoming_dir();
    let done = |adopted: bool, message: String| {
        let _ = std::fs::remove_dir_all(&dir);
        Some((adopted, message))
    };
    let r: Result<Option<(bool, String)>> = (|| {
        let origin: Value = serde_json::from_slice(&std::fs::read(dir.join(ORIGIN))?)?;
        let machine = origin["machine"].as_str().unwrap_or("another computer").to_string();
        let exported_at = origin["exportedAt"].as_f64().unwrap_or(0.0);
        let header: BaseHeader = serde_json::from_slice(&std::fs::read(dir.join("base.json"))?)?;
        let summary: EvalSummary = serde_json::from_slice(&std::fs::read(dir.join("latest.json"))?)?;
        let from = format!("{machine}'s evaluation");
        // The settings first: a backup from 0.x carries a base 1.0 can't read, and is told apart by its app version.
        if header.logic != EVAL_LOGIC || header.portable.is_none() || header.portable.as_deref() != Some(current_portable_key(db, data)?.as_str()) {
            return Ok(done(false, format!("{from} isn't used: the app version, Data Dragon's or the evaluation settings differ from this computer's (update both to the same version)")));
        }
        let base = match decode_base(&std::fs::read(dir.join("base.bin"))?) {
            Ok(b) => b,
            Err(_) => return Ok(done(false, format!("{from} came incomplete: not used"))),
        };
        let whole = base.report.as_ref().map(|r| r.evaluated_at) == Some(header.last_at) && base.known.len() as f64 == header.known && summary.evaluated_at == header.last_at;
        if !whole {
            return Ok(done(false, format!("{from} came incomplete: not used")));
        }
        if let Some(own) = read_base_header() {
            if own.full_held_out >= header.full_held_out {
                return Ok(done(false, format!(
                    "{from} isn't used: this computer's own covers as many games or more ({} against {})",
                    crate::js::thousands(own.full_held_out),
                    crate::js::thousands(header.full_held_out)
                )));
            }
        }
        let ids = db.irelia_match_ids()?;
        let salt = base.salt.clone();
        let mut known_sorted = base.known.clone();
        known_sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let missing = header.known - count_in(&known_sorted, &salt, &ids) as f64;
        if missing > UPDATE_RULES.max_missing * header.known {
            return Ok(done(false, format!("{from} isn't used: {} of the games it measured aren't in this pool", crate::js::thousands(missing))));
        }
        let new_since_last = ids.len() - count_in(&read_f64(&dir.join("seen-last.bin"))?, &salt, &ids);
        let new_since_full = ids.len() - count_in(&read_f64(&dir.join("seen-full.bin"))?, &salt, &ids);
        let now = eval_mark(db);
        let last_mark = Mark { row: now.row, games: now.games - new_since_last as f64, patch: summary.mark.as_ref().and_then(|m| m.patch.clone()).or(header.last_mark.patch.clone()) };
        let full_mark = Mark { row: now.row, games: now.games - new_since_full as f64, patch: header.full_mark.patch.clone() };
        std::fs::create_dir_all(eval_dir())?;
        let tmp = eval_dir().join("base.bin.tmp");
        std::fs::copy(dir.join("base.bin"), &tmp)?;
        std::fs::rename(&tmp, base_file())?;
        let imported = ImportedFrom { machine: machine.clone(), exported_at };
        let next_header = BaseHeader { key: current_key(db, data)?, full_mark: full_mark.clone(), last_mark: last_mark.clone(), imported_from: Some(imported), ..header.clone() };
        std::fs::write(eval_dir().join("base.json"), format!("{}\n", serde_json::to_string_pretty(&next_header)?))?;
        let mut next_summary = summary.clone();
        next_summary.mark = Some(last_mark);
        next_summary.full = Some(FullRef { evaluated_at: summary.full.as_ref().map(|f| f.evaluated_at).unwrap_or(header.full_at), mark: full_mark });
        next_summary.data_version = db.data_version();
        next_summary.imported_from = Some(super::service::ImportedFrom { machine: machine.clone(), exported_at });
        write_summary(&next_summary)?;
        let stamp = chrono::DateTime::from_timestamp_millis(header.last_at as i64).unwrap_or_default().format("%Y-%m-%d-%H-%M").to_string();
        if let Some(r) = &base.report {
            std::fs::write(
                eval_dir().join(format!("eval-imported-{stamp}.txt")),
                format!("Evaluation imported from {machine}'s backup of {}.\n\n{}\n", iso_minutes(exported_at), format_eval(r)),
            )?;
        }
        Ok(done(true, format!("{from} is used here: its verdicts apply now, and the next evaluation measures only the {} games it didn't have", crate::js::thousands(new_since_last as f64))))
    })();
    match r {
        Ok(x) => x,
        Err(e) => done(false, format!("the imported evaluation couldn't be read ({e}): not used")),
    }
}
