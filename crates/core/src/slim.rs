//! Port of `server/slim.ts`, `server/slimReads.ts` and `server/slimPart.ts`: Slim stored data (Settings → Storage), the
//! one action that deletes stored data, behind a backup, a check of the results and a rollback. Its backup folder
//! (backup-0.14.0) and manifest are 0.22.0's, so a run 0.22.0 left cut short is rolled back by 1.0 at its start.
//!
//! (b) keeps the response fields the captures read. 0.22.0 recorded them with a JS Proxy; here they are the field names
//! the Rust captures read (READ_FIELDS). Either way a slim copy is kept only when every capture comes out identical
//! from it, else the entry stays whole: the check, not the list, is what makes it safe.

use crate::capture::{capture_timeline, lane_opponents, leavers, match_quality, participant_rows, CAPTURE_VERSION, CHALLENGE_KEYS};
use crate::codec::{pack, unpack};
use crate::db::Db;
use crate::ddragon::{to_patch, StaticData};
use crate::riot::disk_cache::CacheStore;
use crate::scraper::ingest::capture_game;
use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const SLIM_DIR: &str = "backup-0.14.0";
pub const SLIM_META: &str = "slim_manifest";
const MANIFEST: &str = "manifest.json";
const DB_COPY: &str = "irelia.db";
const ENTRIES: &str = "cache-entries.db";
const ARCHIVE_LIST: &str = "archive-slimmed.json";
const BASELINE: &str = "baseline.json";
const RESTORE_BATCH: i64 = 500;
const MB: f64 = 1024.0 * 1024.0;
const IRELIA: i64 = 39;

pub fn slim_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(SLIM_DIR)
}

/// A step's progress: phase id, label, how far (None: can't be counted), and a count (done, total, unit).
pub type SlimTick<'a> = &'a (dyn Fn(&str, &str, Option<f64>, Option<Value>) + Sync);

fn file_size(f: &Path) -> u64 {
    std::fs::metadata(f).map(|m| m.len()).unwrap_or(0)
}
fn mb(b: f64) -> f64 {
    crate::js::round(b / MB)
}
fn count(done: f64, total: f64, unit: &str) -> Value {
    json!({ "done": crate::js::num(done), "total": crate::js::num(total), "unit": unit })
}

/// The backup's database copy as VACUUM INTO writes it: (part, count).
pub fn copy_count(data_dir: &Path, total: f64) -> (f64, Value) {
    let done = file_size(&slim_dir(data_dir).join(DB_COPY)) as f64;
    ((done / total.max(1.0)).min(0.99), count(mb(done), mb(done).max(mb(total)), "MB"))
}

// ------------------------------------------------------------------------------------------- manifest

pub fn read_manifest(data_dir: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(slim_dir(data_dir).join(MANIFEST)).ok()?).ok()
}

pub fn write_manifest(data_dir: &Path, m: &mut Value, db: Option<&Db>) -> Result<()> {
    m["updatedAt"] = json!(crate::util::now_ms());
    std::fs::create_dir_all(slim_dir(data_dir))?;
    let file = slim_dir(data_dir).join(MANIFEST);
    let tmp = slim_dir(data_dir).join(format!("{MANIFEST}.tmp"));
    std::fs::write(&tmp, format!("{}\n", serde_json::to_string_pretty(m)?))?;
    std::fs::rename(&tmp, &file)?;
    if let Some(db) = db {
        let _ = db.set_meta(SLIM_META, &crate::js::stringify(m));
    }
    Ok(())
}

