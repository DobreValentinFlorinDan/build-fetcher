//! The process's settings (paths, the API key, pacing), from irelia-companion's `config.rs` with Build Fetcher's
//! identity: data under %APPDATA%/build-fetcher/data unless BF_DATA_DIR pins another folder.

use crate::app_config;
use crate::util;
use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateWindow {
    pub limit: f64,
    pub window_ms: f64,
}

/// "20:1,100:120" → windows (X-App-Rate-Limit / X-Method-Rate-Limit). Malformed pairs are skipped.
pub fn parse_limits(header: Option<&str>) -> Vec<RateWindow> {
    let Some(h) = header.filter(|h| !h.is_empty()) else { return vec![] };
    h.split(',')
        .filter_map(|pair| {
            let parts: Vec<f64> = pair.trim().split(':').map(|x| crate::js::to_number(&serde_json::Value::String(x.into()))).collect();
            let limit = *parts.first()?;
            let s = *parts.get(1)?;
            (limit.is_finite() && s.is_finite() && limit > 0.0 && s > 0.0).then_some(RateWindow { limit, window_ms: s * 1000.0 })
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
pub struct PacingOptions {
    pub safety: f64,
    pub burst: f64,
    pub concurrency: usize,
    pub slowdown: f64,
    pub recovery_ms: f64,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Empty when none is set. Never logged, never put in an error (see `mask_key`).
    pub riot_api_key: String,
    pub port: u16,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub ddragon_cache_dir: PathBuf,
    pub rate_limits: Vec<RateWindow>,
    pub pacing: PacingOptions,
    pub league_paths: Vec<PathBuf>,
}

pub const RANKED_SOLO_QUEUE: i64 = 420;
pub const LIVE_POLL_MS: u64 = 1000;
pub const LIVE_IDLE_POLL_MS: u64 = 3000;

/// A personal / development key's limits, until Riot's X-App-Rate-Limit header says otherwise.
pub const DEFAULT_KEY_LIMITS: &str = "20:1,100:120";

fn clamp_int(raw: Option<String>, lo: i64, hi: i64, fallback: i64) -> i64 {
    match raw {
        Some(r) => {
            let n = crate::js::round(crate::js::to_number(&serde_json::Value::String(r)));
            if n.is_finite() {
                (n as i64).clamp(lo, hi)
            } else {
                fallback
            }
        }
        None => fallback,
    }
}

/// %APPDATA%/build-fetcher/data (BF_DEFAULT_DATA_DIR overrides it, e.g. ./data in development).
pub fn default_data_dir() -> PathBuf {
    match util::env("BF_DEFAULT_DATA_DIR") {
        Some(d) => util::resolve(d),
        None => app_config::app_config_dir().join("data"),
    }
}

/// BF_DATA_DIR pins the folder for this process.
pub fn data_dir_locked_by() -> Option<&'static str> {
    util::env("BF_DATA_DIR").map(|_| "BF_DATA_DIR environment variable")
}

pub struct DataPaths {
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub ddragon_cache_dir: PathBuf,
}

pub fn data_paths(dir: &Path) -> DataPaths {
    DataPaths { data_dir: dir.to_path_buf(), db_path: dir.join("build-fetcher.db"), ddragon_cache_dir: dir.join("ddragon") }
}

/// A key as it may be shown or logged: "RGAPI-…1234" (never the whole key), or "" when none is set.
pub fn mask_key(key: &str) -> String {
    let k = key.trim();
    if k.is_empty() {
        return String::new();
    }
    let tail: String = k.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    if k.chars().count() <= 8 {
        return "…".into();
    }
    format!("{}…{tail}", if k.starts_with("RGAPI-") { "RGAPI-" } else { "" })
}

fn load() -> Config {
    let app = app_config::read_app_config();
    let key = app.get("riotApiKey").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()).map(|s| s.trim().to_string());
    let dir = util::env("BF_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| app.get("dataDir").and_then(|v| v.as_str()).map(PathBuf::from))
        .unwrap_or_else(default_data_dir);
    let p = data_paths(&util::resolve(dir));
    let limits = {
        let custom = parse_limits(util::env("RIOT_RATE_LIMITS").as_deref());
        if custom.is_empty() {
            parse_limits(Some(DEFAULT_KEY_LIMITS))
        } else {
            custom
        }
    };
    Config {
        riot_api_key: key.or_else(|| util::env("RIOT_API_KEY")).unwrap_or_default(),
        port: util::env("PORT").and_then(|p| p.parse().ok()).unwrap_or(5184),
        data_dir: p.data_dir,
        db_path: p.db_path,
        ddragon_cache_dir: p.ddragon_cache_dir,
        rate_limits: limits,
        pacing: PacingOptions {
            safety: 0.9,
            burst: 3.0,
            concurrency: clamp_int(util::env("RIOT_CONCURRENCY"), 1, 3, 3) as usize,
            slowdown: 0.8,
            recovery_ms: 60_000.0,
        },
        league_paths: [
            util::env("LEAGUE_PATH"),
            Some("C:/Riot Games/League of Legends".into()),
            Some("D:/Riot Games/League of Legends".into()),
            Some("C:/Program Files/Riot Games/League of Legends".into()),
        ]
        .into_iter()
        .flatten()
        .map(PathBuf::from)
        .collect(),
    }
}

static CONFIG: LazyLock<RwLock<Config>> = LazyLock::new(|| RwLock::new(load()));

/// The current settings (a copy: cheap, and never holds a lock across work).
pub fn config() -> Config {
    CONFIG.read().clone()
}

/// Points every data path at a new folder. Callers reopen what they hold.
pub fn set_data_dir(dir: &Path) {
    let p = data_paths(&util::resolve(dir));
    let mut c = CONFIG.write();
    c.data_dir = p.data_dir;
    c.db_path = p.db_path;
    c.ddragon_cache_dir = p.ddragon_cache_dir;
}

pub fn set_riot_api_key(key: &str) {
    CONFIG.write().riot_api_key = key.trim().to_string();
}

pub fn set_port(port: u16) {
    CONFIG.write().port = port;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_never_show_the_key() {
        assert_eq!(mask_key(""), "");
        assert_eq!(mask_key("short"), "…");
        let k = "RGAPI-12345678-aaaa-bbbb-cccc-1234567890ab";
        let m = mask_key(k);
        assert_eq!(m, "RGAPI-…90ab");
        assert!(!m.contains("12345678"));
    }

    #[test]
    fn parses_limit_headers() {
        let w = parse_limits(Some("20:1,100:120"));
        assert_eq!(w, vec![RateWindow { limit: 20.0, window_ms: 1000.0 }, RateWindow { limit: 100.0, window_ms: 120_000.0 }]);
        assert!(parse_limits(Some("x:y")).is_empty());
        assert!(parse_limits(None).is_empty());
    }
}
