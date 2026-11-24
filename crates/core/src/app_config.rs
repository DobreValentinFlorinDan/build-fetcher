//! Build Fetcher's per-user settings, shared by the desktop shell and the server (from irelia-companion's
//! `app_config.rs`):
//!
//!   %APPDATA%/build-fetcher/settings.json   (BF_CONFIG_DIR overrides the folder, e.g. for tests)
//!
//! Every write re-reads the file and merges only the caller's keys, then replaces the file atomically. Keys this
//! version doesn't know are kept as they are. Build Fetcher never writes anywhere else: Irelia Companion's folder is
//! only ever read, and only its `riotApiKey` (see crates/store `key_import`).

use crate::js;
use crate::util;
use serde_json::{Map, Value};
use std::path::PathBuf;

pub fn app_config_dir() -> PathBuf {
    if let Some(d) = util::env("BF_CONFIG_DIR") {
        return util::resolve(d);
    }
    util::resolve(appdata_dir().join(APP_DIR_NAME))
}

/// The folder name under %APPDATA%: settings, data, logs.
pub const APP_DIR_NAME: &str = "build-fetcher";

/// %APPDATA% (Windows), else ~/.config.
pub fn appdata_dir() -> PathBuf {
    util::env("APPDATA").map(PathBuf::from).unwrap_or_else(|| home_dir().join(".config"))
}

pub fn home_dir() -> PathBuf {
    util::env("HOME").or_else(|| util::env("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub fn app_config_file() -> PathBuf {
    app_config_dir().join("settings.json")
}

/// config.json as stored (an empty object when missing or unreadable).
pub fn read_app_config() -> Map<String, Value> {
    std::fs::read_to_string(app_config_file())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

/// Merges `patch` into config.json: `Some(v)` sets a key, `None` removes it (JS's `undefined`).
pub fn update_app_config(patch: Vec<(&str, Option<Value>)>) -> Map<String, Value> {
    let mut next = read_app_config();
    for (k, v) in patch {
        match v {
            Some(v) => {
                next.insert(k.to_string(), v);
            }
            None => {
                next.shift_remove(k);
            }
        }
    }
    let text = format!("{}\n", js::stringify_pretty(&Value::Object(next.clone())));
    let _ = util::write_atomic(&app_config_file(), text.as_bytes());
    next
}

/// One key of config.json.
pub fn get(key: &str) -> Option<Value> {
    read_app_config().get(key).cloned().filter(|v| !v.is_null())
}

pub fn get_str(key: &str) -> Option<String> {
    get(key).and_then(|v| v.as_str().map(String::from))
}

pub fn get_bool(key: &str) -> Option<bool> {
    get(key).and_then(|v| v.as_bool())
}
