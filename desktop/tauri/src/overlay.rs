//! From irelia-companion's `desktop/overlay.ts`: the in-game HUD, a separate transparent, frameless, always-on-top window loading
//! /overlay. It never takes focus (WS_EX_NOACTIVATE, shown with SW_SHOWNOACTIVATE) and is click-through by default; the
//! hotkeys (RegisterHotKey, through the global-shortcut plugin) are registered only while a game runs; the window
//! exists only while a game runs (or you show it), so idle memory stays low. See crates/desktop/src/overlay.rs for the
//! Vanguard notes.
//!
//! Window work runs on the main thread (tray and hotkey events, `Shell::on_main`). The two IPC commands that can
//! create/show the overlay are async: a sync command runs in the main-thread IPC callback, and creating a WebView
//! window there deadlocks on Windows.

use crate::Shell;
use bf_desktop::overlay::{fit_height, initial_position, Area, OverlayState, START_H, WIDTH};
use bf_desktop::settings::{self, OverlayBounds};
use std::sync::Arc;
use std::time::Duration;
use tauri::{LogicalSize, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

pub const LABEL: &str = "overlay";

pub struct Overlay {
    visible: bool,
    click_through: bool,
    in_game: bool,
    hotkeys: Vec<String>,
    hotkey_errors: Vec<String>,
    win: Option<WebviewWindow>,
    /// Which window the events are about (a destroyed one may still report).
    generation: u64,
    /// The last move, for the 400 ms debounce of saving the position.
    moves: u64,
}

impl Overlay {
    pub fn new(click_through: bool) -> Self {
        Overlay { visible: false, click_through, in_game: false, hotkeys: vec![], hotkey_errors: vec![], win: None, generation: 0, moves: 0 }
    }

    pub fn state(&self) -> OverlayState {
        OverlayState { visible: self.visible, click_through: self.click_through, in_game: self.in_game, hotkey_errors: self.hotkey_errors.clone() }
    }
}

/// The cursor in the overlay page's CSS pixels, when it is over the window.
fn pointer_in(win: &WebviewWindow) -> Option<(f64, f64)> {
    let c = win.cursor_position().ok()?;
    let p = win.inner_position().ok()?;
    let s = win.inner_size().ok()?;
    let scale = win.scale_factor().unwrap_or(1.0);
    let (x, y) = (c.x - p.x as f64, c.y - p.y as f64);
    (x >= 0.0 && y >= 0.0 && x < s.width as f64 && y < s.height as f64).then(|| (x / scale, y / scale))
}

fn area(m: &tauri::Monitor) -> Area {
    let s = m.scale_factor();
    let w = m.work_area();
    Area {
        x: w.position.x as f64 / s,
        y: w.position.y as f64 / s,
        width: w.size.width as f64 / s,
        height: w.size.height as f64 / s,
    }
}

impl Shell {
    fn emit_overlay(&self) {
        let s = self.overlay.lock().unwrap().state();
        self.broadcast("bf:overlay-state", &s);
        self.update_tray_menu(s.visible, s.click_through);
    }

    /// Driven by the server's live-game events.
    pub(crate) fn overlay_set_in_game(self: &Arc<Self>, active: bool) {
        {
            let mut o = self.overlay.lock().unwrap();
            if o.in_game == active {
                return;
            }
            o.in_game = active;
        }
        eprintln!("[overlay] {}", if active { "game started" } else { "game ended" });
        if active {
            let s = self.settings();
            self.overlay.lock().unwrap().click_through = s.overlay.click_through;
            self.register_hotkeys();
            if s.overlay.auto_show {
                self.overlay_show();
            }
        } else {
            self.unregister_hotkeys();
            self.overlay_destroy(); // frees the renderer between games
        }
        self.emit_overlay();
    }

    pub(crate) fn overlay_set_visible(self: &Arc<Self>, visible: bool) {
        if visible {
            self.overlay_show();
        } else {
            self.overlay_hide();
        }
        self.emit_overlay();
    }

    pub(crate) fn overlay_set_click_through(self: &Arc<Self>, on: bool) {
        self.overlay.lock().unwrap().click_through = on;
        self.apply_mouse_mode();
        self.emit_overlay();
    }

    /// The HUD reports its rendered height; the window follows so no transparent area covers the game.
    pub(crate) fn overlay_fit_height(&self, px: f64) {
        let Some(win) = self.overlay.lock().unwrap().win.clone() else { return };
        let Some(height) = fit_height(px) else { return };
        let scale = win.scale_factor().unwrap_or(1.0);
        let current = win.inner_size().map(|s| s.to_logical::<f64>(scale)).ok();
        if current.map(|c| (c.height - height).abs() >= 0.5).unwrap_or(true) {
            let _ = win.set_size(LogicalSize::new(WIDTH, height));
        }
    }

    /// Re-applies opacity and hotkeys after a settings change.
    pub(crate) fn overlay_apply_settings(self: &Arc<Self>) {
        let (win, in_game) = {
            let o = self.overlay.lock().unwrap();
            (o.win.clone(), o.in_game)
        };
        if let Some(w) = win {
            self.apply_opacity(&w);
        }
        if in_game {
            self.register_hotkeys();
        }
        self.emit_overlay();
    }

    pub(crate) fn overlay_dispose(&self) {
        self.unregister_hotkeys();
        self.overlay_destroy();
    }

    // -------------------------------------------------------------- window

    /// The HUD's opacity: the page's (Electron set the window's; a transparent WebView2 window draws through
    /// DirectComposition, so the page fades instead, with the same result).
    fn apply_opacity(&self, w: &WebviewWindow) {
        let opacity = self.settings().overlay.opacity;
        let _ = w.eval(format!("document.documentElement.style.opacity = '{opacity}'"));
    }

    fn overlay_create(self: &Arc<Self>) -> Option<WebviewWindow> {
        let origin = self.origin();
        if origin.is_empty() {
            return None;
        }
        let saved = self.settings().overlay.bounds;
        let displays: Vec<Area> = self.app.available_monitors().unwrap_or_default().iter().map(area).collect();
        let primary = self.app.primary_monitor().ok().flatten().map(|m| area(&m)).or(displays.first().copied()).unwrap_or(Area {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1040.0,
        });
        let (x, y) = initial_position(saved, &displays, primary);
        let generation = {
            let mut o = self.overlay.lock().unwrap();
            o.generation += 1;
            o.generation
        };
        let nav = self.clone();
        let loaded = self.clone();
        let built = WebviewWindowBuilder::new(&self.app, LABEL, WebviewUrl::External(format!("{origin}/overlay").parse().ok()?))
            .title("Build Fetcher Overlay")
            .position(x, y)
            .inner_size(WIDTH, START_H)
            .transparent(true)
            .decorations(false)
            .shadow(false)
            .resizable(false)
            .maximizable(false)
            .minimizable(false)
            .skip_taskbar(true)
            .always_on_top(true)
            .focusable(false)
            .focused(false)
            .visible(false)
            .initialization_script(self.bridge.as_str())
            .on_navigation(move |url| nav.allowed(url))
            .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
            .on_page_load(move |w, p| {
                if matches!(p.event(), tauri::webview::PageLoadEvent::Finished) {
                    loaded.apply_opacity(&w);
                }
            })
            .build();
        let win = match built {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[overlay] could not open the overlay: {e}");
                return None;
            }
        };
        let me = self.clone();
        win.on_window_event(move |e| match e {
            WindowEvent::Moved(_) => {
                let me2 = me.clone();
                me.on_main(move |s| s.overlay_moved(generation, &me2));
            }
            WindowEvent::Destroyed => me.on_main(move |s| {
                let mut o = s.overlay.lock().unwrap();
                if o.generation == generation && o.win.is_some() {
                    o.win = None;
                    o.visible = false;
                    drop(o);
                    s.emit_overlay();
                }
            }),
            _ => {}
        });
        self.overlay.lock().unwrap().win = Some(win.clone());
        self.follow_pointer(generation);
        Some(win)
    }

    /// Tooltips while click-through is on (0.x had Electron forward mouse moves, through a mouse hook): the cursor's
    /// position, read every 100 ms (GetCursorPos, no hook), goes to the overlay page while it is over the HUD, and
    /// bridge.js shows the tooltip of what is under it. Clicks still go to the game.
    fn follow_pointer(self: &Arc<Self>, generation: u64) {
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut last: Option<(i64, i64)> = None;
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let (win, on) = {
                    let o = me.overlay.lock().unwrap();
                    if o.generation != generation {
                        break;
                    }
                    match &o.win {
                        Some(w) => (w.clone(), o.visible && o.click_through),
                        None => break,
                    }
                };
                let at = if on { pointer_in(&win) } else { None };
                let key = at.map(|(x, y)| (x.round() as i64, y.round() as i64));
                if key == last {
                    continue;
                }
                last = key;
                let detail = match at {
                    Some((x, y)) => format!("{{ \"x\": {x}, \"y\": {y} }}"),
                    None => "{ \"x\": null, \"y\": null }".to_string(),
                };
                let _ = win.eval(format!("window.dispatchEvent(new CustomEvent('bf:pointer', {{ detail: {detail} }}))"));
            }
        });
    }

    /// Saves the position 400 ms after the last move.
    fn overlay_moved(&self, generation: u64, me: &Arc<Shell>) {
        let seq = {
            let mut o = self.overlay.lock().unwrap();
            if o.generation != generation {
                return;
            }
            o.moves += 1;
            o.moves
        };
        let me = me.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            me.clone().on_main(move |s| {
                let win = {
                    let o = s.overlay.lock().unwrap();
                    if o.moves != seq || o.generation != generation {
                        return;
                    }
                    o.win.clone()
                };
                let Some(win) = win else { return };
                let scale = win.scale_factor().unwrap_or(1.0);
                let (Ok(pos), Ok(size)) = (win.outer_position(), win.outer_size()) else { return };
                let pos = pos.to_logical::<f64>(scale);
                let size = size.to_logical::<f64>(scale);
                let bounds = OverlayBounds { x: pos.x.round(), y: pos.y.round(), width: size.width.round() };
                let next = {
                    let mut st = s.settings.lock().unwrap();
                    let mut patch = serde_json::json!({ "overlay": st.overlay.clone() });
                    patch["overlay"]["bounds"] = serde_json::to_value(bounds).unwrap_or_default();
                    *st = settings::merge_desktop_settings(&st, &patch);
                    st.clone()
                };
                settings::save_desktop_settings(&next);
            });
        });
    }

    fn overlay_show(self: &Arc<Self>) {
        let existing = self.overlay.lock().unwrap().win.clone();
        let Some(win) = existing.or_else(|| self.overlay_create()) else { return };
        // Not focusable: shown without activating, so the game keeps the focus.
        let _ = win.show();
        let _ = win.set_always_on_top(true);
        self.overlay.lock().unwrap().visible = true;
        self.apply_mouse_mode();
    }

    fn overlay_hide(&self) {
        let win = self.overlay.lock().unwrap().win.clone();
        if let Some(w) = win {
            let _ = w.hide();
        }
        self.overlay.lock().unwrap().visible = false;
    }

    fn overlay_destroy(&self) {
        let win = {
            let mut o = self.overlay.lock().unwrap();
            o.visible = false;
            o.win.take()
        };
        if let Some(w) = win {
            let _ = w.destroy();
        }
    }

    fn apply_mouse_mode(&self) {
        let (win, click_through) = {
            let o = self.overlay.lock().unwrap();
            (o.win.clone(), o.click_through)
        };
        let Some(win) = win else { return };
        // A window not shown yet takes it when it is (GTK has no surface to shape before that).
        if !win.is_visible().unwrap_or(false) {
            return;
        }
        let _ = win.set_ignore_cursor_events(click_through);
        let _ = win.set_focusable(!click_through);
    }

    // -------------------------------------------------------------- hotkeys

    fn register_hotkeys(self: &Arc<Self>) {
        self.unregister_hotkeys();
        let hk = self.settings().hotkeys;
        let gs = self.app.global_shortcut();
        let mut ok = vec![];
        let mut errors = vec![];
        let toggle_overlay = self.clone();
        let toggle_click = self.clone();
        let pairs: [(String, Box<dyn Fn() + Send + Sync>); 2] = [
            (
                hk.toggle_overlay.clone(),
                Box::new(move || {
                    toggle_overlay.on_main(|s| {
                        let v = s.overlay.lock().unwrap().visible;
                        s.overlay_set_visible(!v);
                    })
                }),
            ),
            (
                hk.toggle_click_through.clone(),
                Box::new(move || {
                    toggle_click.on_main(|s| {
                        if !s.overlay.lock().unwrap().visible {
                            s.overlay_show();
                        }
                        let c = s.overlay.lock().unwrap().click_through;
                        s.overlay_set_click_through(!c);
                    })
                }),
            ),
        ];
        for (accel, f) in pairs {
            let registered = gs.on_shortcut(accel.as_str(), move |_, _, e| {
                if e.state == ShortcutState::Pressed {
                    f();
                }
            });
            match registered {
                Ok(()) => ok.push(accel),
                Err(e) => {
                    eprintln!("[overlay] hotkey {accel} not registered: {e}");
                    errors.push(accel);
                }
            }
        }
        let mut o = self.overlay.lock().unwrap();
        o.hotkeys = ok;
        o.hotkey_errors = errors;
    }

    fn unregister_hotkeys(&self) {
        let keys = std::mem::take(&mut self.overlay.lock().unwrap().hotkeys);
        let gs = self.app.global_shortcut();
        for k in keys {
            let _ = gs.unregister(k.as_str());
        }
    }
}
