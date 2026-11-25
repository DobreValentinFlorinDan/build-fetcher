//! `dashboardPriority` of `desktop/main.ts`: during champ select and games the pages' renderer processes run at
//! below-normal priority (0.14.0), as the app itself does (server/gameMode): League wins every contest for a core.
//! Back to normal when the game ends.
//!
//! Electron gave each window its own renderer and lowered only the dashboard's. WebView2's renderers are children of
//! its browser process (a child of the app), and which page a renderer holds isn't visible from outside; 1.0 lowers the
//! renderers of the app's WebView2 (dashboard and overlay). The overlay still gets every core League leaves idle.

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// PIDs of the web renderer processes under this process (WebView2 on Windows, WebKitGTK elsewhere).
pub fn renderer_pids() -> Vec<u32> {
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always));
    let me = Pid::from_u32(std::process::id());
    let under_me = |mut p: Option<Pid>| {
        for _ in 0..8 {
            match p {
                Some(x) if x == me => return true,
                Some(x) => p = sys.process(x).and_then(|q| q.parent()),
                None => return false,
            }
        }
        false
    };
    sys.processes()
        .iter()
        .filter(|(_, p)| {
            let cmd = p.cmd().iter().map(|c| c.to_string_lossy()).collect::<Vec<_>>().join(" ");
            let name = p.name().to_string_lossy().to_lowercase();
            (cmd.contains("--type=renderer") || name.contains("webkitwebprocess")) && under_me(p.parent())
        })
        .map(|(pid, _)| pid.as_u32())
        .collect()
}

/// Below normal (`below`) or normal for these processes; returns how many were set.
pub fn set_priority(pids: &[u32], below: bool) -> usize {
    pids.iter().filter(|&&pid| set_one(pid, below)).count()
}

#[cfg(windows)]
fn set_one(pid: u32, below: bool) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, SetPriorityClass, BELOW_NORMAL_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, PROCESS_SET_INFORMATION,
    };
    unsafe {
        let h = OpenProcess(PROCESS_SET_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let ok = SetPriorityClass(h, if below { BELOW_NORMAL_PRIORITY_CLASS } else { NORMAL_PRIORITY_CLASS }) != 0;
        CloseHandle(h);
        ok
    }
}

#[cfg(unix)]
fn set_one(pid: u32, below: bool) -> bool {
    unsafe extern "C" {
        fn setpriority(which: i32, who: u32, prio: i32) -> i32;
    }
    unsafe { setpriority(0, pid, if below { 10 } else { 0 }) == 0 }
}