pub fn sha256_file(file: &Path, on_bytes: &mut dyn FnMut(f64, f64)) -> Result<String> {
    let mut h = Sha256::new();
    let total = file_size(file) as f64;
    let mut f = std::fs::File::open(file)?;
    let mut buf = vec![0u8; 1 << 20];
    let (mut done, mut last) = (0f64, 0f64);
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        done += n as f64;
        if done - last >= 64.0 * MB {
            last = done;
            on_bytes(done, total);
        }
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn gunzip_json(blob: &[u8]) -> Option<Value> {
    let mut s = String::new();
    flate2::read::GzDecoder::new(blob).read_to_string(&mut s).ok()?;
    serde_json::from_str(&s).ok()
}

// ------------------------------------------------------------------------------------------- plan

#[derive(Clone, Debug)]
pub struct Key {
    pub kind: String,
    pub scope: String,
    pub key: String,
    pub bytes: i64,
}

pub struct Plan {
    pub plan: Value,
    pub timeline_keys: Vec<Key>,
    pub slim_keys: Vec<Key>,
    pub archived: Vec<String>,
}

pub fn plan_slim(db: &Db, store: &CacheStore, data: &StaticData) -> Result<Plan> {
    let newest: HashSet<String> = crate::model::run_eval::model_patches(db, data, 2.0)?.into_iter().collect();
    let old = crate::model::service::before_window(db, data, crate::model::weights::MEMORY_PATCHES)?;
    let mut match_info: HashMap<String, (String, i64, bool)> = HashMap::new();
    {
        let mut st = db.conn.prepare("SELECT m.match_id, m.patch, m.detail_version, (t.match_id IS NOT NULL) AS captured FROM matches m LEFT JOIN timelines t USING(match_id)")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?.unwrap_or(0), r.get::<_, i64>(3)? == 1)))?;
        for r in rows.flatten() {
            match_info.insert(r.0, (r.1, r.2, r.3));
        }
    }
    let mut archived_patch: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
    {
        let mut st = db.conn.prepare("SELECT match_id, patch FROM archive")?;
        for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.flatten() {
            archived_patch.insert(r.0, r.1);
        }
    }
    let archived_captured = |id: &str| -> bool {
        let row: Option<Vec<u8>> = db.conn.query_row("SELECT data FROM archive WHERE match_id = ?", [id], |r| r.get(0)).ok();
        let Some(rows) = row.and_then(|d| gunzip_json(&d)) else { return false };
        rows["matches"][0]["detail_version"].as_i64().unwrap_or(0) >= CAPTURE_VERSION && rows["timelines"].as_array().map(|a| !a.is_empty()).unwrap_or(false)
    };
    let (mut timeline_keys, mut slim_keys) = (vec![], vec![]);
    let (mut timeline_bytes, mut slim_bytes) = (0i64, 0i64);
    for e in store.match_entries()? {
        let m = match_info.get(&e.key);
        let patch = m.map(|x| x.0.clone()).or_else(|| archived_patch.get(&e.key).cloned());
        let captured = m.map(|x| x.1 >= CAPTURE_VERSION && x.2).unwrap_or(false)
            || (m.is_none()
                && e.kind == "timeline"
                && patch.as_ref().map(|p| !newest.contains(p)).unwrap_or(false)
                && archived_patch.contains_key(&e.key)
                && archived_captured(&e.key));
        let k = Key { kind: e.kind.clone(), scope: e.scope.clone(), key: e.key.clone(), bytes: e.bytes };
        if e.kind == "timeline" && patch.as_ref().map(|p| !newest.contains(p)).unwrap_or(false) && captured {
            timeline_bytes += e.bytes;
            timeline_keys.push(k);
            continue;
        }
        slim_bytes += e.bytes;
        slim_keys.push(k);
    }
    let archived: Vec<String> = archived_patch.iter().filter(|(_, p)| old(p) == Some(true)).map(|(id, _)| id.clone()).collect();
    let archive_bytes: i64 = if archived.is_empty() {
        0
    } else {
        db.conn.query_row(
            "SELECT COALESCE(SUM(LENGTH(data)), 0) AS b FROM archive WHERE match_id IN (SELECT value FROM json_each(?))",
            [serde_json::to_string(&archived)?],
            |r| r.get(0),
        )?
    };
    let (pages, size): (i64, i64) = db.conn.query_row(
        "SELECT (SELECT page_count FROM pragma_page_count()) - (SELECT freelist_count FROM pragma_freelist_count()), (SELECT page_size FROM pragma_page_size())",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let db_copy = pages * size;
    let plan = json!({
        "timelines": timeline_keys.len(), "timelineBytes": timeline_bytes, "entries": slim_keys.len(), "entryBytes": slim_bytes,
        "archivedMatches": archived.len(), "archiveBytes": archive_bytes, "dbCopyBytes": db_copy,
        "neededBytes": db_copy + timeline_bytes + slim_bytes + crate::storage::HEADROOM as i64,
    });
    Ok(Plan { plan, timeline_keys, slim_keys, archived })
}

/// None when the backup fits with the reserve left; else why not.
pub fn space_problem(data_dir: &Path, needed: f64) -> Option<String> {
    let free = match std::env::var("IRELIA_SLIM_FREE_BYTES").ok().and_then(|v| v.parse::<f64>().ok()) {
        Some(f) => Some(f),
        None => crate::storage::free_space(data_dir).map(|(free, _)| free as f64),
    };
    let free = free?;
    if free >= needed {
        return None;
    }
    let gb = |b: f64| format!("{} GB", crate::js::to_fixed(b / 1e9, 2));
    Some(format!("Not enough free space for the backup: {} needed (with the usual reserve), {} free. Nothing was changed.", gb(needed), gb(free)))
}

// ------------------------------------------------------------------------------------------- backup

const ENTRY_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS entries (
  kind TEXT NOT NULL, scope TEXT NOT NULL, key TEXT NOT NULL, stored_at INTEGER NOT NULL, data BLOB NOT NULL,
  part TEXT NOT NULL,
  PRIMARY KEY (kind, scope, key));";

pub fn backup(data_dir: &Path, db: &Db, store: &CacheStore, plan: &Plan, m: &mut Value, tick: SlimTick) -> Result<()> {
    let dir = slim_dir(data_dir);
    std::fs::create_dir_all(&dir)?;
    let copy = dir.join(DB_COPY);
    let _ = std::fs::remove_file(&copy);
    tick("copy", "Backing up: copying the database…", Some(0.0), None);
    db.vacuum_into(&copy)?;
    let hashing = "Backing up: checking the copy (SHA-256)…";
    tick("hash", hashing, Some(0.0), None);
    let sha = sha256_file(&copy, &mut |done, total| tick("hash", hashing, Some(done / total.max(1.0)), Some(count(mb(done), mb(total), "MB"))))?;
    m["db"] = json!({ "file": DB_COPY, "bytes": file_size(&copy), "sha256": sha });
    m["archived"] = json!(plan.archived.len());
    std::fs::write(dir.join(ARCHIVE_LIST), serde_json::to_string(&plan.archived)?)?;
    write_manifest(data_dir, m, Some(db))?;
    let out = Connection::open(dir.join(ENTRIES))?;
    out.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
    out.execute_batch(ENTRY_SCHEMA)?;
    let all: Vec<(&Key, &str)> = plan.timeline_keys.iter().map(|k| (k, "a")).chain(plan.slim_keys.iter().map(|k| (k, "b"))).collect();
    let mut bytes = 0i64;
    let moving = "Backing up: moving the affected cache entries into the backup…";
    tick("entries", moving, Some(0.0), Some(count(0.0, all.len() as f64, "cache entries")));
    for (i, batch) in all.chunks(200).enumerate() {
        out.execute_batch("BEGIN")?;
        let mut rows = vec![];
        let r: Result<()> = (|| {
            for (k, part) in batch {
                let row: Option<(i64, Vec<u8>)> = store
                    .db
                    .query_row("SELECT stored_at, data FROM entries WHERE kind = ? AND scope = ? AND key = ?", params![k.kind, k.scope, k.key], |r| Ok((r.get(0)?, r.get(1)?)))
                    .ok();
                let Some((at, data)) = row else { continue };
                out.execute("INSERT OR REPLACE INTO entries(kind, scope, key, stored_at, data, part) VALUES(?, ?, ?, ?, ?, ?)", params![k.kind, k.scope, k.key, at, data, part])?;
                bytes += data.len() as i64;
                rows.push(*k);
            }
            Ok(())
        })();
        match r {
            Ok(()) => out.execute_batch("COMMIT")?,
            Err(e) => {
                let _ = out.execute_batch("ROLLBACK");
                return Err(e);
            }
        }
        store.db.execute_batch("BEGIN IMMEDIATE")?;
        let r: Result<()> = rows.iter().try_for_each(|k| store.delete(&k.kind, &k.scope, &k.key));
        match r {
            Ok(()) => store.db.execute_batch("COMMIT")?,
            Err(e) => {
                let _ = store.db.execute_batch("ROLLBACK");
                return Err(e);
            }
        }
        let done = (i * 200 + batch.len()) as f64;
        tick("entries", moving, Some((done / (all.len().max(1)) as f64).min(1.0)), Some(count(done, all.len() as f64, "cache entries")));
    }
    let n: i64 = out.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))?;
    m["cache"] = json!({ "file": ENTRIES, "entries": n, "bytes": bytes });
    drop(out);
    write_manifest(data_dir, m, Some(db))
}

