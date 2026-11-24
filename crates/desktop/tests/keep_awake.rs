//! Port of desktop/keepAwake.test.ts. 0.22.0 held the request through a PowerShell helper; 1.0 makes the call on a
//! thread of its own, so the fake here stands in for SetThreadExecutionState. On Windows (CI's windows-latest) the
//! real call runs and `powercfg /requests` must list the request under SYSTEM.

use irelia_desktop::keep_awake::*;
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

#[derive(Default)]
struct FakePower {
    calls: Mutex<Vec<(ThreadId, u32)>>,
    refuse: bool,
}

impl Power for FakePower {
    fn set_thread_state(&self, flags: u32) -> Result<(), String> {
        self.calls.lock().unwrap().push((std::thread::current().id(), flags));
        if self.refuse && flags != ES_CONTINUOUS {
            Err("Windows refused the request".into())
        } else {
            Ok(())
        }
    }
}

fn harness(refuse: bool) -> (KeepAwake, Arc<FakePower>, Arc<Mutex<Vec<KeepAwakeState>>>) {
    let power = Arc::new(FakePower { refuse, ..Default::default() });
    let states = Arc::new(Mutex::new(Vec::new()));
    let s2 = states.clone();
    let k = KeepAwake::new(power.clone(), Arc::new(move |s| s2.lock().unwrap().push(s)), Arc::new(|_| {}));
    (k, power, states)
}

fn until(what: &str, f: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn holds_a_system_required_request_on_its_own_thread_and_lets_go() {
    let (k, power, states) = harness(false);
    k.set(true, false);
    until("held", || k.state().system == "held");
    assert_eq!(k.state(), KeepAwakeState { active: true, system: "held", screen: false, error: None });
    let calls = power.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, 0x8000_0001, "ES_CONTINUOUS | ES_SYSTEM_REQUIRED");
    assert_ne!(calls[0].0, std::thread::current().id(), "held by a thread of its own");

    k.set(true, false); // asked again (another setting changed): nothing new
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(power.calls.lock().unwrap().len(), 1);

    k.set(false, false);
    assert_eq!(k.state(), KeepAwakeState { active: false, system: "off", screen: false, error: None });
    until("let go", || power.calls.lock().unwrap().len() == 2);
    let calls = power.calls.lock().unwrap().clone();
    assert_eq!(calls[1], (calls[0].0, ES_CONTINUOUS), "the same thread lets go");
    let seen: Vec<&str> = states.lock().unwrap().iter().map(|s| s.system).collect();
    assert_eq!(seen, ["starting", "held", "off"]);
}

#[test]
fn the_screen_too_adds_the_display_request_on_the_same_thread() {
    let (k, power, _) = harness(false);
    k.set(true, true);
    until("held", || k.state().system == "held");
    assert!(k.state().screen);
    k.set(true, false);
    until("held again", || k.state().system == "held" && power.calls.lock().unwrap().len() == 2);
    let calls = power.calls.lock().unwrap().clone();
    assert_eq!(calls[0].1, 0x8000_0003);
    assert_eq!(calls[1].1, 0x8000_0001);
    assert_eq!(calls[0].0, calls[1].0, "the holder keeps running");
    assert!(!k.state().screen);
    k.set(false, false);
}

#[test]
fn a_refused_request_is_reported() {
    let (k, _, _) = harness(true);
    k.set(true, false);
    until("failed", || k.state().system == "failed");
    assert_eq!(k.state().error.as_deref(), Some("Windows refused the request"));
    assert!(!k.state().active);
    k.set(false, false);
    assert_eq!(k.state().error, None);
}

#[cfg(windows)]
#[test]
fn on_windows_the_real_call_holds_a_request_windows_lists_and_lets_go() {
    let k = KeepAwake::new(Arc::new(SystemPower), Arc::new(|_| {}), Arc::new(|l| println!("{l}")));
    k.set(true, false);
    until("held", || k.state().system != "starting");
    assert_eq!(k.state().system, "held", "{:?}", k.state().error);
    // powercfg /requests needs an administrator; CI's runner is one.
    match std::process::Command::new("powercfg").arg("/requests").output() {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            println!("{text}");
            let system = text.split("SYSTEM:").nth(1).and_then(|s| s.split("AWAYMODE:").next()).unwrap_or("");
            let exe = std::env::current_exe().unwrap();
            let name = exe.file_name().unwrap().to_string_lossy().to_lowercase();
            assert!(system.to_lowercase().contains(&name), "listed under SYSTEM ({name})");
        }
        other => println!("powercfg /requests unavailable: {other:?}"),
    }
    k.set(false, false);
}
