//! Export and import: everything Build Fetcher has learned, as one zip, to move it to another PC or keep a copy.
//!
//! The archive holds:
//!
//! | entry | what |
//! |---|---|
//! | `manifest.json` | `{ app: "build-fetcher", format, version, createdMs, includesKey, files }` |
//! | `settings.json` | the settings; the Riot API key only when the user asked for it (`includeKey`) |
//! | `data/build-fetcher.db` | a consistent snapshot of the database (`VACUUM INTO`, taken while the app runs) |
//! | `data/checkpoints/<file>` | the learner's checkpoints, manifest and log |
//!
//! Data Dragon and the logs are left out: the first is downloaded again, the second is this PC's.
//!
//! Import never swaps files under the running app. `stage_import` checks the archive and unpacks it into
//! `<settings folder>/import-pending/`. At the next start, before the database opens, `apply_pending_import` moves the
//! current data to `<settings folder>/before-import/` (one copy, replaced by the next import, so a wrong import can be
//! undone by hand), puts the imported data in place and merges the settings. When the archive has no key, the key
//! already on this PC is kept.
//!
//! What is checked before anything is staged:
//! - the manifest says Build Fetcher, a known format, and no newer database schema;
//! - only the entries above, with plain file names (no folders, no `..`: nothing can be written outside the staging
//!   folder);
//! - every entry's CRC-32 (the zip reader);
//! - the database passes SQLite's `PRAGMA integrity_check`;
//! - the settings are a JSON object.

use crate::settings::KEY_FIELD;
use crate::store::Store;
use anyhow::{anyhow, bail, Context, Result};
use bf_core::zip::{extract_entry, read_entry, read_zip, ZipWriter};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub const APP: &str = "build-fetcher";
/// The archive layout's version. A newer one is refused by an older app.
pub const FORMAT: u32 = 1;
pub const PENDING_DIR: &str = "import-pending";
pub const BEFORE_DIR: &str = "before-import";
const DB_ENTRY: &str = "data/build-fetcher.db";
const CHECKPOINTS_PREFIX: &str = "data/checkpoints/";
/// Settings this PC keeps whatever the archive says: where its data lives is this PC's business.
/// Settings that belong to this PC and never travel in a backup: the data folder, and the extra API keys (a backup
/// carries at most the main key, and only when asked; importing keeps this PC's extra keys).
const LOCAL_ONLY_SETTINGS: [&str; 2] = ["dataDir", crate::settings::EXTRA_KEYS_FIELD];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub app: String,
    pub format: u32,
    /// The Build Fetcher version that wrote it.
    pub version: String,
    pub created_ms: i64,
    pub includes_key: bool,
    /// (entry, bytes).
    pub files: Vec<(String, u64)>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportReport {
    pub path: PathBuf,
    /// The archive's size.
    pub bytes: u64,
    pub includes_key: bool,
    pub files: usize,
    pub millis: u64,
}

/// What a staged import holds (shown before the restart that applies it).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSummary {
    pub version: String,
    pub created_ms: i64,
    pub includes_key: bool,
    pub database_bytes: u64,
    pub checkpoint_files: usize,
}

/// What `apply_pending_import` did.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportApplied {
    pub summary: ImportSummary,
    /// Where the data that was replaced now is.
    pub previous: PathBuf,
}

/// A default file name for an export: build-fetcher-backup-2026-11-25.zip.
pub fn default_file_name() -> String {
    format!("build-fetcher-backup-{}.zip", chrono::Local::now().format("%Y-%m-%d"))
}

/// The settings as exported: the key removed unless asked for, local-only settings removed.
fn exported_settings(settings_file: &Path, include_key: bool) -> (Map<String, Value>, bool) {
    let mut s = bf_core::app_config::read_config_at(settings_file);
    for k in LOCAL_ONLY_SETTINGS {
        s.shift_remove(k);
    }
    let has_key = s.get(KEY_FIELD).and_then(|v| v.as_str()).map(|k| !k.trim().is_empty()).unwrap_or(false);
    if !include_key {
        s.shift_remove(KEY_FIELD);
    }
    (s, include_key && has_key)
}

