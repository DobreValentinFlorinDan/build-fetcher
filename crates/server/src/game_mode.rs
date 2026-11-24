//! Port of `server/gameMode.ts`: champ select and games. While one is on, the app runs at below-normal priority, so
//! the game's threads win every contest for a core; when champ select starts, the memory the app no longer touches is
//! handed back to the system (Windows: the working-set trim). A trim that frees under 50 MB isn't done again that
//! session: the pages come back as soon as they are touched. (0.x also collected every thread's garbage first; Rust has
//! none to collect, so the "collected" figure is the one before the trim.)

use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const TRIM_MIN_BYTES: u64 = 50 * 1024 * 1024;

pub struct GameModeDeps {
    /// Below normal (true) or normal.
    pub set_priority: Box<dyn Fn(bool) -> Result<(), String> + Send + Sync>,
    pub rss: Box<dyn Fn() -> u64 + Send + Sync>,
    /// The working-set trim; false when it didn't run.
    pub trim: Box<dyn Fn() -> bool + Send + Sync>,
    pub log: Box<dyn Fn(&str) + Send + Sync>,
}

impl Default for GameModeDeps {
    fn default() -> Self {
        GameModeDeps {
            set_priority: Box::new(set_priority),
            rss: Box::new(|| irelia::sys::own_rss().unwrap_or(0)),
            trim: Box::new(trim),
            log: Box::new(|l| eprintln!("{l}")),
        }
    }
}

/// What the last clean-up did, for the log and the tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CleanUp {
    pub before: u64,
    pub after: u64,
    pub trimmed: bool,
}

struct Inner {
    deps: GameModeDeps,
    on: AtomicBool,
    trim_pays: AtomicBool,
    last: Mutex<Option<CleanUp>>,
}

pub struct GameMode {
    inner: Arc<Inner>,
    lock: Mutex<()>,
    cleaning: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Default for GameMode {
    fn default() -> Self {
        GameMode::new(GameModeDeps::default())
    }
}

impl GameMode {
    pub fn new(deps: GameModeDeps) -> Self {
        GameMode {
            inner: Arc::new(Inner { deps, on: AtomicBool::new(false), trim_pays: AtomicBool::new(true), last: Mutex::new(None) }),
            lock: Mutex::new(()),
            cleaning: Mutex::new(None),
        }
    }

    /// Champ select or a game began (true) or ended (false).
    pub fn set(&self, busy: bool) {
        let _g = self.lock.lock();
        if self.inner.on.swap(busy, Ordering::SeqCst) == busy {
            return;
        }
        if let Err(e) = (self.inner.deps.set_priority)(busy) {
            (self.inner.deps.log)(&format!("[game] could not set the app's priority: {e}"));
        }
        if busy {
            let mut c = self.cleaning.lock();
            if c.as_ref().map(|h| h.is_finished()).unwrap_or(true) {
                let inner = self.inner.clone();
                *c = Some(std::thread::spawn(move || clean_up(&inner)));
            }
        }
    }

    /// Waits for the clean-up of the last set(true), if it is still running.
    pub fn pending(&self) {
        let h = self.cleaning.lock().take();
        if let Some(h) = h {
            let _ = h.join();
        }
    }