// ------------------------------------------------------------------------------------------- (b): the fields read

/// The field names the captures read (capture.rs, engine/parse_match.rs, scraper/ingest.rs).
pub fn read_fields() -> HashSet<String> {
    let mut s: HashSet<String> = [
        "metadata", "matchId", "info", "participants", "gameVersion", "queueId", "gameCreation", "gameDuration", "endOfGameResult", "platformId",
        "puuid", "participantId", "championId", "teamId", "teamPosition", "win", "summoner1Id", "summoner2Id", "kills", "deaths", "assists",
        "champLevel", "goldEarned", "totalMinionsKilled", "neutralMinionsKilled", "physicalDamageDealtToChampions", "magicDamageDealtToChampions",
        "trueDamageDealtToChampions", "totalDamageTaken", "damageSelfMitigated", "totalHeal", "totalHealsOnTeammates", "totalDamageShieldedOnTeammates",
        "timeCCingOthers", "totalTimeCCDealt", "timePlayed", "gameEndedInEarlySurrender", "gameEndedInSurrender", "challenges", "perks", "statPerks",
        "offense", "flex", "defense", "styles", "description", "style", "selections", "perk", "frames", "frameInterval", "interval", "timestamp",
        "participantFrames", "totalGold", "currentGold", "xp", "level", "minionsKilled", "jungleMinionsKilled", "position", "x", "y", "events", "type",
        "itemId", "beforeId", "afterId", "skillSlot", "levelUpType", "killerId", "victimId", "assistingParticipantIds", "bounty", "shutdownBounty",
        "killerTeamId", "teamId", "buildingType", "towerType", "laneType", "monsterType", "monsterSubType", "pos",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for k in CHALLENGE_KEYS {
        s.insert(k.to_string());
    }
    for i in 0..7 {
        s.insert(format!("item{i}"));
    }
    s
}

/// `value` with only the fields read: objects keep the read names (and numeric keys, which are ids), arrays every element.
fn keep_read(v: &Value, names: &HashSet<String>) -> Value {
    match v {
        Value::Object(o) => {
            let mut out = Map::new();
            for (k, x) in o {
                if names.contains(k) || k.chars().all(|c| c.is_ascii_digit()) {
                    out.insert(k.clone(), keep_read(x, names));
                }
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| keep_read(x, names)).collect()),
        other => other.clone(),
    }
}

/// Everything the app reads from a cached match (and its timeline): what ingest and the backfill store.
pub fn capture_outputs(m: &Value, timeline: Option<&Value>) -> Value {
    let info = &m["info"];
    let v = info["gameVersion"].as_str().unwrap_or("");
    let q = match_quality(m);
    let mut out = json!({
        "row": { "patch": to_patch(v), "version": v, "queue": info["queueId"], "creation": info["gameCreation"], "duration": info["gameDuration"] },
        "quality": { "remake": q.remake, "endResult": q.end_result },
        "participants": participant_rows(m).iter().map(|r| Value::Object(r.to_map())).collect::<Vec<_>>(),
    });
    let ps = info["participants"].as_array().cloned().unwrap_or_default();
    let irelias: Vec<&Value> = ps.iter().filter(|p| p["championId"].as_i64() == Some(IRELIA)).collect();
    out["irelias"] = Value::Array(irelias.iter().map(|p| json!({ "puuid": p["puuid"], "position": p["teamPosition"], "participantId": p["participantId"] })).collect());
    if let Some(t) = timeline {
        let cap = capture_timeline(t);
        out["timeline"] = cap.to_json();
        out["leavers"] = json!(leavers(&cap));
        out["games"] = Value::Array(
            irelias
                .iter()
                .map(|p| {
                    let g = capture_game(m, t, p, "otp", None);
                    let opp = lane_opponents(m, Some(&cap), p);
                    json!({ "row": format!("{:?}", g.row), "opp": [opp.position, opp.timeline, opp.effective] })
                })
                .collect(),
        );
    }
    out
}

/// The slim copies of a cached match and its timeline, when every capture comes out the same from them; else None.
pub fn slim_entry(m: &Value, timeline: Option<&Value>, names: &HashSet<String>) -> Option<(Value, Option<Value>)> {
    let r = std::panic::catch_unwind(|| {
        let before = crate::js::stringify(&capture_outputs(m, timeline));
        let sm = keep_read(m, names);
        let st = timeline.map(|t| keep_read(t, names));
        let after = crate::js::stringify(&capture_outputs(&sm, st.as_ref()));
        (after == before).then_some((sm, st))
    });
    r.ok().flatten()
}

pub struct PartPut {
    pub kind: String,
    pub stored_at: i64,
    pub blob: Vec<u8>,
    pub slim: bool,
    pub saved: i64,
}
pub struct PartResult {
    pub scope: String,
    pub key: String,
    pub puts: Vec<PartPut>,
}

/// One cached game slimmed: read back, slimmed where every capture comes out the same, compressed again. Pure.
pub fn slim_game<L: Fn(u16) -> Option<Arc<Vec<u8>>>, P: Fn(&str, &[u8]) -> Option<Vec<u8>>>(scope: &str, key: &str, rows: &[(String, i64, Vec<u8>)], lookup: &L, pack_with: &P, names: &HashSet<String>) -> PartResult {
    let orig: Vec<(String, i64, Vec<u8>, Option<String>)> =
        rows.iter().map(|(k, at, d)| (k.clone(), *at, d.clone(), unpack(d, lookup).ok().and_then(|b| String::from_utf8(b).ok()))).collect();
    let find = |k: &str| orig.iter().find(|o| o.0 == k);
    let parse = |o: Option<&(String, i64, Vec<u8>, Option<String>)>| o.and_then(|x| x.3.as_ref()).and_then(|j| serde_json::from_str::<Value>(j).ok());
    let m = parse(find("match"));
    let t = parse(find("timeline"));
    let has_irelia = m.as_ref().and_then(|m| m["info"]["participants"].as_array().map(|a| a.iter().any(|p| p["championId"].as_i64() == Some(IRELIA)))).unwrap_or(false);
    let slim = match &m {
        Some(m) if !(has_irelia && find("timeline").is_none()) => slim_entry(m, t.as_ref(), names),
        _ => None,
    };
    let puts = orig
        .iter()
        .map(|(kind, at, data, _)| {
            let next = slim.as_ref().and_then(|(sm, st)| if kind == "match" { Some(crate::js::stringify(sm)) } else { st.as_ref().map(crate::js::stringify) });
            let blob = next.as_ref().and_then(|n| pack_with(kind, n.as_bytes()));
            let ok = match (&next, &blob) {
                (Some(n), Some(b)) => b.len() < data.len() && unpack(b, lookup).map(|x| x == n.as_bytes()).unwrap_or(false),
                _ => false,
            };
            if ok {
                let b = blob.unwrap();
                PartPut { kind: kind.clone(), stored_at: *at, saved: data.len() as i64 - b.len() as i64, blob: b, slim: true }
            } else {
                PartPut { kind: kind.clone(), stored_at: *at, blob: data.clone(), slim: false, saved: 0 }
            }
        })
        .collect();
    PartResult { scope: scope.into(), key: key.into(), puts }
}

// ------------------------------------------------------------------------------------------- apply

const CLEARED: [(&str, fn() -> Value); 11] = [
    ("puuid", || json!("")),
    ("champ_level", || json!(0)),
    ("kills", || json!(0)),
    ("deaths", || json!(0)),
    ("assists", || json!(0)),
    ("gold_earned", || json!(0)),
    ("cs", || json!(0)),
    ("items", || json!("[]")),
    ("cc_dealt", || json!(0)),
    ("time_played", || json!(0)),
    ("challenges", || Value::Null),
];

/// An archived match's rows with the other nine players' unread columns cleared (None: nothing to clear).
pub fn slim_archived(rows: &Value) -> Option<Value> {
    let irelia: Vec<Value> = rows["irelia_games"].as_array().map(|a| a.iter().map(|g| g["participant_id"].clone()).collect()).unwrap_or_default();
    let mut changed = false;
    let participants: Vec<Value> = rows["participants"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|p| {
            if irelia.contains(&p["participant_id"]) {
                return p;
            }
            let mut next = p.clone();
            for (k, v) in CLEARED {
                next[k] = v();
            }
            if CLEARED.iter().any(|(k, _)| p.get(*k) != next.get(*k)) {
                changed = true;
            }
            next
        })
        .collect();
    if !changed {
        return None;
    }
    let mut out = rows.clone();
    out["participants"] = Value::Array(participants);
    Some(out)
}

const PART_GAMES: usize = 50;

pub fn apply(data_dir: &Path, db: &Db, store: &CacheStore, m: &mut Value, tick: SlimTick, threads: usize, stop: &dyn Fn() -> bool) -> Result<()> {
    let dir = slim_dir(data_dir);
    let src = Connection::open_with_flags(dir.join(ENTRIES), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let (mut slimmed, mut kept, mut saved_bytes) = (0i64, 0i64, 0i64);
    let ids: Vec<(String, String)> = {
        let mut st = src.prepare("SELECT DISTINCT scope, key FROM entries WHERE part = 'b' ORDER BY scope, key")?;
        let r = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.flatten().collect();
        r
    };
    let cached = "Slimming the cached matches and timelines…";
    tick("cache", cached, Some(0.0), Some(count(0.0, ids.len() as f64, "games")));
    let names = read_fields();
    let current = store.dicts.current();
    let dicts: HashMap<u16, Arc<Vec<u8>>> = store.dicts.all(&store.db).into_iter().collect();
    let lookup = move |id: u16| dicts.get(&id).cloned();
    let mut done = 0usize;
    let pool = rayon::ThreadPoolBuilder::new().num_threads(if current.is_some() { threads.max(1) } else { 1 }).build()?;
    for chunk in ids.chunks(PART_GAMES) {
        if stop() {
            return Err(anyhow!("stopped"));
        }
        let games: Vec<(String, String, Vec<(String, i64, Vec<u8>)>)> = chunk
            .iter()
            .map(|(scope, key)| {
                let mut st = src.prepare_cached("SELECT kind, stored_at, data FROM entries WHERE kind IN ('match', 'timeline') AND scope = ? AND key = ? AND part = 'b'")?;
                let rows = st.query_map([scope, key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.flatten().collect();
                Ok((scope.clone(), key.clone(), rows))
            })
            .collect::<Result<_>>()?;
        let results: Vec<PartResult> = match &current {
            Some(dict) => {
                use rayon::prelude::*;
                let pack_with = |_: &str, raw: &[u8]| pack(raw, Some(dict)).ok();
                pool.install(|| games.par_iter().map(|(s, k, rows)| slim_game(s, k, rows, &lookup, &pack_with, &names)).collect())
            }
            None => {
                // The first blob written makes the cache's dictionary (CacheStore::encode): on this thread alone.
                let lk = |id: u16| store.dicts.get(&store.db, id);
                let enc = std::sync::Mutex::new(());
                let pack_with = |kind: &str, raw: &[u8]| {
                    let _g = enc.lock();
                    store.encode(kind, raw).ok()
                };
                let _ = &lookup;
                games.iter().map(|(s, k, rows)| slim_game(s, k, rows, &lk, &pack_with, &names)).collect()
            }
        };
        store.db.execute_batch("BEGIN IMMEDIATE")?;
        let r: Result<()> = (|| {
            for r in &results {
                for p in &r.puts {
                    store.put(&p.kind, &r.scope, &r.key, &p.blob, p.stored_at)?;
                    if p.slim {
                        slimmed += 1;
                        saved_bytes += p.saved;
                    } else {
                        kept += 1;
                    }
                }
            }
            Ok(())
        })();
        match r {
            Ok(()) => store.db.execute_batch("COMMIT")?,
            Err(e) => {
                let _ = store.db.execute_batch("ROLLBACK");
                return Err(e);
            }
        }
        done += chunk.len();
        tick("cache", cached, Some(done as f64 / ids.len().max(1) as f64), Some(count(done as f64, ids.len() as f64, "games")));
    }
    drop(src);
    // (c) The archive's old patches.
    let list: Vec<String> = serde_json::from_str(&std::fs::read_to_string(dir.join(ARCHIVE_LIST))?)?;
    let (mut archived, mut archive_saved) = (0i64, 0i64);
    let archiving = "Slimming the archive…";
    tick("archive", archiving, Some(0.0), Some(count(0.0, list.len() as f64, "archived games")));
    for (i, batch) in list.chunks(200).enumerate() {
        if stop() {
            return Err(anyhow!("stopped"));
        }
        db.tx(|| {
            for id in batch {
                let row: Option<Vec<u8>> = db.conn.query_row("SELECT data FROM archive WHERE match_id = ?", [id], |r| r.get(0)).ok();
                let Some(data) = row else { continue };
                let rows = gunzip_json(&data).ok_or_else(|| anyhow!("Archived match {id} could not be read"))?;
                let Some(next) = slim_archived(&rows) else { continue };
                let text = crate::js::stringify(&next);
                let blob = crate::db::gzip(text.as_bytes());
                let back = gunzip_json(&blob).ok_or_else(|| anyhow!("Archived match {id} did not read back the same"))?;
                if crate::js::stringify(&back) != text {
                    return Err(anyhow!("Archived match {id} did not read back the same"));
                }
                db.conn.execute("UPDATE archive SET data = ? WHERE match_id = ?", params![blob, id])?;
                archived += 1;
                archive_saved += data.len() as i64 - blob.len() as i64;
            }
            Ok(())
        })?;
        let d = (i * 200 + batch.len()).min(list.len()) as f64;
        tick("archive", archiving, Some(d / list.len().max(1) as f64), Some(count(d, list.len() as f64, "archived games")));
    }
    tick("reclaim", "Freeing the disk space it saved…", None, None);
    let _ = store.reclaim();
    let _ = db.reclaim();
    m["applied"] = json!({ "slimmed": slimmed, "kept": kept, "archived": archived, "savedBytes": saved_bytes + archive_saved });
    write_manifest(data_dir, m, Some(db))
}

// ------------------------------------------------------------------------------------------- the results

fn order(id: &str) -> u32 {
    let mut h: u32 = 2_166_136_261;
    for c in id.encode_utf16() {
        h = (h ^ c as u32).wrapping_mul(16_777_619);
    }
    h
}

/// What the slimming must not change: a quick evaluation (not saved), and for 200 stored games the lane baseline, the
/// draft-adjusted build and the live "Next" at 5..25 minutes, from the model built as the app builds it.
pub fn results(db: &Db, data: &Arc<StaticData>, threads: usize, drafts: usize, tick: SlimTick, stop: &dyn Fn() -> bool) -> Result<Value> {
    use crate::model::run_eval::{evaluate_data_folder, FolderOptions};
    let evaluating = "the quick evaluation";
    tick("eval", evaluating, Some(0.0), None);
    let r = evaluate_data_folder(db, data.clone(), FolderOptions {
        quick: true,
        auto: false,
        path: "full",
        threads,
        log: Box::new(|_| {}),
        progress: Box::new(|p| tick("eval", evaluating, Some(crate::progress::step_fraction(p.step, p.steps, p.part.unwrap_or(0.0), None)), Some(count(p.step as f64, p.steps as f64, "evaluation steps")))),
        save: false,
        control: None,
        grouping: None,
        gpu: false,
    })?;
    let mut evaluation = serde_json::to_value(&r.report)?;
    if let Some(o) = evaluation.as_object_mut() {
        o.remove("evaluatedAt");
    }
    if stop() {
        return Err(anyhow!("stopped"));
    }
    tick("model", "building the model", None, None);
    let scope = crate::scope::load_scope().unwrap_or_else(|_| crate::scope::default_scope());
    let excl = Arc::new(crate::engine::exclusivity::build_exclusivity_index(data));
    let region_patch = crate::model::run_eval::region_patch_fn(db, data)?;
    let env = crate::model::service::ModelEnv {
        data: data.clone(),
        excl: excl.clone(),
        scope: scope.clone(),
        region_patch: region_patch.clone(),
        own_platform: None,
        busy: false,
        starting_item_window_ms: crate::config::STARTING_ITEM_WINDOW_MS,
    };
    let mut service = crate::model::service::ModelService::default();
    service.rebuild(db, &env, threads);
    let Some(built) = service.current() else { return Ok(json!({ "evaluation": evaluation, "drafts": null, "error": service.error() })) };
    let settings = crate::model::service::read_settings(&scope, None, None);
    let sources = scope.build_sources.clone();
    let patches: Vec<String> = db.patch_breakdown(&sources)?.into_iter().map(|p| p.0).collect();
    let lane_games: Vec<Arc<crate::db::IreliaGameRow>> = db.games_for_patches(&patches, &sources)?.into_iter().map(Arc::new).collect();
    let mut games: Vec<Arc<crate::model::dataset::PreparedGame>> = built.model.scored.iter().filter(|s| s.g.source != "me").map(|s| s.g.clone()).collect();
    games.sort_by(|a, b| order(&a.match_id).cmp(&order(&b.match_id)).then(a.match_id.cmp(&b.match_id)));
    games.truncate(drafts);
    let weighted = crate::model::draft::weigh_games(&built.model, data, &region_patch, &settings.weights);
    let mut by_slot: HashMap<usize, crate::model::estimate::ObsRef> = HashMap::new();
    let ids: Vec<String> = games.iter().map(|g| g.match_id.clone()).collect();
    let timelines = db.timelines_of(&ids)?;
    let n_games = games.len();
    tick("drafts", "the drafts", Some(0.0), Some(count(0.0, n_games as f64, "drafts")));
    let mut baselines: HashMap<String, Value> = HashMap::new();
    let mut out = vec![];
    let built_model = &built.model;
    for (n, g) in games.iter().enumerate() {
        if stop() {
            return Err(anyhow!("stopped"));
        }
        if n % 10 == 9 {
            tick("drafts", "the drafts", Some((n + 1) as f64 / n_games as f64), Some(count((n + 1) as f64, n_games as f64, "drafts")));
        }
        let bk = format!("{}|{:?}", g.lane, g.opponent);
        if !baselines.contains_key(&bk) {
            let opp = g.opponent.filter(|o| data.champions.contains_key(o));
            let b = crate::engine::recommend::recommend(&lane_games, data, &excl, opp, &crate::engine::recommend::EngineSettings::default(), Some(g.lane), &*region_patch, None);
            baselines.insert(bk.clone(), serde_json::to_value(&b)?);
        }
        let baseline_v = baselines[&bk].clone();
        let baseline: crate::types::BuildRecommendation = serde_json::from_value(baseline_v.clone())?;
        let q = crate::model::draft::DraftQuery { lane: g.lane, opponent: g.opponent, enemies: g.enemies.clone(), allies: g.allies.clone() };
        let draft = crate::model::draft::recommend_draft(built_model, &q, data, &excl, &region_patch, &baseline, &settings, &Default::default());
        let row: Option<crate::db::IreliaGameRow> = timelines.get(&g.match_id).and_then(|t| {
            let mut st = db.conn.prepare("SELECT * FROM irelia_games WHERE match_id = ?").ok()?;
            let mut r = st.query_row([&g.match_id], |r| Ok(crate::db::IreliaGameRow::from_row(r))).ok()?;
            r.platform = Some(g.platform.clone());
            r.timeline = Some(t.clone());
            Some(r)
        });
        let ps = db.participants_of(&g.match_id)?;
        let refs: Vec<&crate::db::ParticipantLite> = ps.iter().collect();
        let tl = row.as_ref().and_then(|r| crate::model::dataset::game_timeline(r, &refs));
        let next = tl.map(|tl| {
            crate::model::dataset::moments_at(&tl, &[5.0, 10.0, 15.0, 20.0, 25.0].map(|m| m * 60_000.0), &built.profiles, data)
                .into_iter()
                .map(|mo| {
                    let slot = mo.owned.iter().filter(|id| crate::engine::validator::is_completed_item(data.item(**id as i64))).count();
                    if slot >= 6 {
                        return Value::Null;
                    }
                    let mut z = crate::model::context::standardise(&g.draft, &built_model.draft_scale);
                    z.extend(crate::model::context::standardise(&mo.live, &built_model.live_scale));
                    let obs = by_slot
                        .entry(slot)
                        .or_insert_with(|| {
                            let mut list = vec![];
                            for wg in &weighted.list {
                                let sg = &built_model.scored[wg.i];
                                let Some(c) = sg.g.items.get(slot) else { continue };
                                let w = wg.base * weighted.patch(&sg.g, &[c.item as i64]);
                                if w > 0.0 {
                                    list.push(crate::model::estimate::Observation {
                                        option: crate::model::draft::opt_str(c.item),
                                        wpa: sg.item_wpa[slot],
                                        win: sg.g.win,
                                        start_win: sg.item_w0[slot],
                                        w,
                                        lane: sg.g.lane,
                                        opponent: sg.g.opponent,
                                        pilot: Arc::from(sg.g.pilot.as_str()),
                                        z: crate::model::estimate::Z::Row(built_model.z_items.clone(), built_model.item_row[wg.i] as usize + slot),
                                        row: None,
                                        age: Some(wg.age),
                                    });
                                }
                            }
                            crate::model::draft::observed(list, &settings.weights)
                        })
                        .clone();
                    let query = crate::model::estimate::Query { lane: g.lane, opponent: g.opponent, z, sims: None, memo: None };
                    let est = crate::model::estimate::estimate_decision(&obs, &query, &settings.estimate, None, None);
                    Value::Array(
                        est.options
                            .iter()
                            .filter(|x| crate::engine::exclusivity::is_purchasable_item(data.item(crate::js::to_number(&json!(&*x.option)) as i64)))
                            .take(3)
                            .map(|x| json!([&*x.option, crate::js::num(x.theta), crate::js::num(x.n_eff)]))
                            .collect(),
                    )
                })
                .collect::<Vec<_>>()
        });
        out.push(json!({ "id": g.match_id, "baseline": baseline_v, "draft": serde_json::to_value(draft)?, "next": next }));
    }
    Ok(json!({ "evaluation": evaluation, "drafts": out, "error": null }))
}

/// The first places two results differ, as paths, at most `max`.
pub fn differences(a: &Value, b: &Value, max: usize) -> Vec<String> {
    let mut out = vec![];
    fn walk(x: &Value, y: &Value, p: &str, out: &mut Vec<String>, max: usize) {
        if out.len() >= max {
            return;
        }
        match (x, y) {
            (Value::Object(_) | Value::Array(_), Value::Object(_) | Value::Array(_)) => {
                let keys = |v: &Value| -> Vec<String> {
                    match v {
                        Value::Object(o) => o.keys().cloned().collect(),
                        Value::Array(a) => (0..a.len()).map(|i| i.to_string()).collect(),
                        _ => vec![],
                    }
                };
                let mut ks = keys(x);
                for k in keys(y) {
                    if !ks.contains(&k) {
                        ks.push(k);
                    }
                }
                for k in ks {
                    let get = |v: &Value| match v {
                        Value::Object(o) => o.get(&k).cloned().unwrap_or(Value::Null),
                        Value::Array(a) => k.parse::<usize>().ok().and_then(|i| a.get(i).cloned()).unwrap_or(Value::Null),
                        _ => Value::Null,
                    };
                    walk(&get(x), &get(y), &if p.is_empty() { k.clone() } else { format!("{p}.{k}") }, out, max);
                }
            }
            _ => {
                if crate::js::stringify(x) != crate::js::stringify(y) {
                    let s = |v: &Value| crate::js::stringify(v).chars().take(60).collect::<String>();
                    out.push(format!("{p}: {} → {}", s(x), s(y)));
                }
            }
        }
    }
    walk(a, b, "", &mut out, max);
    out
}

pub fn save_baseline(data_dir: &Path, r: &Value) -> Result<String> {
    let text = crate::js::stringify(r);
    std::fs::write(slim_dir(data_dir).join(BASELINE), &text)?;
    Ok(Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect())
}
pub fn read_baseline(data_dir: &Path) -> Result<Value> {
    Ok(serde_json::from_str(&std::fs::read_to_string(slim_dir(data_dir).join(BASELINE))?)?)
}

// ------------------------------------------------------------------------------------------- rolling back, keeping

/// The cache entries back from the backup, each as it was; checked entry by entry.
pub fn restore_entries(data_dir: &Path, cache_root: &Path, tick: &dyn Fn(&str)) -> Result<i64> {
    let file = slim_dir(data_dir).join(ENTRIES);
    if !file.exists() {
        return Ok(0);
    }
    let src = Connection::open_with_flags(&file, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let store = CacheStore::open_default(cache_root)?;
    let total: i64 = src.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))?;
    let (mut last, mut done) = (0i64, 0i64);
    tick(&format!("putting the cache entries back: 0 of {}", crate::js::thousands(total as f64)));
    loop {
        let mut st = src.prepare_cached("SELECT rowid, kind, scope, key, stored_at, data FROM entries WHERE rowid > ? ORDER BY rowid LIMIT ?")?;
        let rows: Vec<(i64, String, String, String, i64, Vec<u8>)> = st.query_map(params![last, RESTORE_BATCH], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.flatten().collect();
        if rows.is_empty() {
            break;
        }
        store.db.execute_batch("BEGIN IMMEDIATE")?;
        let r: Result<()> = rows.iter().try_for_each(|r| store.put(&r.1, &r.2, &r.3, &r.5, r.4));
        match r {
            Ok(()) => store.db.execute_batch("COMMIT")?,
            Err(e) => {
                let _ = store.db.execute_batch("ROLLBACK");
                return Err(e);
            }
        }
        for r in &rows {
            let back: Option<Vec<u8>> = store.db.query_row("SELECT data FROM entries WHERE kind = ? AND scope = ? AND key = ?", params![r.1, r.2, r.3], |x| x.get(0)).ok();
            if back.as_deref() != Some(&r.5[..]) {
                return Err(anyhow!("The cache entry {} {} did not come back as it was", r.1, r.3));
            }
        }
        last = rows.last().unwrap().0;
        done += rows.len() as i64;
        tick(&format!("putting the cache entries back: {} of {}", crate::js::thousands(done as f64), crate::js::thousands(total as f64)));
    }
    Ok(done)
}

/// The rollback: the database file replaced by the backup's copy (checked by SHA-256) and the cache entries put back.
/// Nothing may have the database open.
pub fn rollback_file(data_dir: &Path, cache_root: &Path, m: &Value, tick: &dyn Fn(&str)) -> Result<(i64, String)> {
    let dir = slim_dir(data_dir);
    let file = m["db"]["file"].as_str().ok_or_else(|| anyhow!("No database copy to restore from"))?;
    let want = m["db"]["sha256"].as_str().unwrap_or("").to_string();
    let target = data_dir.join("irelia.db");
    let tmp = data_dir.join("irelia.db.restore");
    tick(&format!("copying the database's backup ({} MB)", crate::js::thousands(mb(m["db"]["bytes"].as_f64().unwrap_or(0.0)))));
    std::fs::copy(dir.join(file), &tmp)?;
    let check = |what: &str| {
        let w = what.to_string();
        move |done: f64, total: f64| tick(&format!("{w}: {} of {} MB", crate::js::thousands(mb(done)), crate::js::thousands(mb(total))))
    };
    if sha256_file(&tmp, &mut check("checking the copy (SHA-256)"))? != want {
        return Err(anyhow!("The backup copy of the database does not match its checksum: nothing was restored"));
    }
    for f in ["irelia.db-wal", "irelia.db-shm"] {
        let _ = std::fs::remove_file(data_dir.join(f));
    }
    std::fs::rename(&tmp, &target)?;
    let restored = sha256_file(&target, &mut check("checking the restored database (SHA-256)"))?;
    if restored != want {
        return Err(anyhow!("The restored database does not match the backup"));
    }
    Ok((restore_entries(data_dir, cache_root, tick)?, restored))
}

/// Roll back, chosen later: the archived rows put back row by row (newer games stay), and the cache entries.
pub fn rollback_rows(data_dir: &Path, cache_root: &Path, db: &Db) -> Result<(i64, i64, i64)> {
    let dir = slim_dir(data_dir);
    let list: Vec<String> = serde_json::from_str(&std::fs::read_to_string(dir.join(ARCHIVE_LIST))?)?;
    let copy = Connection::open_with_flags(dir.join(DB_COPY), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let list_json = serde_json::to_string(&list)?;
    let rows_of = |c: &Connection, ids: &str| -> Result<Vec<(String, Vec<u8>)>> {
        let mut st = c.prepare("SELECT match_id, data FROM archive WHERE match_id IN (SELECT value FROM json_each(?))")?;
        let r = st.query_map([ids], |r| Ok((r.get(0)?, r.get(1)?)))?.flatten().collect();
        Ok(r)
    };
    let digest = |rows: &mut Vec<(String, Vec<u8>)>| -> String {
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        let mut h = Sha256::new();
        for (id, d) in rows.iter() {
            h.update(id.as_bytes());
            h.update(d);
        }
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    };
    let from_copy = rows_of(&copy, &list_json)?;
    let present: HashSet<String> = rows_of(&db.conn, &list_json)?.into_iter().map(|r| r.0).collect();
    let mut back: Vec<(String, Vec<u8>)> = from_copy.iter().filter(|r| present.contains(&r.0)).cloned().collect();
    db.tx(|| {
        for (id, d) in &back {
            db.conn.execute("UPDATE archive SET data = ? WHERE match_id = ?", params![d, id])?;
        }
        Ok(())
    })?;
    let ids: Vec<String> = back.iter().map(|r| r.0.clone()).collect();
    let mut now = rows_of(&db.conn, &serde_json::to_string(&ids)?)?;
    if digest(&mut now) != digest(&mut back) {
        return Err(anyhow!("The archived matches did not come back as they were"));
    }
    let entries = restore_entries(data_dir, cache_root, &|_| {})?;
    Ok((entries, back.len() as i64, (from_copy.len() - back.len()) as i64))
}

pub fn remove_backup(data_dir: &Path) {
    let _ = std::fs::remove_dir_all(slim_dir(data_dir));
}

pub fn backup_bytes(data_dir: &Path) -> f64 {
    std::fs::read_dir(slim_dir(data_dir)).map(|rd| rd.flatten().filter(|e| e.path().is_file()).map(|e| file_size(&e.path()) as f64).sum()).unwrap_or(0.0)
}

/// At start, before anything opens the database: a run cut short. Before the slimming nothing changed (the backup
/// goes); from it on it is rolled back.
pub fn recover_slim(data_dir: &Path, cache_root: &Path, tick: &dyn Fn(&str)) -> Result<Option<Value>> {
    let Some(m) = read_manifest(data_dir) else { return Ok(None) };
    let step = m["step"].as_str().unwrap_or("");
    if ["verified", "kept", "rolled-back", "cancelled"].contains(&step) {
        return Ok(Some(m));
    }
    if step == "backup" || step == "baseline" {
        restore_entries(data_dir, cache_root, tick)?;
        remove_backup(data_dir);
        let mut done = m.clone();
        done["step"] = json!("cancelled");
        done["note"] = json!("Interrupted before slimming: nothing was changed.");
        done["updatedAt"] = json!(crate::util::now_ms());
        return Ok(Some(done));
    }
    let (entries, _) = rollback_file(data_dir, cache_root, &m, tick)?;
    remove_backup(data_dir);
    let mut done = m.clone();
    done["step"] = json!("rolled-back");
    done["note"] = json!(format!("Interrupted while slimming: rolled back (database checked by SHA-256, {} cache entries put back).", crate::js::thousands(entries as f64)));
    done["updatedAt"] = json!(crate::util::now_ms());
    Ok(Some(done))
}
