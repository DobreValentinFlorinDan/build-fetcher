//! Port of desktop/leftover.test.ts: a middleware left running by an earlier start is ended by its PID only when that
//! PID is still the same executable.

use irelia_desktop::leftover::*;
use std::sync::Mutex;
use std::time::Duration;

fn long_running() -> std::process::Command {
    #[cfg(windows)]
    {
        let mut c = std::process::Command::new("ping");
        c.args(["-n", "60", "127.0.0.1"]);
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = std::process::Command::new("sleep");
        c.arg("60");
        c
    }
}

/// The executable's path as the record keeps it.
fn exe_of(pid: u32) -> String {
    #[cfg(windows)]
    {
        let _ = pid;
        "C:\\Windows\\System32\\PING.EXE".into()
    }
    #[cfg(not(windows))]
    {
        std::fs::read_link(format!("/proc/{pid}/exe")).unwrap().to_string_lossy().into_owned()
    }
}

#[test]
fn leftover_middleware() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("middleware.pid");
    let lines = Mutex::new(Vec::<String>::new());
    let log = |l: &str| lines.lock().unwrap().push(l.to_string());
    let deps = LeftoverDeps { image_of: &image_of, kill: &kill, alive: &alive, log: &log, wait: Duration::from_secs(5) };

    // a process of the same executable is ended by its PID, and the file goes
    let mut child = long_running().stdout(std::process::Stdio::null()).spawn().unwrap();
    let pid = child.id();
    write_pid(&file, &PidRecord { pid, exe: exe_of(pid), at: 0.0 });
    // Reaps it as it ends (a zombie still counts as alive).
    let reaper = std::thread::spawn(move || child.wait());
    assert_eq!(end_leftover(&file, &deps), Some(pid));
    reaper.join().unwrap().unwrap();
    assert!(!alive(pid));
    assert!(!file.exists());
    assert!(lines.lock().unwrap().last().unwrap().contains(&format!("PID {pid}")));

    // a PID of another executable, or of no process, is left alone
    let killed = Mutex::new(0);
    let count = |_: u32| *killed.lock().unwrap() += 1;
    let no_kill = LeftoverDeps { kill: &count, ..deps };
    write_pid(&file, &PidRecord { pid: std::process::id(), exe: dir.path().join("Something Else.exe").to_string_lossy().into(), at: 0.0 });
    assert_eq!(end_leftover(&file, &no_kill), None);
    assert!(!file.exists(), "a stale file goes");
    write_pid(&file, &PidRecord { pid: (1 << 22) + 12345, exe: exe_of(std::process::id()), at: 0.0 });
    assert_eq!(end_leftover(&file, &no_kill), None);
    assert_eq!(*killed.lock().unwrap(), 0);
    assert_eq!(end_leftover(&file, &no_kill), None, "no file");

    // the file is cleared only for its own PID
    write_pid(&file, &PidRecord { pid: 42, exe: "x".into(), at: 1.0 });
    clear_pid(&file, 43);
    assert_eq!(read_pid(&file).map(|r| r.pid), Some(42));
    clear_pid(&file, 42);
    assert_eq!(read_pid(&file), None);

    // a record written by 0.22.0 (Windows paths, integer time) is read
    std::fs::write(&file, "{\"pid\":4242,\"exe\":\"C:\\\\Users\\\\me\\\\AppData\\\\Local\\\\Programs\\\\irelia-companion\\\\IreliaCompanion.exe\",\"at\":1762357479123}\n").unwrap();
    let rec = read_pid(&file).unwrap();
    assert_eq!(rec.pid, 4242);
    assert!(rec.exe.ends_with("IreliaCompanion.exe"));
}
