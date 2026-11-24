//! What the user is doing (`Activity`), for the fetch scheduler and the learner, which pause or slow down outside
//! Idle: InGame and ChampSelect from the League client's gameflow phase and the Live Client; otherwise Busy while the
//! PC was used in the last few minutes (Windows' last-input time), else Idle. Published on a `tokio::sync::watch`.

use bf_core::build::Activity;
use parking_lot::Mutex;
use tokio::sync::watch;

/// Input within this many seconds counts as the PC being in use.
pub const BUSY_WITHIN_S: u64 = 180;
/// How often the OS idle time is read.
pub const IDLE_POLL_S: u64 = 5;

const IN_GAME_PHASES: [&str; 3] = ["GameStart", "InProgress", "Reconnect"];

/// The rule, on its own: a game (by the client's phase or the Live Client answering) beats champ select, which beats
/// the PC's idle time. `idle_s` None (not Windows, or the call failed) counts as Idle: heavy work then runs whenever
/// League doesn't.
pub fn activity_of(phase: &str, live_game: bool, idle_s: Option<u64>) -> Activity {
    if live_game || IN_GAME_PHASES.contains(&phase) {
        Activity::InGame
    } else if phase == "ChampSelect" {
        Activity::ChampSelect
    } else {
        match idle_s {
            Some(s) if s < BUSY_WITHIN_S => Activity::Busy,
            _ => Activity::Idle,
        }
    }
}

#[derive(Default)]
struct Inputs {
    phase: String,
    live_game: bool,
    idle_s: Option<u64>,
}

/// Holds the inputs and publishes the activity when it changes.
pub struct ActivityDetector {
    inputs: Mutex<Inputs>,
    tx: watch::Sender<Activity>,
}

impl Default for ActivityDetector {
    fn default() -> Self {
        ActivityDetector { inputs: Mutex::new(Inputs::default()), tx: watch::channel(Activity::Idle).0 }
    }
}

impl ActivityDetector {
    pub fn subscribe(&self) -> watch::Receiver<Activity> {
        self.tx.subscribe()
    }

    pub fn current(&self) -> Activity {
        *self.tx.borrow()
    }

    /// Recomputes after `f` changed an input; Some(new activity) when it changed.
    fn update(&self, f: impl FnOnce(&mut Inputs)) -> Option<Activity> {
        let next = {
            let mut i = self.inputs.lock();
            f(&mut i);
            activity_of(&i.phase, i.live_game, i.idle_s)
        };
        // send_if_modified: subscribers wake only on a real change; the lock above isn't held across it.
        let changed = self.tx.send_if_modified(|a| std::mem::replace(a, next) != next);
        changed.then_some(next)
    }

    pub fn set_phase(&self, phase: &str) -> Option<Activity> {
        self.update(|i| i.phase = phase.to_string())
    }
    pub fn set_live_game(&self, on: bool) -> Option<Activity> {
        self.update(|i| i.live_game = on)
    }
    pub fn set_idle_seconds(&self, s: Option<u64>) -> Option<Activity> {
        self.update(|i| i.idle_s = s)
    }
}

/// Seconds since the last keyboard or mouse input (Windows: GetLastInputInfo). None elsewhere.
#[cfg(windows)]
pub fn os_idle_seconds() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    // SAFETY: a correctly sized LASTINPUTINFO on the stack; the call only writes into it.
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return None;
    }
    // Both are milliseconds since boot in 32 bits; wrapping_sub handles the 49.7-day wrap.
    let now = unsafe { GetTickCount() };
    Some(now.wrapping_sub(info.dwTime) as u64 / 1000)
}

#[cfg(not(windows))]
pub fn os_idle_seconds() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rule() {
        assert_eq!(activity_of("None", false, None), Activity::Idle, "no idle time (not Windows): Idle");
        assert_eq!(activity_of("Lobby", false, Some(10)), Activity::Busy);
        assert_eq!(activity_of("Lobby", false, Some(BUSY_WITHIN_S)), Activity::Idle);
        assert_eq!(activity_of("ChampSelect", false, Some(1)), Activity::ChampSelect);
        assert_eq!(activity_of("InProgress", false, Some(1)), Activity::InGame);
        assert_eq!(activity_of("Reconnect", false, None), Activity::InGame);
        assert_eq!(activity_of("None", true, Some(9999)), Activity::InGame, "the Live Client answering is a game");
        assert_eq!(activity_of("EndOfGame", false, Some(5)), Activity::Busy);
    }

    #[tokio::test]
    async fn transitions_through_a_game_are_published_once_each() {
        let d = ActivityDetector::default();
        let mut rx = d.subscribe();
        let mut seen = vec![];
        let steps: Vec<Box<dyn Fn(&ActivityDetector) -> Option<Activity>>> = vec![
            Box::new(|d| d.set_idle_seconds(Some(4))),        // the user is at the PC
            Box::new(|d| d.set_phase("Lobby")),               // no change: still Busy
            Box::new(|d| d.set_phase("ChampSelect")),
            Box::new(|d| d.set_phase("ChampSelect")),         // repeated event: no change
            Box::new(|d| d.set_phase("GameStart")),
            Box::new(|d| d.set_live_game(true)),              // still InGame
            Box::new(|d| d.set_phase("InProgress")),
            Box::new(|d| d.set_idle_seconds(Some(600))),      // no input during the game: still InGame
            Box::new(|d| d.set_live_game(false)),
            Box::new(|d| d.set_phase("EndOfGame")),           // back to the PC's idle time: Idle (600 s)
            Box::new(|d| d.set_idle_seconds(Some(2))),        // the user moves the mouse
            Box::new(|d| d.set_phase("None")),
        ];
        for s in &steps {
            if let Some(a) = s(&d) {
                assert!(rx.has_changed().unwrap());
                assert_eq!(*rx.borrow_and_update(), a);
                seen.push(a);
            } else {
                assert!(!rx.has_changed().unwrap(), "no change, no wake-up");
            }
        }
        assert_eq!(seen, vec![Activity::Busy, Activity::ChampSelect, Activity::InGame, Activity::Idle, Activity::Busy]);
        assert_eq!(d.current(), Activity::Busy);
    }

    #[test]
    fn a_dodge_goes_back_to_idle_or_busy() {
        let d = ActivityDetector::default();
        assert_eq!(d.set_phase("ChampSelect"), Some(Activity::ChampSelect));
        assert_eq!(d.set_phase("Lobby"), Some(Activity::Idle));
    }

    #[test]
    fn os_idle_time_is_none_off_windows() {
        if !cfg!(windows) {
            assert_eq!(os_idle_seconds(), None);
        }
    }
}
