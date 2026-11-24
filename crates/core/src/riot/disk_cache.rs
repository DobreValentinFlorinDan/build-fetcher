//! Port of `server/riot/diskCache.ts`: the Riot API cache. Compressed JSON responses in
//! `<data>/cache/riot-cache.db` (one row per response, zstd with a dictionary made from a match and a timeline of
//! the cache), and until they are converted, the gzip files of versions before 0.14.0 under
//! `<data>/cache/<kind>/<scope>/<key>.json.gz`.

use crate::codec::{pack, unpack, Dictionaries, Dictionary};
use crate::db::gunzip;
use crate::js;
use crate::util::{self, now_ms};
use anyhow::Result;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

pub const KINDS: [&str; 6] = ["match", "timeline", "league", "mastery", "matchIds", "account"];

/// How long each kind of Riot response stays valid on disk (None: forever).
pub fn cache_ttl(kind: &str) -> Option<i64> {
    match kind {
        "league" => Some(6 * 3600_000),
        "mastery" => Some(3 * 24 * 3600_000),
        "matchIds" => Some(10 * 60_000),
        "account" => Some(7 * 24 * 3600_000),
        _ => None,
    }
}

pub const CACHE_DB: &str = "riot-cache.db";
pub fn cache_db_file(root: &Path) -> PathBuf {
    root.join(CACHE_DB)
}

/// A key as it was a file name (before 0.14.0): /[^\w.-]/g → '_'.
pub fn key_of(key: &str) -> String {
    key.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' { c } else { '_' }).collect()
}

