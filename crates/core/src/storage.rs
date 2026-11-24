//! Port of `server/storage.ts`: the data folder's layout, its stats, and moving it (copy, verify, delete).
//!
//!   irelia.db (+ -wal / -shm)   SQLite
//!   ddragon/<version>/          Data Dragon JSON per patch
//!   cache/riot-cache.db         Riot API responses, compressed
//!   cache/<kind>/<platform>/    gzipped Riot API responses from before 0.14.0, until they are converted
//!   backup-0.14.0/              Slim stored data's backup
//! Only these entries belong to the app: a move copies exactly these and cleanup deletes exactly these.

use crate::config::data_paths;
use crate::riot::disk_cache::{DiskCache, CACHE_DB};
use crate::util::{self, measure, FileEntry};
use anyhow::{anyhow, bail, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const DB_FILE: &str = "irelia.db";
const DB_SIDECARS: [&str; 2] = ["irelia.db-wal", "irelia.db-shm"];
const DIRS: [&str; 3] = ["ddragon", "cache", "backup-0.14.0"];
pub const HEADROOM: u64 = 64 * 1024 * 1024;
const MAX_DIR_CHARS: usize = 140;

fn owned() -> Vec<&'static str> {
    let mut v = vec![DB_FILE];
    v.extend(DB_SIDECARS);
    v.extend(DIRS);
    v
}

fn cache_db_files() -> [String; 3] {
    [CACHE_DB.to_string(), format!("{CACHE_DB}-wal"), format!("{CACHE_DB}-shm")]
}

fn norm(p: &Path) -> String {
    let r = util::resolve(p).to_string_lossy().into_owned();
    if cfg!(windows) {
        r.to_lowercase()
    } else {
        r
    }
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    norm(a) == norm(b)
}

/// True when `child` is strictly inside `parent`.
pub fn is_inside(child: &Path, parent: &Path) -> bool {
    let c = norm(child);
    let p = norm(parent);
    let sep = std::path::MAIN_SEPARATOR;
    let p = p.trim_end_matches(sep).to_string();
    c.len() > p.len() && c.starts_with(&p) && c[p.len()..].starts_with(sep)
}

fn nearest_existing(p: &Path) -> Option<PathBuf> {
    let mut dir = util::resolve(p);
    while !dir.exists() {
        let up = dir.parent()?.to_path_buf();
        if up == dir {
            return None;
        }
        dir = up;
    }
    Some(dir)
}

/// Free and total bytes of the drive holding `p` (or its nearest existing parent).
pub fn free_space(p: &Path) -> Option<(u64, u64)> {
    let base = nearest_existing(p)?;
    Some((fs2::available_space(&base).ok()?, fs2::total_space(&base).ok()?))
}

fn writable(p: &Path) -> bool {
    let Some(base) = nearest_existing(p) else { return false };
    let probe = base.join(format!(".irelia-write-test-{}", std::process::id()));
    if std::fs::write(&probe, b"").is_err() {
        return false;
    }
    let _ = std::fs::remove_file(&probe);
    true
}

fn size_of(dir: &Path, names: &[String]) -> Vec<(String, u64, u64)> {
    names
        .iter()
        .map(|n| {
            let (b, f) = measure(&dir.join(n));
            (n.clone(), f, b)
        })
        .collect()
}

fn read_dir_names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

pub struct StatsOptions {
    pub default_data_dir: PathBuf,
    pub config_file: PathBuf,
    pub locked_by: Option<String>,
    pub ddragon_version: String,
    pub busy: bool,
}

/// StorageStats (shared/types.ts).
pub fn storage_stats(data_dir: &Path, o: &StatsOptions) -> Result<Value> {
    let p = data_paths(data_dir);
    let versions = read_dir_names(&p.ddragon_cache_dir);
    let own = owned();
    let others: Vec<String> = read_dir_names(data_dir).into_iter().filter(|n| !own.contains(&n.as_str())).collect();
    let category = |key: &str, label: &str, parts: Vec<(String, u64, u64)>| -> Value {
        let mut kept: Vec<(String, u64, u64)> = parts.into_iter().filter(|x| x.1 > 0 || x.2 > 0).collect();
        kept.sort_by(|a, b| b.2.cmp(&a.2));
        let bytes: u64 = kept.iter().map(|x| x.2).sum();
        let files: u64 = kept.iter().map(|x| x.1).sum();
        json!({
            "key": key, "label": label,
            "parts": kept.iter().map(|(n, f, b)| json!({"name": n, "files": f, "bytes": b})).collect::<Vec<_>>(),
            "bytes": bytes, "files": files,
        })
    };
    let mut db_names = vec![DB_FILE.to_string()];
    db_names.extend(DB_SIDECARS.iter().map(|s| s.to_string()));
    let cache_parts: Vec<(String, u64, u64)> =
        DiskCache::usage(&p.riot_cache_dir)?.into_iter().map(|(n, f, b)| (n, f as u64, b as u64)).collect();
    let mut categories = vec![
        category("database", "SQLite database", size_of(data_dir, &db_names)),
        category("riotCache", "Riot API cache", cache_parts),
        category("ddragon", "Data Dragon", size_of(&p.ddragon_cache_dir, &versions)),
        category("other", "Other files", size_of(data_dir, &others)),
        category("backup", "Slim stored data backup", size_of(data_dir, &["backup-0.14.0".to_string()])),
    ];
    categories.retain(|c| c["key"] != "backup" || c["bytes"].as_u64().unwrap_or(0) > 0);
    let total: u64 = categories.iter().map(|c| c["bytes"].as_u64().unwrap_or(0)).sum();
    Ok(json!({
        "dataDir": data_dir.to_string_lossy(),
        "defaultDataDir": o.default_data_dir.to_string_lossy(),
        "configFile": o.config_file.to_string_lossy(),
        "lockedBy": o.locked_by,
        "totalBytes": total,
        "categories": categories,
        "disk": free_space(data_dir).map(|(f, t)| json!({"freeBytes": f, "totalBytes": t})),
        "ddragonVersion": o.ddragon_version,
        "busy": o.busy,
    }))
}

