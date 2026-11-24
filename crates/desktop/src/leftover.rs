//! Port of `desktop/leftover.ts`: a middleware left running by an earlier start (0.14.2).
//!
//! 0.x ran its middleware in a separate process. When that app went without ending it (its process ended by hand, or a
//! crash), the middleware kept the port and the database open, and every later start failed on them. Its PID is kept
//! in `middleware.pid` next to the settings; a start ends it by that PID, after checking that the PID is still that
//! executable (never by name: another program may have taken the PID).
//!
//! 1.0 runs its server in the app's own process, so it writes no such file; it still ends one a 0.x start left behind
//! (the first start after the update).

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PidRecord {
    pub pid: u32,
    pub exe: String,
    pub at: f64,
}

pub struct LeftoverDeps<'a> {
    /// The running image of a PID ("Irelia Companion.exe"); None when there's no such process.
    pub image_of: &'a dyn Fn(u32) -> Option<String>,
    pub kill: &'a dyn Fn(u32),
    pub alive: &'a dyn Fn(u32) -> bool,
    pub log: &'a dyn Fn(&str),
    /// How long to wait for it to go.
    pub wait: Duration,
}

pub fn write_pid(file: &Path, rec: &PidRecord) {
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(file, format!("{}\n", serde_json::to_string(rec).unwrap_or_default()));
}

/// Removed when that process exits (only if the file is still about it).
pub fn clear_pid(file: &Path, pid: u32) {
    if read_pid(file).map(|r| r.pid) == Some(pid) {
        let _ = std::fs::remove_file(file);
    }
}

pub fn read_pid(file: &Path) -> Option<PidRecord> {
    let text = std::fs::read_to_string(file).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let pid = v.get("pid")?.as_f64()?;
    let exe = v.get("exe")?.as_str()?;
    if pid.fract() != 0.0 || pid <= 0.0 || pid > u32::MAX as f64 {
        return None;
    }
    Some(PidRecord { pid: pid as u32, exe: exe.to_string(), at: v.get("at").and_then(|a| a.as_f64()).unwrap_or(0.0) })
}

/// The last part of a path written on either system (the record may come from Windows).
fn base_name(p: &str) -> &str {
    p.rsplit(['/', '\\']).next().unwrap_or(p)
}

/// Ends the middleware an earlier start left running, if any; returns its PID when it ended one.
pub fn end_leftover(file: &Path, d: &LeftoverDeps) -> Option<u32> {
    let rec = read_pid(file)?;
    let same = (d.image_of)(rec.pid).is_some_and(|image| image.to_lowercase() == base_name(&rec.exe).to_lowercase());
    if !same {
        let _ = std::fs::remove_file(file);
        return None;
    }
    (d.log)(&format!("[desktop] ending the middleware an earlier start left running (PID {})", rec.pid));
    (d.kill)(rec.pid);
    let until = Instant::now() + d.wait;
    while (d.alive)(rec.pid) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(200));
    }
    if (d.alive)(rec.pid) {
        (d.log)(&format!("[desktop] PID {} is still running", rec.pid));
    } else {
        let _ = std::fs::remove_file(file);
    }
    Some(rec.pid)
}

/// The image of a PID: tasklist on Windows, /proc elsewhere.
pub fn image_of(pid: u32) -> Option<String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            // "Irelia Companion.exe","1234","Console","1","123,456 K"
            let mut cols = line.trim().split("\",\"");
            let (Some(name), Some(p)) = (cols.next(), cols.next()) else { continue };
            let name = name.trim_start_matches('"');
            if p.trim_matches('"').parse::<u32>().ok() == Some(pid) {
                return Some(name.to_string());
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    }
}

pub fn alive(pid: u32) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            // Access denied still means it exists.
            return windows_sys::Win32::Foundation::GetLastError() == windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE as u32
    }
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        if unsafe { kill(pid as i32, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(1) // EPERM
    }
}

pub fn kill(pid: u32) {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !h.is_null() {
            TerminateProcess(h, 1);
            CloseHandle(h);
        }
    }
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        unsafe {
            kill(pid as i32, 15);
        }
    }
}
