//! The server's log lines: standard error (the desktop app sends it to logs/build-fetcher.log), and a hook the tests
//! use to check that no line ever carries an API key.

use parking_lot::RwLock;
use std::sync::Arc;

type Hook = Arc<dyn Fn(&str) + Send + Sync>;

static HOOK: RwLock<Option<Hook>> = RwLock::new(None);

pub fn line(s: &str) {
    eprintln!("{s}");
    let hook = HOOK.read().clone();
    if let Some(h) = hook {
        h(s);
    }
}

/// Every line from now on also goes to `f` (tests).
pub fn set_hook(f: Option<Hook>) {
    *HOOK.write() = f;
}

/// `bf_log!("[live] {x}")`: a log line.
#[macro_export]
macro_rules! bf_log {
    ($($t:tt)*) => { $crate::logging::line(&format!($($t)*)) };
}
