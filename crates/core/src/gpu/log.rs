//! The GPU log, %APPDATA%/build-fetcher/gpu.log, shown in Settings → GPU. What the
//! GPU was found to be, the checks and what it did; nothing else goes in it. Kept under about half a megabyte: past
//! that, its older half goes.

use std::io::Write;
use std::path::PathBuf;

const MAX_BYTES: u64 = 512 * 1024;

pub fn gpu_log_file() -> PathBuf {
    crate::app_config::app_config_dir().join("gpu.log")
}

pub fn gpu_log(line: &str) {
    let file = gpu_log_file();
    let _ = std::fs::create_dir_all(crate::app_config::app_config_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&file) {
        let _ = writeln!(f, "{}  {line}", chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    }
    if std::fs::metadata(&file).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
        if let Ok(text) = std::fs::read_to_string(&file) {
            let from = text.len().saturating_sub(MAX_BYTES as usize / 2);
            let at = text[from..].find('\n').map(|i| from + i + 1).unwrap_or(from);
            let _ = std::fs::write(&file, &text[at..]);
        }
    }
}

/// The log's last `lines` lines ('' before anything was written).
pub fn read_gpu_log(lines: usize) -> String {
    let text = std::fs::read_to_string(gpu_log_file()).unwrap_or_default();
    let all: Vec<&str> = text.split('\n').collect();
    all[all.len().saturating_sub(lines + 1)..].join("\n").trim_end().to_string()
}

pub fn clear_gpu_log() {
    let _ = std::fs::remove_file(gpu_log_file());
}