/// Writes the archive to `out`. The database snapshot is taken through the store's own connection, so fetching and
/// training can go on meanwhile.
pub fn export(store: &Store, settings_file: &Path, checkpoints_dir: &Path, out: &Path, include_key: bool) -> Result<ExportReport> {
    let t0 = std::time::Instant::now();
    if out.extension().and_then(|e| e.to_str()).map(|e| !e.eq_ignore_ascii_case("zip")).unwrap_or(true) {
        bail!("The backup file must end in .zip");
    }
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() && !parent.is_dir() {
            bail!("The folder {} does not exist", parent.display());
        }
    }
    // The snapshot goes next to the database (same disk), under a name nothing else uses, and is removed after.
    let snap = store.dir.join(format!(".export-{}-{}.db", std::process::id(), bf_core::util::now_ms()));
    let _cleanup = RemoveOnDrop(snap.clone());
    store.snapshot_to(&snap).context("Snapshot of the database")?;

    let (settings, includes_key) = exported_settings(settings_file, include_key);
    let settings_raw = serde_json::to_vec_pretty(&Value::Object(settings))?;
    let mut files: Vec<(String, PathBuf)> = vec![];
    if let Ok(rd) = std::fs::read_dir(checkpoints_dir) {
        let mut names: Vec<String> = rd.filter_map(|e| e.ok()).filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false)).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        for n in names {
            // Half-written files (write_atomic's temporary names) are skipped: the finished ones are what count.
            if is_plain_name(&n) && !n.ends_with(".tmp") {
                files.push((format!("{CHECKPOINTS_PREFIX}{n}"), checkpoints_dir.join(&n)));
            }
        }
    }
    let mut listing = vec![("settings.json".to_string(), settings_raw.len() as u64), (DB_ENTRY.to_string(), std::fs::metadata(&snap)?.len())];
    for (name, path) in &files {
        listing.push((name.clone(), std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)));
    }
    let manifest = Manifest { app: APP.into(), format: FORMAT, version: env!("CARGO_PKG_VERSION").into(), created_ms: bf_core::util::now_ms(), includes_key, files: listing };

    let mut w = ZipWriter::new(out);
    let r = (|| -> Result<()> {
        w.add_buffer("manifest.json", &serde_json::to_vec_pretty(&manifest)?)?;
        w.add_buffer("settings.json", &settings_raw)?;
        w.add_file(DB_ENTRY, &snap, &mut |_| {})?;
        for (name, path) in &files {
            w.add_file(name, path, &mut |_| {})?;
        }
        Ok(())
    })();
    if let Err(e) = r {
        w.abort();
        return Err(e);
    }
    let (path, bytes) = w.finish()?;
    Ok(ExportReport { path, bytes, includes_key, files: manifest.files.len() + 1, millis: t0.elapsed().as_millis() as u64 })
}

/// A file name without folders, drive letters or dot-dot.
fn is_plain_name(n: &str) -> bool {
    !n.is_empty() && n != "." && n != ".." && !n.contains(['/', '\\', ':']) && !n.starts_with('.')
}

/// The archive's manifest, checked.
fn read_manifest(zip: &Path, entries: &[bf_core::zip::ZipEntry]) -> Result<Manifest> {
    let e = entries.iter().find(|e| e.name == "manifest.json").ok_or_else(|| anyhow!("This is not a Build Fetcher backup (no manifest.json)"))?;
    let m: Manifest = serde_json::from_slice(&read_entry(zip, e)?).map_err(|_| anyhow!("This is not a Build Fetcher backup (unreadable manifest.json)"))?;
    if m.app != APP {
        bail!("This is not a Build Fetcher backup (it says {:?})", m.app);
    }
    if m.format > FORMAT {
        bail!("This backup was made by a newer Build Fetcher ({}, format {}); update this one first", m.version, m.format);
    }
    Ok(m)
}

/// Checks `zip` and unpacks it into `<config_dir>/import-pending/` (replacing an import staged before). Nothing in use
/// is touched; the import takes effect at the next start.
pub fn stage_import(zip: &Path, config_dir: &Path) -> Result<ImportSummary> {
    let entries = read_zip(zip).map_err(|e| anyhow!("Not a readable zip file: {e}"))?;
    let manifest = read_manifest(zip, &entries)?;
    // Only the known entries, with plain names.
    for e in &entries {
        let ok = matches!(e.name.as_str(), "manifest.json" | "settings.json" | DB_ENTRY)
            || e.name.strip_prefix(CHECKPOINTS_PREFIX).map(is_plain_name).unwrap_or(false);
        if !ok {
            bail!("This backup holds an unexpected entry ({:?}); it was not imported", e.name);
        }
    }
    let db = entries.iter().find(|e| e.name == DB_ENTRY).ok_or_else(|| anyhow!("This backup has no database"))?;
    let partial = config_dir.join(format!("{PENDING_DIR}.partial"));
    bf_core::util::remove(&partial)?;
    std::fs::create_dir_all(partial.join("checkpoints"))?;
    let r = (|| -> Result<ImportSummary> {
        let mut checkpoint_files = 0;
        for e in &entries {
            let dest = match e.name.as_str() {
                "manifest.json" => partial.join("manifest.json"),
                "settings.json" => partial.join("settings.json"),
                DB_ENTRY => partial.join("build-fetcher.db"),
                n => {
                    checkpoint_files += 1;
                    partial.join("checkpoints").join(&n[CHECKPOINTS_PREFIX.len()..])
                }
            };
            extract_entry(zip, e, &dest, &mut |_| {})?;
        }
        check_database(&partial.join("build-fetcher.db"))?;
        if partial.join("settings.json").exists() {
            let raw = std::fs::read(partial.join("settings.json"))?;
            match serde_json::from_slice::<Value>(&raw) {
                Ok(Value::Object(_)) => {}
                _ => bail!("The backup's settings.json is not a settings file"),
            }
        }
        Ok(ImportSummary { version: manifest.version.clone(), created_ms: manifest.created_ms, includes_key: manifest.includes_key, database_bytes: db.size, checkpoint_files })
    })();
    match r {
        Ok(summary) => {
            let pending = config_dir.join(PENDING_DIR);
            bf_core::util::remove(&pending)?;
            std::fs::rename(&partial, &pending)?;
            Ok(summary)
        }
        Err(e) => {
            let _ = bf_core::util::remove(&partial);
            Err(e)
        }
    }
}