    pub fn last(&self) -> Option<CleanUp> {
        *self.inner.last.lock()
    }
}

fn clean_up(inner: &Inner) {
    let d = &inner.deps;
    let before = (d.rss)();
    let mut trimmed = false;
    if inner.trim_pays.load(Ordering::SeqCst) && inner.on.load(Ordering::SeqCst) {
        trimmed = (d.trim)();
    }
    let after = (d.rss)();
    *inner.last.lock() = Some(CleanUp { before, after, trimmed });
    let mb = |b: u64| format!("{} MB", (b as f64 / 1_048_576.0).round());
    let line = format!("[game] champ select: {}", mb(before));
    if !trimmed {
        (d.log)(&format!("{line}."));
    } else if before.saturating_sub(after) < TRIM_MIN_BYTES {
        inner.trim_pays.store(false, Ordering::SeqCst);
        (d.log)(&format!("{line}, {} after the working-set trim: under {} freed, so it isn't done again.", mb(after), mb(TRIM_MIN_BYTES)));
    } else {
        (d.log)(&format!("{line}, {} after the working-set trim.", mb(after)));
    }
}

#[cfg(windows)]
fn set_priority(below: bool) -> Result<(), String> {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetPriorityClass, BELOW_NORMAL_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS};
    let ok = unsafe { SetPriorityClass(GetCurrentProcess(), if below { BELOW_NORMAL_PRIORITY_CLASS } else { NORMAL_PRIORITY_CLASS }) };
    if ok == 0 {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn trim() -> bool {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
    unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX) != 0 }
}

#[cfg(unix)]
fn set_priority(below: bool) -> Result<(), String> {
    // PRIORITY_BELOW_NORMAL is nice 10 on Unix (Node's os.setPriority). Lowering it back needs privileges there.
    unsafe extern "C" {
        fn setpriority(which: i32, who: u32, prio: i32) -> i32;
    }
    if unsafe { setpriority(0, 0, if below { 10 } else { 0 }) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

#[cfg(unix)]
fn trim() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    const MB: u64 = 1024 * 1024;

    #[test]
    fn below_normal_during_champ_select_and_the_game_normal_after_then_the_trim() {
        let priorities = Arc::new(Mutex::new(Vec::<bool>::new()));
        let order = Arc::new(Mutex::new(Vec::<&str>::new()));
        let rss = Arc::new(AtomicU64::new(900 * MB));
        let (p2, o2, r2, r3) = (priorities.clone(), order.clone(), rss.clone(), rss.clone());
        let mode = GameMode::new(GameModeDeps {
            set_priority: Box::new(move |b| {
                p2.lock().push(b);
                Ok(())
            }),
            rss: Box::new(move || r2.load(Ordering::SeqCst)),
            trim: Box::new(move || {
                o2.lock().push("trim");
                r3.fetch_sub(200 * MB, Ordering::SeqCst);
                true
            }),
            log: Box::new(|_| {}),
        });
        mode.set(true);
        mode.set(true); // no change: nothing again
        mode.pending();
        assert_eq!(*priorities.lock(), [true]);
        assert_eq!(*order.lock(), ["trim"]);
        assert_eq!(mode.last(), Some(CleanUp { before: 900 * MB, after: 700 * MB, trimmed: true }));
        mode.set(false);
        assert_eq!(*priorities.lock(), [true, false]);
        // It freed enough: the next champ select trims again.
        mode.set(true);
        mode.pending();
        assert_eq!(order.lock().iter().filter(|x| **x == "trim").count(), 2);
    }

    #[test]
    fn a_trim_that_frees_under_50_mb_is_not_done_again_and_a_failing_priority_call_is_logged() {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let trims = Arc::new(AtomicU64::new(0));
        let (l2, t2, t3) = (lines.clone(), trims.clone(), trims.clone());
        let mode = GameMode::new(GameModeDeps {
            set_priority: Box::new(|_| Err("denied".into())),
            rss: Box::new(move || 500 * MB - t2.load(Ordering::SeqCst) * (TRIM_MIN_BYTES - MB)),
            trim: Box::new(move || {
                t3.fetch_add(1, Ordering::SeqCst);
                true
            }),
            log: Box::new(move |l| l2.lock().push(l.to_string())),
        });
        mode.set(true);
        mode.pending();
        mode.set(false);
        mode.set(true);
        mode.pending();
        mode.set(false);
        assert_eq!(trims.load(Ordering::SeqCst), 1, "not again after it freed too little");
        assert!(lines.lock().iter().any(|l| l.contains("could not set the app's priority: denied")));
        assert!(lines.lock().iter().any(|l| l.contains("isn't done again")));
    }
}