/// Deletes Data Dragon versions other than the one in use (and `also_keep`). Returns (files, bytes).
pub fn prune_ddragon(dir: &Path, keep_version: &str, also_keep: &[String]) -> Result<(u64, u64)> {
    let mut files = 0;
    let mut bytes = 0;
    for v in read_dir_names(dir) {
        if v == keep_version || also_keep.contains(&v) {
            continue;
        }
        let (b, f) = measure(&dir.join(&v));
        util::remove(&dir.join(&v))?;
        files += f;
        bytes += b;
    }
    Ok((files, bytes))
}

/// StorageInspection: pre-flight check for a new data folder.
pub fn inspect_target(raw: &str, current_dir: &Path) -> Value {
    let input = raw.trim();
    let target = util::resolve(if input.is_empty() { "." } else { input });
    let mut problems: Vec<String> = Vec::new();
    let absolute = !input.is_empty() && (Path::new(input).is_absolute() || (cfg!(windows) && input.len() > 2 && &input[1..3] == ":\\"));
    if !absolute {
        problems.push("Choose an absolute folder path, e.g. D:\\Games\\IreliaData.".into());
    } else if same_path(&target, current_dir) {
        problems.push("That is already the data folder.".into());
    } else if is_inside(&target, current_dir) {
        problems.push("The new folder cannot be inside the current data folder.".into());
    } else if is_inside(current_dir, &target) {
        problems.push("The new folder cannot contain the current data folder.".into());
    }
    if target.to_string_lossy().chars().count() > MAX_DIR_CHARS {
        problems.push(format!("Choose a shorter path (at most {MAX_DIR_CHARS} characters): Windows limits full file paths to 260."));
    }
    let st = std::fs::metadata(&target).ok();
    if let Some(m) = &st {
        if !m.is_dir() {
            problems.push("That path is a file, not a folder.".into());
        }
    }
    let has_data = st.as_ref().map(|m| m.is_dir()).unwrap_or(false) && owned().iter().any(|n| target.join(n).exists());
    let can_write = writable(&target);
    if !can_write {
        problems.push("Windows denied write access to that folder.".into());
    }
    let names: Vec<String> = owned().iter().map(|s| s.to_string()).collect();
    let required: u64 = size_of(current_dir, &names).iter().map(|x| x.2).sum();
    let disk = free_space(&target);
    if let Some((free, _)) = disk {
        if free < required + HEADROOM {
            problems.push(format!(
                "Not enough free space: {} needed, {} free.",
                fmt_bytes((required + HEADROOM) as f64),
                fmt_bytes(free as f64)
            ));
        }
    }
    json!({
        "path": target.to_string_lossy(), "exists": st.is_some(), "writable": can_write, "hasData": has_data,
        "freeBytes": disk.map(|d| d.0), "requiredBytes": required, "problems": problems,
    })
}

pub struct CopyResult {
    pub files: Vec<FileEntry>,
    pub total_bytes: u64,
    pub cache_entries: Option<i64>,
}

/// StorageProgress callback: (phase, done bytes, total bytes, text).
pub type Progress<'a> = &'a mut dyn FnMut(&str, u64, u64, &str);

