//! Port of `desktop/keepAwake.ts`: keeping Windows awake while fetching, or while an evaluation you started runs
//! (Settings → Desktop).
//!
//! 0.22.0 needed a hidden PowerShell to hold ES_SYSTEM_REQUIRED through SetThreadExecutionState, because Electron's own
//! blocker asks only for PowerRequestExecutionRequired, which does not stop idle sleep on Modern Standby PCs (most
//! current Windows 11 laptops). 1.0 makes the same call itself: one thread of the app holds
//! ES_CONTINUOUS | ES_SYSTEM_REQUIRED (| ES_DISPLAY_REQUIRED with "Keep the screen on too") for as long as it is wanted,
//! and lets go with ES_CONTINUOUS. The request is the thread's, so Windows drops it when the thread or the app ends.
//!
//! Windows still sleeps when you put it to sleep (lid, power button, Start → Sleep), and on battery a Modern Standby PC
//! ends these requests a few minutes after its sleep timeout.

use serde::Serialize;
use std::sync::{mpsc, Arc, Mutex};

pub const ES_CONTINUOUS: u32 = 0x8000_0000;
pub const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
pub const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct KeepAwakeState {
    pub active: bool,
    /// 'off' | 'starting' | 'held' | 'failed'
    pub system: &'static str,
    pub screen: bool,
    pub error: Option<String>,
}

/// SetThreadExecutionState on the calling thread.
pub trait Power: Send + Sync + 'static {
    fn set_thread_state(&self, flags: u32) -> Result<(), String>;
}

/// The real call (Windows); elsewhere it reports that it isn't done.
pub struct SystemPower;

impl Power for SystemPower {
    #[cfg(windows)]
    fn set_thread_state(&self, flags: u32) -> Result<(), String> {
        let prev = unsafe { windows_sys::Win32::System::Power::SetThreadExecutionState(flags) };
        if prev == 0 {
            Err("Windows refused the request".into())
        } else {
            Ok(())
        }
    }

    #[cfg(not(windows))]
    fn set_thread_state(&self, flags: u32) -> Result<(), String> {
        if flags == ES_CONTINUOUS {
            Ok(())
        } else {
            Err("keeping the computer awake is done on Windows only".into())
        }
    }
}

struct Inner {
    on: bool,
    screen: bool,
    holder: Option<mpsc::Sender<Option<u32>>>,
    /// Which holder thread the state below is about (a thread let go of may still report).
    generation: u64,
    held: bool,
    error: Option<String>,
}

pub type Changed = Arc<dyn Fn(KeepAwakeState) + Send + Sync>;

#[derive(Clone)]
pub struct KeepAwake {
    inner: Arc<Mutex<Inner>>,
    power: Arc<dyn Power>,
    changed: Changed,
    log: Arc<dyn Fn(&str) + Send + Sync>,
}

fn state_of(i: &Inner) -> KeepAwakeState {
    let system = if !i.on {
        "off"
    } else if i.held {
        "held"
    } else if i.error.is_some() {
        "failed"
    } else {
        "starting"
    };
    KeepAwakeState { active: i.on && i.held, system, screen: i.on && i.screen, error: if i.on { i.error.clone() } else { None } }
}

fn flags(screen: bool) -> u32 {
    ES_CONTINUOUS | ES_SYSTEM_REQUIRED | if screen { ES_DISPLAY_REQUIRED } else { 0 }
}

impl KeepAwake {
    pub fn new(power: Arc<dyn Power>, changed: Changed, log: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        KeepAwake {
            inner: Arc::new(Mutex::new(Inner { on: false, screen: false, holder: None, generation: 0, held: false, error: None })),
            power,
            changed,
            log,
        }
    }

    pub fn state(&self) -> KeepAwakeState {
        state_of(&self.inner.lock().unwrap())
    }

    /// Keep the computer awake (`on`), with the screen too (`screen`), or let go.
    pub fn set(&self, on: bool, screen: bool) {
        let mut i = self.inner.lock().unwrap();
        let was = state_of(&i);
        let flags_changed = i.on && on && i.screen != screen;
        i.on = on;
        i.screen = screen;
        if on {
            match &i.holder {
                Some(tx) if flags_changed => {
                    let _ = tx.send(Some(flags(screen)));
                    i.held = false;
                    i.error = None;
                }
                Some(_) => {}
                None => {
                    i.generation += 1;
                    i.held = false;
                    i.error = None;
                    let (tx, rx) = mpsc::channel::<Option<u32>>();
                    match self.spawn_holder(i.generation, flags(screen), rx) {
                        Ok(()) => i.holder = Some(tx),
                        Err(e) => i.error = Some(format!("the keep-awake thread could not start: {e}")),
                    }
                }
            }
        } else if let Some(tx) = i.holder.take() {
            let _ = tx.send(None);
            i.generation += 1;
            i.held = false;
            i.error = None;
        } else {
            i.held = false;
            i.error = None;
        }
        let now = state_of(&i);
        drop(i);
        if now != was {
            (self.changed)(now);
        }
    }

    fn spawn_holder(&self, generation: u64, first: u32, rx: mpsc::Receiver<Option<u32>>) -> std::io::Result<()> {
        let me = self.clone();
        let spawned = std::thread::Builder::new().name("keep-awake".into()).spawn(move || {
            let mut f = first;
            loop {
                let r = me.power.set_thread_state(f);
                me.report(generation, r, f);
                match rx.recv() {
                    Ok(Some(next)) => f = next,
                    _ => break,
                }
            }
            let _ = me.power.set_thread_state(ES_CONTINUOUS);
        });
        spawned.map(|_| ())
    }

    fn report(&self, generation: u64, r: Result<(), String>, f: u32) {
        let mut i = self.inner.lock().unwrap();
        if i.generation != generation || !i.on || flags(i.screen) != f {
            return;
        }
        let was = state_of(&i);
        match r {
            Ok(()) => {
                i.held = true;
                i.error = None;
            }
            Err(e) => {
                i.held = false;
                i.error = Some(e);
            }
        }
        let now = state_of(&i);
        drop(i);
        if now != was {
            if now.system == "held" {
                (self.log)(&format!("[keep awake] holding ES_SYSTEM_REQUIRED{}", if now.screen { " and ES_DISPLAY_REQUIRED" } else { "" }));
            } else if let Some(e) = &now.error {
                (self.log)(&format!("[keep awake] {e}"));
            }
            (self.changed)(now);
        }
    }
}
