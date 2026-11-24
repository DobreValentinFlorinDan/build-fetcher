//! "Use the same key as Irelia Companion": the one place Build Fetcher looks into Irelia Companion's folder.
//!
//! The rules (docs/DESIGN.md):
//! - the folder is `%APPDATA%\irelia-companion` (`BF_IRELIA_CONFIG_DIR` overrides it, for tests);
//! - only `config.json` is opened, read-only (`OpenOptions::read(true)` and nothing else), and nothing is ever
//!   written, created or renamed in that folder;
//! - only the `riotApiKey` field is read: the JSON is parsed into a type with that one field, so no other value is
//!   ever held in memory, let alone copied (serde skips the rest token by token);
//! - `irelia_key_available` says only whether a key is there, never the key; the key is read again and stored in
//!   Build Fetcher's settings only by `import_irelia_key`, which the app calls on the user's click.
//!
//! Errors and `Unreadable` reasons describe the file, never its content.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};

/// What the Settings page may offer.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase", tag = "state", content = "reason")]
pub enum KeyOffer {
    /// Irelia Companion has a key: offer "Use the same key as Irelia Companion".
    Available,
    /// No Irelia Companion settings, or no key in them.
    Missing,
    /// The key field is there but blank.
    Empty,
    /// The file exists but can't be used (not JSON, a folder, no permission, too big…).
    Unreadable(String),
}

pub const IRELIA_DIR_NAME: &str = "irelia-companion";
pub const IRELIA_FILE: &str = "config.json";
/// Irelia's config.json is a few hundred bytes; anything this big is not it.
const MAX_BYTES: u64 = 1024 * 1024;

/// Irelia Companion's settings folder.
pub fn irelia_config_dir() -> PathBuf {
    match bf_core::util::env("BF_IRELIA_CONFIG_DIR") {
        Some(d) => PathBuf::from(d),
        None => bf_core::app_config::appdata_dir().join(IRELIA_DIR_NAME),
    }
}

pub fn irelia_key_available() -> KeyOffer {
    offer_at(&irelia_config_dir())
}

/// Copies Irelia Companion's key into Build Fetcher's settings (the user clicked). Returns the masked key.
pub fn import_irelia_key() -> Result<String> {
    import_from(&irelia_config_dir(), &crate::settings::settings_file())
}

/// Stores a pasted key in Build Fetcher's settings. Returns the masked key.
pub fn set_key(new: &str) -> Result<String> {
    crate::settings::set_key_at(&crate::settings::settings_file(), new)
}

pub fn offer_at(dir: &Path) -> KeyOffer {
    match read_key(dir) {
        Ok(Some(k)) if !k.trim().is_empty() => KeyOffer::Available,
        Ok(Some(_)) => KeyOffer::Empty,
        Ok(None) => KeyOffer::Missing,
        Err(reason) => KeyOffer::Unreadable(reason),
    }
}

pub fn import_from(dir: &Path, settings_file: &Path) -> Result<String> {
    match read_key(dir) {
        Ok(Some(k)) if !k.trim().is_empty() => crate::settings::set_key_at(settings_file, &k),
        Ok(Some(_)) => Err(anyhow!("Irelia Companion's key is empty")),
        Ok(None) => Err(anyhow!("Irelia Companion has no key")),
        Err(reason) => Err(anyhow!("Irelia Companion's settings can't be read: {reason}")),
    }
}

/// The one field read. `Value` (not `String`) so a non-text value is told apart from a missing one.
#[derive(Deserialize)]
struct OnlyTheKey {
    #[serde(rename = "riotApiKey", default)]
    riot_api_key: Option<Value>,
}

/// Ok(None): no file or no key. Err: a reason for the user (never content).
fn read_key(dir: &Path) -> std::result::Result<Option<String>, String> {
    let path = dir.join(IRELIA_FILE);
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_reason(&e)),
    };
    if meta.is_dir() {
        return Err("config.json is a folder".into());
    }
    if meta.len() > MAX_BYTES {
        return Err("config.json is too big to be Irelia Companion's settings".into());
    }
    // Read-only: no write, append, create or truncate flag.
    let file = std::fs::OpenOptions::new().read(true).open(&path).map_err(|e| io_reason(&e))?;
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes).map_err(|e| io_reason(&e))?;
    let only: OnlyTheKey = serde_json::from_slice(&bytes).map_err(|_| "config.json is not valid JSON settings".to_string())?;
    match only.riot_api_key {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err("riotApiKey is not text".into()),
    }
}