/// SQLite's own check, on a read-only open; and no schema newer than this app reads.
fn check_database(path: &Path) -> Result<()> {
    let db = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| anyhow!("The backup's database can't be opened: {e}"))?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(|e| anyhow!("The backup's database can't be read: {e}"))?;
    if version > crate::db::SCHEMA_VERSION {
        bail!("The backup's database was written by a newer Build Fetcher (schema {version}); update this one first");
    }
    let check: String = db.query_row("PRAGMA integrity_check", [], |r| r.get(0)).map_err(|e| anyhow!("The backup's database can't be checked: {e}"))?;
    if check != "ok" {
        bail!("The backup's database is damaged ({check})");
    }
    Ok(())
}

/// The import staged and waiting for a restart, if any.
pub fn pending_import(config_dir: &Path) -> Option<ImportSummary> {
    let dir = config_dir.join(PENDING_DIR);
    let m: Manifest = serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).ok()?).ok()?;
    let db = std::fs::metadata(dir.join("build-fetcher.db")).ok()?;
    let checkpoint_files = std::fs::read_dir(dir.join("checkpoints")).map(|rd| rd.count()).unwrap_or(0);
    Some(ImportSummary { version: m.version, created_ms: m.created_ms, includes_key: m.includes_key, database_bytes: db.len(), checkpoint_files })
}

pub fn cancel_import(config_dir: &Path) -> Result<()> {
    bf_core::util::remove(&config_dir.join(PENDING_DIR))?;
    Ok(())
}

/// At start, before the database or the learner opens anything: puts a staged import in place. Returns None when
/// nothing was staged.
pub fn apply_pending_import(config_dir: &Path, data_dir: &Path, settings_file: &Path) -> Result<Option<ImportApplied>> {
    let pending = config_dir.join(PENDING_DIR);
    let Some(summary) = pending_import(config_dir) else {
        // A half-staged or broken folder is nothing to apply.
        if pending.exists() {
            bf_core::util::remove(&pending)?;
        }
        return Ok(None);
    };
    // The data in use goes aside (one copy: the previous one is dropped).
    let before = config_dir.join(BEFORE_DIR);
    bf_core::util::remove(&before)?;
    std::fs::create_dir_all(&before)?;
    for name in ["build-fetcher.db", "build-fetcher.db-wal", "build-fetcher.db-shm", "checkpoints"] {
        let p = data_dir.join(name);
        if p.exists() {
            std::fs::rename(&p, before.join(name)).with_context(|| format!("Moving {} aside", p.display()))?;
        }
    }
    if settings_file.exists() {
        std::fs::copy(settings_file, before.join("settings.json"))?;
    }
    std::fs::create_dir_all(data_dir)?;
    std::fs::rename(pending.join("build-fetcher.db"), data_dir.join("build-fetcher.db"))?;
    if pending.join("checkpoints").is_dir() {
        std::fs::rename(pending.join("checkpoints"), data_dir.join("checkpoints"))?;
    }
    // Settings: the archive's replace this PC's, except this PC's own (data folder) and the key when the archive
    // has none.
    if let Ok(raw) = std::fs::read(pending.join("settings.json")) {
        if let Ok(Value::Object(imported)) = serde_json::from_slice::<Value>(&raw) {
            let current = bf_core::app_config::read_config_at(settings_file);
            let mut next = imported;
            for k in LOCAL_ONLY_SETTINGS {
                next.shift_remove(k);
                if let Some(v) = current.get(k) {
                    next.insert(k.into(), v.clone());
                }
            }
            let imported_key = next.get(KEY_FIELD).and_then(|v| v.as_str()).map(|k| !k.trim().is_empty()).unwrap_or(false);
            if !imported_key {
                next.shift_remove(KEY_FIELD);
                if let Some(v) = current.get(KEY_FIELD) {
                    next.insert(KEY_FIELD.into(), v.clone());
                }
            }
            let text = format!("{}\n", serde_json::to_string_pretty(&Value::Object(next))?);
            bf_core::util::write_atomic(settings_file, text.as_bytes())?;
        }
    }
    bf_core::util::remove(&pending)?;
    Ok(Some(ImportApplied { summary, previous: before }))
}

struct RemoveOnDrop(PathBuf);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
