//! The app's log: `logs/middleware.log` next to the settings (the name 0.x used, so the Settings page and older notes
//! still point at it), moved to `.1` above 5 MB at start. The installed app has no console, so its standard output and
//! error go there; a development build keeps the terminal.

use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

pub fn log_file(config_dir: &Path) -> PathBuf {
    config_dir.join("logs").join("middleware.log")
}

/// Rotates the log if it is large, writes the start line, and (when `redirect`) sends stdout and stderr to it.
pub fn open_log(file: &Path, redirect: bool) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(file).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false) {
        let mut old = file.as_os_str().to_owned();
        old.push(".1");
        let _ = std::fs::rename(file, PathBuf::from(old));
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(file)?;
    writeln!(f, "\n--- {} starting Irelia Companion {} ---", now_iso(), env!("CARGO_PKG_VERSION"))?;
    if redirect {
        redirect_std(f);
    }
    Ok(())
}

fn now_iso() -> String {
    chrono_like_iso(irelia::util::now_ms())
}

/// `new Date(ms).toISOString()`.
fn chrono_like_iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)).unwrap_or_default()
}

#[cfg(windows)]
fn redirect_std(f: std::fs::File) {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
    let h = f.into_raw_handle();
    unsafe {
        SetStdHandle(STD_OUTPUT_HANDLE, h as _);
        SetStdHandle(STD_ERROR_HANDLE, h as _);
    }
}

#[cfg(unix)]
fn redirect_std(f: std::fs::File) {
    use std::os::unix::io::IntoRawFd;
    unsafe extern "C" {
        fn dup2(old: i32, new: i32) -> i32;
    }
    let fd = f.into_raw_fd();
    unsafe {
        dup2(fd, 1);
        dup2(fd, 2);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn iso_dates() {
        assert_eq!(super::chrono_like_iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(super::chrono_like_iso(1_762_357_479_123), "2025-11-05T15:44:39.123Z");
    }
}