/// Copies the app's entries from `from` to `to` (the databases with VACUUM INTO, cache files with their mtimes).
pub fn copy_data_dir(from: &Path, to: &Path, vacuum_into: &dyn Fn(&Path) -> Result<()>, on_progress: Progress) -> Result<CopyResult> {
    std::fs::create_dir_all(to)?;
    let cache_db = from.join("cache").join(CACHE_DB);
    let cache_files = cache_db_files();
    let mut files: Vec<FileEntry> = Vec::new();
    for dir in DIRS {
        for f in util::walk_files(&from.join(dir)) {
            let in_cache_dir = f.path.parent() == cache_db.parent();
            let name = f.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if !(in_cache_dir && cache_files.contains(&name)) {
                files.push(f);
            }
        }
    }
    let mut db_names = vec![DB_FILE.to_string()];
    db_names.extend(DB_SIDECARS.iter().map(|s| s.to_string()));
    let db_bytes: u64 = size_of(from, &db_names).iter().map(|x| x.2).sum();
    let cache_bytes: u64 = size_of(&from.join("cache"), &cache_files).iter().map(|x| x.2).sum();
    let total_bytes = db_bytes + cache_bytes + files.iter().map(|f| f.size).sum::<u64>();
    let mut done = 0u64;
    if from.join(DB_FILE).exists() {
        on_progress("database", done, total_bytes, "Copying database (VACUUM INTO)…");
        vacuum_into(&to.join(DB_FILE))?;
    }
    done += db_bytes;
    let mut cache_entries = None;
    if cache_db.exists() {
        on_progress("database", done, total_bytes, "Copying the API cache (VACUUM INTO)…");
        std::fs::create_dir_all(to.join("cache"))?;
        let src = Connection::open(&cache_db)?;
        src.execute("VACUUM INTO ?", [to.join("cache").join(CACHE_DB).to_string_lossy()])?;
        cache_entries = Some(src.query_row("SELECT COUNT(*) AS n FROM entries", [], |r| r.get(0))?);
    }
    done += cache_bytes;
    let mut tick = 0i64;
    let n = files.len();
    for (i, f) in files.iter().enumerate() {
        let rel = f.path.strip_prefix(from)?;
        let dest = to.join(rel);
        if let Some(d) = dest.parent() {
            std::fs::create_dir_all(d)?;
        }
        if dest.exists() {
            bail!("EEXIST: file already exists, copyfile '{}'", dest.display());
        }
        std::fs::copy(&f.path, &dest)?;
        let mtime = filetime::FileTime::from_unix_time((f.mtime_ms / 1000.0).floor() as i64, ((f.mtime_ms % 1000.0) * 1e6) as u32);
        filetime::set_file_mtime(&dest, mtime)?;
        done += f.size;
        if util::now_ms() - tick > 150 || i == n - 1 {
            tick = util::now_ms();
            on_progress("copying", done, total_bytes, &format!("Copying cache files {} of {}…", i + 1, n));
        }
    }
    Ok(CopyResult { files, total_bytes, cache_entries })
}

/// Size check of every copied file plus SQLite quick_check and row counts.
pub fn verify_copy(from: &Path, to: &Path, files: &[FileEntry], rows: Option<&[(String, i64)]>, cache_entries: Option<i64>) -> Result<()> {
    for f in files {
        let rel = f.path.strip_prefix(from)?;
        let ok = std::fs::metadata(to.join(rel)).map(|m| m.len() == f.size).unwrap_or(false);
        if !ok {
            bail!("Copy check failed for {}", rel.display());
        }
    }
    if let Some(expected) = cache_entries {
        let c = Connection::open_with_flags(to.join("cache").join(CACHE_DB), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let check: String = c.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if check != "ok" {
            bail!("Copied API cache failed its integrity check: {check}");
        }
        let got: i64 = c.query_row("SELECT COUNT(*) AS n FROM entries", [], |r| r.get(0))?;
        if got != expected {
            bail!("Copied API cache has {got} entries, expected {expected}");
        }
    }
    let Some(rows) = rows else { return Ok(()) };
    let db = Connection::open_with_flags(to.join(DB_FILE), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let check: String = db.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if check != "ok" {
        bail!("Copied database failed its integrity check: {check}");
    }
    for (table, n) in rows {
        let got: i64 = db.query_row(&format!("SELECT COUNT(*) AS n FROM {table}"), [], |r| r.get(0))?;
        if got != *n {
            return Err(anyhow!("Copied database has {got} {table} rows, expected {n}"));
        }
    }
    Ok(())
}

/// Deletes the app's own entries from `dir`, then the folder itself if nothing else is left. Returns what remains.
pub fn remove_owned_data(dir: &Path) -> Result<Vec<String>> {
    for name in owned() {
        util::remove(&dir.join(name))?;
    }
    let left = read_dir_names(dir);
    if left.is_empty() {
        let _ = std::fs::remove_dir(dir);
    }
    Ok(left)
}

pub fn fmt_bytes(n: f64) -> String {
    let u = ["B", "KB", "MB", "GB", "TB"];
    let mut n = n;
    let mut i = 0;
    while n >= 1024.0 && i < u.len() - 1 {
        n /= 1024.0;
        i += 1;
    }
    format!("{} {}", crate::js::to_fixed(n, if i > 0 && n < 10.0 { 1 } else { 0 }), u[i])
}
