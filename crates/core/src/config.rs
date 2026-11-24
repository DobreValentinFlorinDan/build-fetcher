//! Port of `server/config.ts`: the process's settings (paths, the API key, pacing, engine constants).

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

/// "20:1,100:120" → windows (riot/rateLimiter.ts parseLimits).
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
    pub riot_api_key: String,
    pub port: u16,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub ddragon_cache_dir: PathBuf,
    pub riot_cache_dir: PathBuf,
    pub rate_limits: Vec<RateWindow>,
    pub pacing: PacingOptions,
    pub league_paths: Vec<PathBuf>,
}

pub const CHAMPION_ID: i64 = 39;
pub const CHAMPION_KEY: &str = "Irelia";
pub const RANKED_SOLO_QUEUE: i64 = 420;
pub const RANKED_FLEX_QUEUE: i64 = 440;
pub const RESEED_AFTER_MS: i64 = 6 * 3600_000;
pub const RECHECK_OTP_AFTER_MS: i64 = 3 * 24 * 3600_000;
pub const RECHECK_OTHERS_AFTER_MS: i64 = 14 * 24 * 3600_000;
pub const RESCAN_PLAYER_AFTER_MS: i64 = 45 * 60_000;
pub const RATE_SNAPSHOT_MS: u64 = 200;
pub const MIN_MATCHUP_GAMES: f64 = 15.0;
pub const GENERAL_PRIOR_WEIGHT: f64 = 0.15;
pub const MIN_LANE_GAMES: f64 = 30.0;
pub const OTHER_LANE_WEIGHT: f64 = 0.15;
pub const PREVIOUS_PATCH_WEIGHT: f64 = 0.35;
pub const MIN_CURRENT_PATCH_GAMES: f64 = 40.0;
pub const STARTING_ITEM_WINDOW_MS: f64 = 60_000.0;
pub const LIVE_POLL_MS: u64 = 1000;
pub const LIVE_IDLE_POLL_MS: u64 = 3000;

const DEV_KEY_LIMITS: &str = "20:1,100:120";

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

/// data/ unless the user moved it: ./data in dev, %APPDATA%/irelia-companion/data in the installed app.
pub fn default_data_dir() -> PathBuf {
    util::resolve(util::env("IRELIA_DEFAULT_DATA_DIR").unwrap_or_else(|| "data".into()))
}

/// IRELIA_DATA_DIR pins the folder for this process and disables moving it.
pub fn data_dir_locked_by() -> Option<&'static str> {
    util::env("IRELIA_DATA_DIR").map(|_| "IRELIA_DATA_DIR environment variable")
}

pub struct DataPaths {
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub ddragon_cache_dir: PathBuf,
    pub riot_cache_dir: PathBuf,
}

pub fn data_paths(dir: &Path) -> DataPaths {
    DataPaths {
        data_dir: dir.to_path_buf(),
        db_path: dir.join("irelia.db"),
        ddragon_cache_dir: dir.join("ddragon"),
        riot_cache_dir: dir.join("cache"),
    }
}

fn load() -> Config {
    util::load_env_file(&util::resolve(".env"));
    let app = app_config::read_app_config();
    let key = app.get("riotApiKey").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from);
    let dir = util::env("IRELIA_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| app.get("dataDir").and_then(|v| v.as_str()).map(PathBuf::from))
        .unwrap_or_else(default_data_dir);
    let p = data_paths(&util::resolve(dir));
    let limits = {
        let custom = parse_limits(util::env("RIOT_RATE_LIMITS").as_deref());
        if custom.is_empty() {
            parse_limits(Some(DEV_KEY_LIMITS))
        } else {
            custom
        }
    };
    Config {
        riot_api_key: key.or_else(|| util::env("RIOT_API_KEY")).unwrap_or_default(),
        port: util::env("PORT").and_then(|p| p.parse().ok()).unwrap_or(5174),
        data_dir: p.data_dir,
        db_path: p.db_path,
        ddragon_cache_dir: p.ddragon_cache_dir,
        riot_cache_dir: p.riot_cache_dir,
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

/// Points every data path at a new folder (after a data-folder move). Callers reopen what they hold.
pub fn set_data_dir(dir: &Path) {
    let p = data_paths(&util::resolve(dir));
    let mut c = CONFIG.write();
    c.data_dir = p.data_dir;
    c.db_path = p.db_path;
    c.ddragon_cache_dir = p.ddragon_cache_dir;
    c.riot_cache_dir = p.riot_cache_dir;
}

pub fn set_riot_api_key(key: &str) {
    CONFIG.write().riot_api_key = key.to_string();
}

pub fn set_port(port: u16) {
    CONFIG.write().port = port;
}