/// Where an entry was before 0.14.0.
pub fn legacy_file(root: &Path, kind: &str, scope: &str, key: &str) -> PathBuf {
    root.join(kind).join(scope).join(format!("{}.json.gz", key_of(key)))
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS entries (
  kind       TEXT NOT NULL,
  scope      TEXT NOT NULL,               -- platform or region
  key        TEXT NOT NULL,
  stored_at  INTEGER NOT NULL,            -- epoch ms: the TTL counts from it
  data       BLOB NOT NULL,               -- the JSON response, compressed (server/codec.ts)
  PRIMARY KEY (kind, scope, key)
);
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
";
const SCHEMA_VERSION: i64 = 1;
const SIZE_INDEX: &str = "CREATE INDEX IF NOT EXISTS entries_size ON entries(kind, scope, key, length(data))";
const SIZE_INDEX_VERSION: i64 = 2;
const SET_VERSION: &str = "INSERT INTO meta(key, value) VALUES('schema_version', ?)
  ON CONFLICT(key) DO UPDATE SET value = CAST(MAX(CAST(value AS INTEGER), CAST(excluded.value AS INTEGER)) AS TEXT)";

#[derive(Clone, Debug)]
pub struct Usage {
    pub kind: String,
    pub files: i64,
    pub bytes: i64,
}

#[derive(Clone, Debug)]
pub struct MatchEntry {
    pub kind: String,
    pub scope: String,
    pub key: String,
    pub bytes: i64,
}

/// The Riot API cache's database.
pub struct CacheStore {
    pub root: PathBuf,
    pub db: Connection,
    pub dicts: Dictionaries,
}

impl CacheStore {
    pub fn open(root: &Path, busy_timeout_ms: u64) -> Result<CacheStore> {
        std::fs::create_dir_all(root)?;
        let db = Connection::open(cache_db_file(root))?;
        db.busy_timeout(std::time::Duration::from_millis(busy_timeout_ms))?;
        db.execute_batch("PRAGMA page_size = 16384; PRAGMA auto_vacuum = INCREMENTAL;")?;
        db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA journal_size_limit = 67108864;")?;
        db.execute_batch("BEGIN IMMEDIATE")?;
        let r = (|| -> Result<()> {
            db.execute_batch(SCHEMA)?;
            db.execute_batch(crate::codec::CODEC_SCHEMA)?;
            db.execute(SET_VERSION, [SCHEMA_VERSION.to_string()])?;
            Ok(())
        })();
        match r {
            Ok(()) => db.execute_batch("COMMIT")?,
            Err(e) => {
                let _ = db.execute_batch("ROLLBACK");
                return Err(e);
            }
        }
        let dicts = Dictionaries::new(&db);
        Ok(CacheStore { root: root.to_path_buf(), db, dicts })
    }

    pub fn open_default(root: &Path) -> Result<CacheStore> {
        CacheStore::open(root, 30_000)
    }

    fn lookup(&self) -> impl Fn(u16) -> Option<Arc<Vec<u8>>> + '_ {
        move |id| self.dicts.get(&self.db, id)
    }

    /// The stored response's JSON bytes and when it was stored, or None.
    pub fn read(&self, kind: &str, scope: &str, key: &str) -> Result<Option<(i64, Vec<u8>)>> {
        let row: Option<(i64, Vec<u8>)> = self
            .db
            .prepare_cached("SELECT stored_at, data FROM entries WHERE kind = ? AND scope = ? AND key = ?")?
            .query_row(params![kind, scope, key_of(key)], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        match row {
            Some((at, data)) => Ok(Some((at, unpack(&data, &self.lookup())?))),
            None => Ok(None),
        }
    }

    /// Stores the JSON bytes `raw` (compressed), stored at `stored_at`. Returns the blob written.
    pub fn write(&self, kind: &str, scope: &str, key: &str, raw: &[u8], stored_at: i64) -> Result<Vec<u8>> {
        let blob = self.encode(kind, raw)?;
        self.put(kind, scope, key, &blob, stored_at)?;
        Ok(blob)
    }

    pub fn decode(&self, blob: &[u8]) -> Result<Vec<u8>> {
        unpack(blob, &self.lookup())
    }
    pub fn encode_check(&self, blob: &[u8], raw: &[u8]) -> bool {
        self.decode(blob).map(|d| d == raw).unwrap_or(false)
    }
    pub fn encode(&self, kind: &str, raw: &[u8]) -> Result<Vec<u8>> {
        let d = self.dictionary(kind, raw)?;
        pack(raw, d.as_ref())
    }
    pub fn put(&self, kind: &str, scope: &str, key: &str, blob: &[u8], stored_at: i64) -> Result<()> {
        self.db
            .prepare_cached(
                "INSERT INTO entries(kind, scope, key, stored_at, data) VALUES(?, ?, ?, ?, ?)
        ON CONFLICT(kind, scope, key) DO UPDATE SET stored_at = excluded.stored_at, data = excluded.data",
            )?
            .execute(params![kind, scope, key_of(key), stored_at, blob])?;
        Ok(())
    }

    /// The dictionary to write with: the current one, else one made from the newest stored match and this timeline
    /// (or this match and the newest timeline). Until then, none.
    fn dictionary(&self, kind: &str, raw: &[u8]) -> Result<Option<Dictionary>> {
        let d = self.dicts.current();
        if d.is_some() || (kind != "match" && kind != "timeline") {
            return Ok(d);
        }
        let other: Option<Vec<u8>> = self
            .db
            .prepare_cached("SELECT data FROM entries WHERE kind = ? ORDER BY stored_at DESC LIMIT 1")?
            .query_row([if kind == "match" { "timeline" } else { "match" }], |r| r.get(0))
            .optional()?;
        let Some(other) = other else { return Ok(None) };
        let other_raw = unpack(&other, &self.lookup())?;
        let samples: [&[u8]; 2] = if kind == "match" { [raw, &other_raw] } else { [&other_raw, raw] };
        Ok(Some(self.dicts.create(&self.db, &samples, "a match and a timeline of this cache")?))
    }

    pub fn ensure_dictionary(&self, m: &[u8], t: &[u8]) -> Result<Dictionary> {
        match self.dicts.current() {
            Some(d) => Ok(d),
            None => self.dicts.create(&self.db, &[m, t], "a match and a timeline of this cache"),
        }
    }

    pub fn delete(&self, kind: &str, scope: &str, key: &str) -> Result<()> {
        self.db.prepare_cached("DELETE FROM entries WHERE kind = ? AND scope = ? AND key = ?")?.execute(params![kind, scope, key_of(key)])?;
        Ok(())
    }

    pub fn usage(&self) -> Result<Vec<Usage>> {
        let mut st = self.db.prepare_cached("SELECT kind, COUNT(*) AS files, SUM(LENGTH(data)) AS bytes FROM entries GROUP BY kind")?;
        let r = st
            .query_map([], |r| Ok(Usage { kind: r.get(0)?, files: r.get(1)?, bytes: r.get::<_, Option<i64>>(2)?.unwrap_or(0) }))?
            .filter_map(|x| x.ok())
            .collect();
        Ok(r)
    }

    pub fn count(&self) -> i64 {
        self.db.query_row("SELECT COUNT(*) AS n FROM entries", [], |r| r.get(0)).unwrap_or(0)
    }

    /// Builds the size index if the file doesn't have it yet: true when it did.
    pub fn ensure_size_index(&self) -> Result<bool> {
        let have = self
            .db
            .prepare("SELECT 1 AS x FROM sqlite_master WHERE type = 'index' AND name = 'entries_size'")?
            .exists([])?;
        if have {
            return Ok(false);
        }
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        let r = (|| -> Result<()> {
            self.db.execute_batch(SIZE_INDEX)?;
            self.db.execute(SET_VERSION, [SIZE_INDEX_VERSION.to_string()])?;
            Ok(())
        })();
        match r {
            Ok(()) => {
                self.db.execute_batch("COMMIT")?;
                Ok(true)
            }
            Err(e) => {
                let _ = self.db.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    pub fn reclaim(&self) -> Result<()> {
        self.db.execute_batch("PRAGMA incremental_vacuum;")?;
        self.db.query_row("PRAGMA wal_checkpoint(TRUNCATE);", [], |_| Ok(())).ok();
        Ok(())
    }

    /// Deletes everything, a batch at a time.
    pub fn clear(&self, on_progress: &mut dyn FnMut(f64)) -> Result<i64> {
        let total = self.count();
        let mut done = 0i64;
        loop {
            let rows: Vec<(String, String, String)> = {
                let mut st = self.db.prepare_cached("SELECT kind, scope, key FROM entries LIMIT ?")?;
                let r = st.query_map([2000], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.filter_map(|x| x.ok()).collect();
                r
            };
            if rows.is_empty() {
                break;
            }
            self.db.execute_batch("BEGIN IMMEDIATE")?;
            for (k, s, key) in &rows {
                if let Err(e) = self.db.prepare_cached("DELETE FROM entries WHERE kind = ? AND scope = ? AND key = ?").and_then(|mut st| st.execute(params![k, s, key])) {
                    let _ = self.db.execute_batch("ROLLBACK");
                    return Err(e.into());
                }
            }
            self.db.execute_batch("COMMIT")?;
            done += rows.len() as i64;
            on_progress(if total > 0 { done as f64 / total as f64 } else { 1.0 });
        }
        self.reclaim()?;
        Ok(done)
    }

    /// Deletes the entries past their TTL.
    pub fn clear_expired(&self, now: i64) -> Result<i64> {
        let mut files = 0i64;
        for kind in KINDS {
            if let Some(ttl) = cache_ttl(kind) {
                files += self.db.prepare_cached("DELETE FROM entries WHERE kind = ? AND stored_at < ?")?.execute(params![kind, now - ttl])? as i64;
            }
        }
        if files > 0 {
            self.reclaim()?;
        }
        Ok(files)
    }

    /// Every cached match and timeline: kind, scope, key and compressed size.
    pub fn match_entries(&self) -> Result<Vec<MatchEntry>> {
        let mut st = self.db.prepare_cached("SELECT kind, scope, key, LENGTH(data) AS bytes FROM entries WHERE kind IN ('match', 'timeline')")?;
        let r = st
            .query_map([], |r| Ok(MatchEntry { kind: r.get(0)?, scope: r.get(1)?, key: r.get(2)?, bytes: r.get(3)? }))?
            .filter_map(|x| x.ok())
            .collect();
        Ok(r)
    }
}

pub type SharedStore = Arc<Mutex<CacheStore>>;

static SHARED: LazyLock<Mutex<HashMap<PathBuf, SharedStore>>> = LazyLock::new(Default::default);

/// The main thread's stores, one per folder (Storage card, cache cleanup); closed before the data folder moves.
pub fn cache_store(root: &Path) -> Result<SharedStore> {
    let mut m = SHARED.lock();
    if let Some(s) = m.get(root) {
        return Ok(s.clone());
    }
    let s = Arc::new(Mutex::new(CacheStore::open(root, 5000)?));
    m.insert(root.to_path_buf(), s.clone());
    Ok(s)
}

pub fn close_cache_stores() {
    SHARED.lock().clear();
}

/// Whether the folder still has cache files from before 0.14.0 (any kind's folder).
fn has_legacy_files(root: &Path) -> bool {
    KINDS.iter().any(|k| root.join(k).is_dir())
}

/// The Riot API cache as the client reads and writes it.
pub struct DiskCache {
    pub hits: u64,
    pub misses: u64,
    root: PathBuf,
    store: Option<CacheStore>,
    legacy: Option<(bool, i64)>,
}

impl DiskCache {
    pub fn new(root: &Path) -> DiskCache {
        DiskCache { hits: 0, misses: 0, root: root.to_path_buf(), store: None, legacy: None }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn db(&mut self) -> Result<&CacheStore> {
        if self.store.is_none() {
            self.store = Some(CacheStore::open_default(&self.root)?);
        }
        Ok(self.store.as_ref().unwrap())
    }

    pub fn ensure_size_index(&mut self) -> Result<bool> {
        self.db()?.ensure_size_index()
    }

    fn any_legacy(&mut self) -> bool {
        let now = now_ms();
        let stale = match self.legacy {
            None => true,
            Some((any, at)) => any && now - at > 5 * 60_000,
        };
        if stale {
            self.legacy = Some((has_legacy_files(&self.root), now));
        }
        self.legacy.unwrap().0
    }

    /// A cached answer, unless it expired or (`since`, 0.21.2) was stored before that moment.
    pub fn get(&mut self, kind: &str, scope: &str, key: &str, since: Option<i64>) -> Option<Value> {
        let r = self.get_inner(kind, scope, key, since);
        match r {
            Ok(Some(v)) => {
                self.hits += 1;
                Some(v)
            }
            _ => {
                self.misses += 1;
                None
            }
        }
    }

    fn get_inner(&mut self, kind: &str, scope: &str, key: &str, since: Option<i64>) -> Result<Option<Value>> {
        let since_v = since.map(|s| s as f64).unwrap_or(f64::NEG_INFINITY);
        let ttl = cache_ttl(kind);
        if let Some((stored_at, raw)) = self.db()?.read(kind, scope, key)? {
            if let Some(t) = ttl {
                if now_ms() - stored_at > t {
                    return Ok(None);
                }
            }
            if (stored_at as f64) < since_v {
                return Ok(None);
            }
            return Ok(Some(serde_json::from_slice(&raw)?));
        }
        if self.any_legacy() {
            let file = legacy_file(&self.root, kind, scope, key);
            if ttl.is_some() || since.is_some() {
                let m = std::fs::metadata(&file)?;
                let mtime = util::mtime_ms(&m);
                let expired = match ttl {
                    Some(t) => now_ms() as f64 - mtime > t as f64,
                    None => false,
                };
                if expired || mtime < since_v {
                    return Ok(None);
                }
            }
            let value: Value = serde_json::from_slice(&gunzip(&std::fs::read(&file)?)?)?;
            return Ok(Some(value));
        }
        Ok(None)
    }

    pub fn set(&mut self, kind: &str, scope: &str, key: &str, value: &Value) -> Result<()> {
        let raw = js::stringify(value);
        self.db()?.write(kind, scope, key, raw.as_bytes(), now_ms())?;
        if self.any_legacy() {
            let _ = std::fs::remove_file(legacy_file(&self.root, kind, scope, key));
        }
        Ok(())
    }

    pub fn close(&mut self) {
        self.store = None;
    }

    /// Disk usage per cache kind: the database's entries and the old files left.
    pub fn usage(root: &Path) -> Result<Vec<(String, i64, i64)>> {
        let store = cache_store(root)?;
        let in_db: HashMap<String, Usage> = store.lock().usage()?.into_iter().map(|u| (u.kind.clone(), u)).collect();
        let mut parts: Vec<(String, i64, i64)> = KINDS
            .iter()
            .map(|kind| {
                let (bytes, files) = util::measure(&root.join(kind));
                let db = in_db.get(*kind);
                (kind.to_string(), files as i64 + db.map(|d| d.files).unwrap_or(0), bytes as i64 + db.map(|d| d.bytes).unwrap_or(0))
            })
            .collect();
        let blobs: i64 = in_db.values().map(|u| u.bytes).sum();
        let file: i64 = ["", "-wal"].iter().map(|x| util::measure(&PathBuf::from(format!("{}{}", cache_db_file(root).display(), x))).0 as i64).sum();
        if file > blobs {
            parts.push(("index and free space".into(), 0, file - blobs));
        }
        Ok(parts.into_iter().filter(|p| p.1 > 0 || p.2 > 0).collect())
    }

    fn db_bytes(root: &Path) -> i64 {
        util::measure(&cache_db_file(root)).0 as i64 + util::measure(&PathBuf::from(format!("{}-wal", cache_db_file(root).display()))).0 as i64
    }

    /// Deletes cached responses. `expired_only` keeps entries still inside their TTL. Returns (files, bytes).
    pub fn prune(root: &Path, expired_only: bool, on_progress: &mut dyn FnMut(f64)) -> Result<(i64, i64)> {
        let store = cache_store(root)?;
        let before = Self::db_bytes(root);
        let mut files = 0i64;
        let mut bytes = 0i64;
        if !expired_only {
            let sizes: Vec<(u64, u64)> = KINDS.iter().map(|k| util::measure(&root.join(k))).collect();
            let legacy: i64 = sizes.iter().map(|s| s.1 as i64).sum();
            let in_db = store.lock().count();
            let total = legacy + in_db;
            files += store.lock().clear(&mut |f| on_progress(if total > 0 { f * in_db as f64 / total as f64 } else { 1.0 }))?;
            for (i, kind) in KINDS.iter().enumerate() {
                util::remove(&root.join(kind))?;
                files += sizes[i].1 as i64;
                bytes += sizes[i].0 as i64;
                let done: i64 = sizes[..=i].iter().map(|s| s.1 as i64).sum();
                on_progress(if total > 0 { (in_db + done) as f64 / total as f64 } else { 1.0 });
            }
        } else {
            files += store.lock().clear_expired(now_ms())?;
            for kind in KINDS {
                let Some(ttl) = cache_ttl(kind) else { continue };
                let cutoff = (now_ms() - ttl) as f64;
                for f in util::walk_files(&root.join(kind)) {
                    if f.mtime_ms >= cutoff {
                        continue;
                    }
                    let _ = std::fs::remove_file(&f.path);
                    files += 1;
                    bytes += f.size as i64;
                }
            }
        }
        let after = Self::db_bytes(root);
        Ok((files, bytes + (before - after).max(0)))
    }

    /// Deletes cached matches and timelines of games `is_old` says the app won't read again. Undecided ones stay.
    pub fn prune_matches(root: &Path, is_old: &mut dyn FnMut(&str, &dyn Fn() -> Option<String>) -> bool) -> Result<(i64, i64)> {
        let mut files = 0i64;
        let mut bytes = 0i64;
        let mut verdicts: HashMap<String, bool> = HashMap::new();
        let store = cache_store(root)?;
        let version_of = |raw: Option<Vec<u8>>| -> Option<String> {
            let v: Value = serde_json::from_slice(&raw?).ok()?;
            v.get("info")?.get("gameVersion")?.as_str().map(String::from)
        };
        let entries = store.lock().match_entries()?;
        for e in entries {
            let id = e.key.clone();
            let old = match verdicts.get(&id) {
                Some(v) => *v,
                None => {
                    let st = store.clone();
                    let (kind, scope, key) = (e.kind.clone(), e.scope.clone(), e.key.clone());
                    let version = move || -> Option<String> {
                        if kind != "match" {
                            return None;
                        }
                        version_of(st.lock().read("match", &scope, &key).ok().flatten().map(|x| x.1))
                    };
                    let v = is_old(&id, &version);
                    verdicts.insert(id.clone(), v);
                    v
                }
            };
            if !old {
                continue;
            }
            store.lock().delete(&e.kind, &e.scope, &e.key)?;
            files += 1;
            bytes += e.bytes;
        }
        for kind in ["match", "timeline"] {
            for f in util::walk_files(&root.join(kind)) {
                let name = f.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let Some(id) = name.strip_suffix(".json.gz") else { continue };
                let id = id.to_string();
                let old = match verdicts.get(&id) {
                    Some(v) => *v,
                    None => {
                        let p = f.path.clone();
                        let version = move || -> Option<String> {
                            if kind != "match" {
                                return None;
                            }
                            version_of(std::fs::read(&p).ok().and_then(|b| gunzip(&b).ok()))
                        };
                        let v = is_old(&id, &version);
                        verdicts.insert(id.clone(), v);
                        v
                    }
                };
                if !old {
                    continue;
                }
                let _ = std::fs::remove_file(&f.path);
                files += 1;
                bytes += f.size as i64;
            }
        }
        if files > 0 {
            store.lock().reclaim()?;
        }
        Ok((files, bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn disk_cache_round_trips_and_honours_ttls() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut cache = DiskCache::new(root);
        cache.set("match", "KR", "KR_1", &json!({"a": 1})).unwrap();
        assert_eq!(cache.get("match", "KR", "KR_1", None), Some(json!({"a": 1})));
        assert_eq!(cache.get("match", "KR", "KR_2", None), None);

        // Stored an hour ago: older than the 10 min match-id TTL.
        let store = CacheStore::open_default(root).unwrap();
        store.write("matchIds", "KR", "p1", br#"["KR_1"]"#, now_ms() - 60 * 60_000).unwrap();
        assert_eq!(cache.get("matchIds", "KR", "p1", None), None);
        assert_eq!(cache.hits, 1);

        // A file from before 0.14.0 is read until it is converted, its mtime the TTL's clock.
        let file = legacy_file(root, "league", "KR", "CHALLENGER");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, crate::db::gzip(br#"{"entries":[1]}"#)).unwrap();
        let mut later = DiskCache::new(root);
        assert_eq!(later.get("league", "KR", "CHALLENGER", None), Some(json!({"entries": [1]})));
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 3600);
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(old).unwrap();
        assert_eq!(later.get("league", "KR", "CHALLENGER", None), None);
        later.set("league", "KR", "CHALLENGER", &json!({"entries": [2]})).unwrap();
        assert_eq!(later.get("league", "KR", "CHALLENGER", None), Some(json!({"entries": [2]})));
        assert!(!file.exists());
        // Compressed with a dictionary once a match and a timeline are both stored.
        let dicts = || store.db.query_row("SELECT COUNT(*) FROM codec_dicts", [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(dicts(), 0);
        cache.set("timeline", "KR", "KR_1", &json!({"info": {"frames": []}})).unwrap();
        assert_eq!(dicts(), 1);
        assert_eq!(cache.get("timeline", "KR", "KR_1", None), Some(json!({"info": {"frames": []}})));
    }
}