fn io_reason(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => "no permission to read config.json".into(),
        _ => format!("config.json can't be read ({:?})", e.kind()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{api_key, load, KEY_FIELD};
    use std::time::{Duration, SystemTime};

    const FAKE: &str = "RGAPI-00000000-test";

    /// Irelia's folder with a config.json holding `text`, its mtime pushed into the past so a rewrite would show.
    fn irelia(text: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join(IRELIA_FILE);
        std::fs::write(&f, text).unwrap();
        let old = SystemTime::now() - Duration::from_secs(3600 * 24 * 30);
        filetime::set_file_mtime(&f, filetime::FileTime::from_system_time(old)).unwrap();
        (dir, f)
    }

    /// Bytes, mtime and the folder listing: everything a write would change.
    fn fingerprint(dir: &Path) -> Vec<(String, Vec<u8>, SystemTime)> {
        let mut v: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let m = e.metadata().unwrap();
                let bytes = if m.is_file() { std::fs::read(e.path()).unwrap() } else { vec![] };
                (e.file_name().to_string_lossy().into_owned(), bytes, m.modified().unwrap())
            })
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    #[test]
    fn present_key_is_offered_then_imported_and_nothing_else_is_copied() {
        let text = format!(r#"{{"riotApiKey":"  {FAKE} ","dataDir":"D:/irelia","port":5173,"tier":"MASTER","nested":{{"riotApiKey":"other"}}}}"#);
        let (dir, _) = irelia(&text);
        let before = fingerprint(dir.path());
        assert_eq!(offer_at(dir.path()), KeyOffer::Available);
        let ours = tempfile::tempdir().unwrap();
        let settings = ours.path().join("settings.json");
        std::fs::write(&settings, r#"{"gpu":false}"#).unwrap();
        assert_eq!(import_from(dir.path(), &settings).unwrap(), "RGAPI-…test");
        assert_eq!(api_key(&settings).as_deref(), Some(FAKE));
        // Only riotApiKey was added to Build Fetcher's settings: no dataDir, port, tier or nested value.
        let raw: serde_json::Map<String, Value> = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        let mut keys: Vec<&str> = raw.keys().map(|k| k.as_str()).collect();
        keys.sort();
        assert_eq!(keys, vec!["gpu", KEY_FIELD]);
        assert!(!load(&settings).0.gpu);
        // Irelia's folder: same files, same bytes, same mtimes.
        assert_eq!(fingerprint(dir.path()), before);
    }

    #[test]
    fn missing_folder_file_or_field() {
        let none = tempfile::tempdir().unwrap();
        assert_eq!(offer_at(&none.path().join("not-installed")), KeyOffer::Missing);
        assert_eq!(offer_at(none.path()), KeyOffer::Missing);
        let (dir, _) = irelia(r#"{"dataDir":"D:/x"}"#);
        assert_eq!(offer_at(dir.path()), KeyOffer::Missing);
        let (dir2, _) = irelia(r#"{"riotApiKey":null}"#);
        assert_eq!(offer_at(dir2.path()), KeyOffer::Missing);
        let ours = tempfile::tempdir().unwrap();
        let settings = ours.path().join("settings.json");
        assert!(import_from(dir.path(), &settings).is_err());
        assert!(!settings.exists(), "a failed import writes nothing");
    }

    #[test]
    fn empty_key() {
        let (dir, _) = irelia(r#"{"riotApiKey":"   "}"#);
        assert_eq!(offer_at(dir.path()), KeyOffer::Empty);
        let ours = tempfile::tempdir().unwrap();
        assert!(import_from(dir.path(), &ours.path().join("s.json")).is_err());
    }

    #[test]
    fn unreadable_files() {
        for text in ["{not json", "[1,2,3]", r#"{"riotApiKey": 12345}"#, ""] {
            let (dir, _) = irelia(text);
            let before = fingerprint(dir.path());
            match offer_at(dir.path()) {
                KeyOffer::Unreadable(reason) => assert!(!reason.contains("12345"), "{reason}"),
                other => panic!("{text}: {other:?}"),
            }
            assert_eq!(fingerprint(dir.path()), before);
        }
        // A folder where the file should be.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(IRELIA_FILE)).unwrap();
        assert_eq!(offer_at(dir.path()), KeyOffer::Unreadable("config.json is a folder".into()));
        // Too big.
        let (big, _) = irelia(&format!(r#"{{"riotApiKey":"{FAKE}","pad":"{}"}}"#, "x".repeat(2 * 1024 * 1024)));
        assert!(matches!(offer_at(big.path()), KeyOffer::Unreadable(_)));
    }

    #[cfg(unix)]
    #[test]
    fn no_permission() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, f) = irelia(&format!(r#"{{"riotApiKey":"{FAKE}"}}"#));
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o000)).unwrap();
        // root reads anything: then the permission case can't be produced here, and the key is simply available.
        let readable_anyway = std::fs::File::open(&f).is_ok();
        let offer = offer_at(dir.path());
        if readable_anyway {
            assert_eq!(offer, KeyOffer::Available);
        } else {
            assert_eq!(offer, KeyOffer::Unreadable("no permission to read config.json".into()));
        }
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn read_only_folder_still_works() {
        // If anything tried to write next to config.json (a temp file, a backup), a read-only folder would fail it.
        use std::os::unix::fs::PermissionsExt;
        let (dir, _) = irelia(&format!(r#"{{"riotApiKey":"{FAKE}"}}"#));
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let ours = tempfile::tempdir().unwrap();
        let before = fingerprint(dir.path());
        assert_eq!(import_from(dir.path(), &ours.path().join("settings.json")).unwrap(), "RGAPI-…test");
        assert_eq!(fingerprint(dir.path()), before);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn env_var_points_at_the_folder() {
        let (dir, _) = irelia(&format!(r#"{{"riotApiKey":"{FAKE}"}}"#));
        // Only this test sets the variable.
        std::env::set_var("BF_IRELIA_CONFIG_DIR", dir.path());
        assert_eq!(irelia_config_dir(), dir.path());
        assert_eq!(irelia_key_available(), KeyOffer::Available);
        std::env::remove_var("BF_IRELIA_CONFIG_DIR");
    }

    #[test]
    fn offer_serializes_without_the_key() {
        assert_eq!(serde_json::to_value(KeyOffer::Available).unwrap(), serde_json::json!({ "state": "available" }));
        assert_eq!(serde_json::to_value(KeyOffer::Unreadable("x".into())).unwrap(), serde_json::json!({ "state": "unreadable", "reason": "x" }));
    }
}
