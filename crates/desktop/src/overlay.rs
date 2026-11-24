//! The pure parts of `desktop/overlay.ts`: where the HUD window goes, how tall it may be, and its state as the pages
//! see it (`OverlayState`). The window itself is desktop/tauri's.
//!
//! Vanguard / Riot policy notes — this is deliberately an ordinary desktop window: nothing is injected into or hooked
//! from the League process (no DLLs, no DirectX hooks, no memory reads); it sits above the game the way any topmost
//! window does (Borderless or Windowed mode); it never takes focus and is click-through by default; hotkeys use
//! RegisterHotKey, never a keyboard hook, and are registered only while a game runs; nothing is ever sent to the game.

use crate::settings::OverlayBounds;
use serde::Serialize;

/// Matches the HUD's w-[300px].
pub const WIDTH: f64 = 300.0;
pub const MIN_H: f64 = 40.0;
pub const MAX_H: f64 = 900.0;
pub const START_H: f64 = 360.0;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlayState {
    pub visible: bool,
    pub click_through: bool,
    pub in_game: bool,
    /// Accelerators Windows refused (already taken by another app).
    pub hotkey_errors: Vec<String>,
}

/// A display's work area, in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Area {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// `initialPosition`: the saved spot when it is still on a display, else the right edge of the primary display, below
/// the scoreboard corner (clear of the minimap, shop and ability bar).
pub fn initial_position(saved: Option<OverlayBounds>, displays: &[Area], primary: Area) -> (f64, f64) {
    if let Some(s) = saved {
        let on_screen = displays
            .iter()
            .any(|a| s.x >= a.x - 50.0 && s.y >= a.y - 50.0 && s.x + 50.0 <= a.x + a.width && s.y + 50.0 <= a.y + a.height);
        if on_screen {
            return (s.x, s.y);
        }
    }
    (primary.x + primary.width - WIDTH - 24.0, primary.y + irelia::js::round(primary.height * 0.18))
}

/// `fitHeight`: the HUD reports its rendered height; the window follows within bounds. None for a non-number.
pub fn fit_height(px: f64) -> Option<f64> {
    px.is_finite().then(|| irelia::js::round(px).clamp(MIN_H, MAX_H))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FHD: Area = Area { x: 0.0, y: 0.0, width: 1920.0, height: 1040.0 };

    #[test]
    fn placement() {
        assert_eq!(initial_position(None, &[FHD], FHD), (1596.0, 187.0));
        let saved = OverlayBounds { x: 100.0, y: 200.0, width: 300.0 };
        assert_eq!(initial_position(Some(saved), &[FHD], FHD), (100.0, 200.0));
        let gone = OverlayBounds { x: 2500.0, y: 200.0, width: 300.0 };
        assert_eq!(initial_position(Some(gone), &[FHD], FHD), (1596.0, 187.0), "on a display that is no longer there");
        assert_eq!(fit_height(12.0), Some(40.0));
        assert_eq!(fit_height(420.4), Some(420.0));
        assert_eq!(fit_height(5000.0), Some(900.0));
        assert_eq!(fit_height(f64::NAN), None);
    }
}
