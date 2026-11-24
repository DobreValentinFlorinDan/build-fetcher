//! From irelia-companion's `desktop/settings.ts`: the desktop settings, kept under `desktop` in
//! %APPDATA%/build-fetcher/settings.json, merged over the defaults. A patch from a page is untrusted: every field is
//! validated, and a field that isn't valid falls back to its default.

use bf_core::app_config::{read_app_config, update_app_config};
use bf_core::js;
use serde::Serialize;
use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct OverlayBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlaySettings {
    /// Show the HUD when a game starts, hide it when the game ends.
    pub auto_show: bool,
    /// Start every game click-through (the mouse goes to the game).
    pub click_through: bool,
    /// 0.4 .. 1
    pub opacity: f64,
    /// Last position; unset = right edge of the primary display.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<OverlayBounds>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hotkeys {
    pub toggle_overlay: String,
    pub toggle_click_through: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopSettings {
    pub overlay: OverlaySettings,
    /// Accelerators. Registered only while a game is running, so they never shadow other apps.
    pub hotkeys: Hotkeys,
    pub close_to_tray: bool,
    /// Windows doesn't go to sleep while fetching runs.
    pub keep_awake_while_fetching: bool,
    /// ...and the screen stays on too.
    pub keep_screen_on_while_fetching: bool,
}

/// `DEFAULT_DESKTOP_SETTINGS`.
pub fn defaults() -> DesktopSettings {
    DesktopSettings {
        overlay: OverlaySettings { auto_show: true, click_through: true, opacity: 0.92, bounds: None },
        hotkeys: Hotkeys { toggle_overlay: "Shift+Tab".into(), toggle_click_through: "Ctrl+Shift+Tab".into() },
        close_to_tray: true,
        keep_awake_while_fetching: true,
        keep_screen_on_while_fetching: false,
    }
}

impl DesktopSettings {
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

fn overlay_value(o: &OverlaySettings) -> Map<String, Value> {
    match serde_json::to_value(o) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

fn hotkeys_value(h: &Hotkeys) -> Map<String, Value> {
    match serde_json::to_value(h) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

/// `{ ...cur, ...patch }` for one nested object (a patch that isn't an object adds nothing).
fn spread(mut cur: Map<String, Value>, patch: Option<&Value>) -> Map<String, Value> {
    if let Some(Value::Object(p)) = patch {
        for (k, v) in p {
            cur.insert(k.clone(), v.clone());
        }
    }
    cur
}

/// `Number.isFinite(v)`: a finite number (never a string).
fn finite(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|x| x.is_finite())
}

/// An accelerator as the settings accept it: `/^[\w+]{1,40}$/`.
fn accel(v: Option<&Value>, fallback: &str) -> String {
    match v.and_then(Value::as_str) {
        Some(s) if (1..=40).contains(&s.chars().count()) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '+') => s.to_string(),
        _ => fallback.to_string(),
    }
}

/// `mergeDesktopSettings`: applies a (page-supplied, so untrusted) patch with every field validated.
pub fn merge_desktop_settings(cur: &DesktopSettings, patch: &Value) -> DesktopSettings {
    let d = defaults();
    let empty = Map::new();
    let p = patch.as_object().unwrap_or(&empty);
    let o = spread(overlay_value(&cur.overlay), p.get("overlay"));
    let h = spread(hotkeys_value(&cur.hotkeys), p.get("hotkeys"));
    let bool_or = |v: Option<&Value>, fallback: bool| v.and_then(Value::as_bool).unwrap_or(fallback);
    let bounds = match o.get("bounds") {
        Some(b) if js::truthy(b) => match (finite(b.get("x")), finite(b.get("y")), finite(b.get("width"))) {
            (Some(x), Some(y), Some(width)) => Some(OverlayBounds { x: js::round(x) + 0.0, y: js::round(y) + 0.0, width: js::round(width) + 0.0 }),
            _ => None,
        },
        _ => None,
    };
    DesktopSettings {
        overlay: OverlaySettings {
            auto_show: bool_or(o.get("autoShow"), d.overlay.auto_show),
            click_through: bool_or(o.get("clickThrough"), d.overlay.click_through),
            opacity: finite(o.get("opacity")).map(|x| x.clamp(0.4, 1.0)).unwrap_or(d.overlay.opacity),
            bounds,
        },
        hotkeys: Hotkeys {
            toggle_overlay: accel(h.get("toggleOverlay"), &d.hotkeys.toggle_overlay),
            toggle_click_through: accel(h.get("toggleClickThrough"), &d.hotkeys.toggle_click_through),
        },
        close_to_tray: bool_or(p.get("closeToTray"), cur.close_to_tray),
        keep_awake_while_fetching: bool_or(p.get("keepAwakeWhileFetching"), cur.keep_awake_while_fetching),
        keep_screen_on_while_fetching: bool_or(p.get("keepScreenOnWhileFetching"), cur.keep_screen_on_while_fetching),
    }
}

/// `loadDesktopSettings`: what config.json holds under `desktop`, merged over the defaults.
pub fn load_desktop_settings() -> DesktopSettings {
    let stored = read_app_config().get("desktop").cloned().unwrap_or(Value::Object(Map::new()));
    merge_desktop_settings(&defaults(), &stored)
}

/// `saveDesktopSettings`.
pub fn save_desktop_settings(s: &DesktopSettings) {
    update_app_config(vec![("desktop", Some(s.to_value()))]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fields_are_validated_one_by_one() {
        let d = defaults();
        let m = merge_desktop_settings(&d, &json!({
            "overlay": { "opacity": 0.1, "autoShow": "yes", "bounds": { "x": 10.4, "y": -2.5, "width": 300 } },
            "hotkeys": { "toggleOverlay": "Alt+F1", "toggleClickThrough": "Ctrl Shift" },
        }));
        assert_eq!(m.overlay.opacity, 0.4);
        assert!(m.overlay.auto_show, "not a boolean: the default");
        assert_eq!(m.overlay.bounds, Some(OverlayBounds { x: 10.0, y: -2.0, width: 300.0 }));
        assert_eq!(m.hotkeys.toggle_overlay, "Alt+F1");
        assert_eq!(m.hotkeys.toggle_click_through, "Ctrl+Shift+Tab");
        let m = merge_desktop_settings(&m, &json!({ "overlay": { "opacity": "1", "bounds": { "x": 1, "y": 2 } } }));
        assert_eq!(m.overlay.opacity, 0.92, "a string is not a number");
        assert_eq!(m.overlay.bounds, None);
    }

    #[test]
    fn saved_form() {
        let mut s = defaults();
        assert_eq!(
            js::stringify(&s.to_value()),
            r#"{"overlay":{"autoShow":true,"clickThrough":true,"opacity":0.92},"hotkeys":{"toggleOverlay":"Shift+Tab","toggleClickThrough":"Ctrl+Shift+Tab"},"closeToTray":true,"keepAwakeWhileFetching":true,"keepScreenOnWhileFetching":false}"#
        );
        s.overlay.bounds = Some(OverlayBounds { x: 1600.0, y: 194.0, width: 300.0 });
        assert!(js::stringify(&s.to_value()).contains(r#""bounds":{"x":1600,"y":194,"width":300}"#));
    }
}
