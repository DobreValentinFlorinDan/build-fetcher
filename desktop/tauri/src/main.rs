//! Build Fetcher's desktop app (from irelia-companion's `desktop/main.ts`): the server (crates/server) in this
//! process, the dashboard window, the in-game overlay (created per game), the tray, single instance, keeping the
//! computer awake while fetching, and `window.buildFetcher` for the pages.
//!
//!   cargo run -p build-fetcher               development build: dashboard from ./dist
//!   (CI) tauri build                         the NSIS installer (release.yml)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod overlay;
mod renderers;

use bf_desktop::feed::{self, Feed};
use bf_desktop::keep_awake::{KeepAwake, KeepAwakeState, SystemPower};
use bf_desktop::settings::{self, DesktopSettings};
use bf_desktop::{log, priority, DesktopInfo, PREFERRED_PORT};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::webview::NewWindowResponse;
use tauri::{AppHandle, Manager, RunEvent, State, Webview, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::DialogExt;

const BRIDGE: &str = include_str!("bridge.js");
const MAIN: &str = "main";

#[derive(Default, Clone, Copy)]
struct Flags {
    /// The server's fetching is on (its `status` event).
    fetching: bool,
    /// Champ select or a game (its `activity` event).
    playing: bool,
}

struct TrayParts {
    icon: TrayIcon,
    show_overlay: CheckMenuItem<tauri::Wry>,
    click_through: CheckMenuItem<tauri::Wry>,
}

pub struct Shell {
    app: AppHandle,
    settings: Mutex<DesktopSettings>,
    /// The server's origin once it listens (http://127.0.0.1:port); empty until then.
    origin: Mutex<String>,
    quitting: AtomicBool,
    overlay: Mutex<overlay::Overlay>,
    keep_awake: KeepAwake,
    flags: Mutex<Flags>,
    tray: Mutex<Option<TrayParts>>,
    info: DesktopInfo,
    /// window.buildFetcher (bridge.js), injected into the app's own pages.
    bridge: String,
    watcher: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KeepAwakeView {
    #[serde(flatten)]
    state: KeepAwakeState,
    fetching: bool,
}

impl Shell {
    fn origin(&self) -> String {
        self.origin.lock().unwrap().clone()
    }

    fn settings(&self) -> DesktopSettings {
        self.settings.lock().unwrap().clone()
    }

    /// Runs `f` on the main thread, later (window, tray and overlay work happens there, one thing at a time).
    fn on_main(self: &Arc<Self>, f: impl FnOnce(&Arc<Shell>) + Send + 'static) {
        let me = self.clone();
        let _ = self.app.run_on_main_thread(move || f(&me));
    }

    /// Every page of the app: dashboard and overlay.
    fn broadcast(&self, event: &str, detail: &impl Serialize) {
        let detail = serde_json::to_string(detail).unwrap_or_else(|_| "null".into());
        let js = format!("window.dispatchEvent(new CustomEvent({}, {{ detail: {detail} }}))", serde_json::to_string(event).unwrap());
        for w in self.app.webview_windows().values() {
            let _ = w.eval(js.as_str());
        }
    }

    // -------------------------------------------------------------- windows

    /// Only the app's own pages (and the splash) load in its windows.
    fn allowed(&self, url: &url::Url) -> bool {
        let origin = self.origin();
        if !origin.is_empty() && url.origin().ascii_serialization() == origin {
            return true;
        }
        url.scheme() == "tauri" || url.host_str() == Some("tauri.localhost")
    }

    fn create_main(self: &Arc<Self>) -> Option<WebviewWindow> {
        let origin = self.origin();
        let url = if origin.is_empty() {
            WebviewUrl::App("index.html".into())
        } else {
            WebviewUrl::External(format!("{origin}/").parse().ok()?)
        };
        let nav = self.clone();
        let loaded = self.clone();
        let built = WebviewWindowBuilder::new(&self.app, MAIN, url)
            .title("Build Fetcher")
            .inner_size(1360.0, 900.0)
            .min_inner_size(960.0, 640.0)
            .background_color(tauri::webview::Color(5, 5, 6, 255))
            .visible(false)
            .initialization_script(self.bridge.as_str())
            .on_navigation(move |url| nav.allowed(url))
            .on_new_window(|url, _| {
                // External links (Riot developer portal, ...) open in the default browser, never in the app.
                if url.scheme() == "https" {
                    let _ = tauri_plugin_opener::open_url(url.as_str(), None::<&str>);
                }
                NewWindowResponse::Deny
            })
            .on_page_load(move |w, p| {
                if matches!(p.event(), tauri::webview::PageLoadEvent::Finished) {
                    if !w.is_visible().unwrap_or(true) {
                        let _ = w.show();
                    }
                    loaded.dashboard_priority();
                    #[cfg(debug_assertions)]
                    if std::env::var_os("BF_SELFTEST").is_some() && w.url().map(|u| u.scheme() == "http").unwrap_or(false) {
                        selftest(w.clone());
                    }
                }
            });
        let built = match self.app.default_window_icon() {
            Some(icon) => built.icon(icon.clone()).ok()?,
            None => built,
        };
        let win = match built.build() {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[desktop] could not open the window: {e}");
                return None;
            }
        };
        let me = self.clone();
        win.on_window_event(move |e| {
            if let WindowEvent::Destroyed = e {
                // Closing the window ends its WebView2 renderer even when the app stays in the tray: a hidden
                // dashboard would keep its memory for nothing. The tray (or a second launch) opens a new one.
                me.on_main(|s| {
                    if !s.quitting.load(Ordering::SeqCst) && !s.settings().close_to_tray {
                        s.quit();
                    }
                });
            }
        });
        Some(win)
    }

    fn show_main(self: &Arc<Self>) {
        let win = match self.app.get_webview_window(MAIN) {
            Some(w) => w,
            None => match self.create_main() {
                Some(w) => w,
                None => return,
            },
        };
        if win.is_minimized().unwrap_or(false) {
            let _ = win.unminimize();
        }
        let _ = win.show();
        let _ = win.set_focus();
    }

    /// During champ select and games the dashboard's renderer runs at below-normal priority; the overlay's
    /// stays normal (renderers.rs finds which is which). Back to normal for all when the game ends.
    fn dashboard_priority(&self) {
        let below = self.flags.lock().unwrap().playing;
        let report = |pids: &[u32], n: usize| {
            if n < pids.len() {
                eprintln!("[game] could not set the priority of {} of {} renderer processes", pids.len() - n, pids.len());
            }
        };
        #[cfg(windows)]
        if below {
            if let Some(main) = self.app.get_webview_window(MAIN) {
                renderers::dashboard_renderers(&main, self.origin(), move |pids| {
                    std::thread::spawn(move || {
                        let n = priority::set_priority(&pids, true);
                        report(&pids, n);
                    });
                });
            }
            return;
        }
        std::thread::spawn(move || {
            let pids = priority::renderer_pids();
            let n = priority::set_priority(&pids, below);
            report(&pids, n);
        });
    }

    fn splash_text(&self, text: &str) {
        if !self.origin().is_empty() {
            return;
        }
        if let Some(w) = self.app.get_webview_window(MAIN) {
            let js = format!("(() => {{ const p = document.querySelector('p'); if (p) p.textContent = {}; }})()", serde_json::to_string(text).unwrap());
            let _ = w.eval(js.as_str());
        }
    }

    // -------------------------------------------------------------- tray

    fn create_tray(self: &Arc<Self>) -> tauri::Result<()> {
        let app = &self.app;
        let state = self.overlay.lock().unwrap().state();
        let open = MenuItem::with_id(app, "open", "Open Build Fetcher", true, None::<&str>)?;
        let show_overlay = CheckMenuItem::with_id(app, "overlay", "Show overlay", true, state.visible, None::<&str>)?;
        let click_through = CheckMenuItem::with_id(app, "click-through", "Click-through", true, state.click_through, None::<&str>)?;
        let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
        let sep1 = PredefinedMenuItem::separator(app)?;
        let sep2 = PredefinedMenuItem::separator(app)?;
        let menu = Menu::with_items(app, &[&open, &sep1, &show_overlay, &click_through, &sep2, &quit])?;
        let me = self.clone();
        let me2 = self.clone();
        let mut builder = TrayIconBuilder::with_id("tray")
            .tooltip("Build Fetcher")
            .menu(&menu)
            .show_menu_on_left_click(false)
            .on_menu_event(move |_, e| {
                let id = e.id().as_ref().to_string();
                me.on_main(move |s| match id.as_str() {
                    "open" => s.show_main(),
                    "overlay" => {
                        let v = s.tray.lock().unwrap().as_ref().map(|t| t.show_overlay.is_checked().unwrap_or(false)).unwrap_or(false);
                        s.overlay_set_visible(v);
                    }
                    "click-through" => {
                        let v = s.tray.lock().unwrap().as_ref().map(|t| t.click_through.is_checked().unwrap_or(false)).unwrap_or(false);
                        s.overlay_set_click_through(v);
                    }
                    "quit" => s.quit(),
                    _ => {}
                });
            })
            .on_tray_icon_event(move |_, e| {
                if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                    me2.on_main(|s| s.show_main());
                }
            });
        if let Some(icon) = app.default_window_icon() {
            builder = builder.icon(icon.clone());
        }
        let icon = builder.build(app)?;
        *self.tray.lock().unwrap() = Some(TrayParts { icon, show_overlay, click_through });
        self.show_keep_awake(); // sets the tooltip
        Ok(())
    }

    fn update_tray_menu(&self, visible: bool, click_through: bool) {
        if let Some(t) = self.tray.lock().unwrap().as_ref() {
            let _ = t.show_overlay.set_checked(visible);
            let _ = t.click_through.set_checked(click_through);
        }
    }

    // -------------------------------------------------------------- keep awake while fetching

    fn keep_awake_view(&self) -> KeepAwakeView {
        let f = *self.flags.lock().unwrap();
        KeepAwakeView { state: self.keep_awake.state(), fetching: f.fetching }
    }

    fn show_keep_awake(&self) {
        let v = self.keep_awake_view();
        let tooltip = if v.state.active {
            format!("Build Fetcher · fetching, keeping the computer awake{}", if v.state.screen { " and the screen on" } else { "" })
        } else if v.state.system == "failed" {
            "Build Fetcher · fetching, but Windows may still sleep".to_string()
        } else {
            "Build Fetcher".to_string()
        };
        let icon = self.tray.lock().unwrap().as_ref().map(|t| t.icon.clone());
        if let Some(icon) = icon {
            let _ = icon.set_tooltip(Some(tooltip));
        }
        self.broadcast("bf:keep-awake-state", &v);
    }

    /// While fetching runs and the setting is on, Windows doesn't go to sleep.
    fn apply_keep_awake(&self) {
        let f = *self.flags.lock().unwrap();
        let s = self.settings();
        self.keep_awake.set(f.fetching && s.keep_awake_while_fetching, s.keep_screen_on_while_fetching);
        self.show_keep_awake();
    }

    // -------------------------------------------------------------- the server's feed

    fn on_feed(self: &Arc<Self>, f: Feed) {
        match f {
            Feed::Game(active) => self.on_main(move |s| s.overlay_set_in_game(active)),
            Feed::Fetching(r) => {
                let changed = std::mem::replace(&mut self.flags.lock().unwrap().fetching, r) != r;
                if changed {
                    self.apply_keep_awake();
                }
            }
            Feed::Playing(on) => {
                let changed = std::mem::replace(&mut self.flags.lock().unwrap().playing, on) != on;
                if changed {
                    self.dashboard_priority();
                }
            }
        }
    }

    // -------------------------------------------------------------- start and end

    fn fatal(self: &Arc<Self>, title: &str, message: String) {
        eprintln!("[desktop] {title}: {message}");
        self.quitting.store(true, Ordering::SeqCst);
        self.app
            .dialog()
            .message(format!("{message}\n\nLog: {}", self.info.log_file))
            .title(title)
            .kind(tauri_plugin_dialog::MessageDialogKind::Error)
            .blocking_show();
        self.app.exit(1);
    }

    fn quit(self: &Arc<Self>) {
        self.quitting.store(true, Ordering::SeqCst);
        self.app.exit(0);
    }

    /// `before-quit`: nothing keeps the computer awake, hotkeys go, the overlay closes.
    fn cleanup(self: &Arc<Self>) {
        self.quitting.store(true, Ordering::SeqCst);
        if let Some(w) = self.watcher.lock().unwrap().take() {
            w.abort();
        }
        self.flags.lock().unwrap().fetching = false;
        self.keep_awake.set(false, false);
        self.overlay_dispose();
    }
}

async fn boot(shell: Arc<Shell>) {
    // What the start is doing, on the splash.
    let splash = shell.clone();
    let opts = bf_server::serve::ServeOptions {
        port: Some(PREFERRED_PORT),
        any_port: true,
        web_dir: Some(bf_server::serve::default_web_dir()),
        starting: Box::new(move |text| {
            eprintln!("[start] {text}");
            splash.splash_text(text);
        }),
        handles: None,
    };
    let server = match bf_server::serve::run(opts).await {
        Ok(s) => s,
        Err(e) => {
            let s = shell.clone();
            let _ = tokio::task::spawn_blocking(move || s.fatal("Build Fetcher could not start", format!("{e:#}"))).await;
            return;
        }
    };
    let origin = format!("http://127.0.0.1:{}", server.port);
    eprintln!("[desktop] dashboard at {origin}/");
    *shell.origin.lock().unwrap() = origin.clone();
    shell.on_main(|s| {
        let origin = s.origin();
        match s.app.get_webview_window(MAIN) {
            Some(w) => {
                if let Ok(u) = format!("{origin}/").parse() {
                    let _ = w.navigate(u);
                }
            }
            None => {
                s.create_main();
            }
        }
        if let Err(e) = s.create_tray() {
            eprintln!("[desktop] no tray icon: {e}");
        }
    });
    let feed_shell = shell.clone();
    *shell.watcher.lock().unwrap() = Some(tauri::async_runtime::spawn(async move {
        let _ = feed::watch_server(&origin, move |f| feed_shell.on_feed(f)).await;
    }));
    let done = server.done.await;
    if !shell.quitting.load(Ordering::SeqCst) {
        let why = match done {
            Ok(Ok(())) => "it ended".to_string(),
            Ok(Err(e)) => e.to_string(),
            Err(e) => e.to_string(),
        };
        let s = shell.clone();
        let _ = tokio::task::spawn_blocking(move || s.fatal("Build Fetcher's server stopped", why)).await;
    }
}

/// Development check (BF_SELFTEST=1, debug builds): calls window.buildFetcher from the dashboard and prints what came
/// back, so the bridge can be seen working without clicking through the pages.
#[cfg(debug_assertions)]
fn selftest(w: WebviewWindow) {
    let js = r#"(async () => {
        const r = { bridge: typeof window.buildFetcher };
        try {
          r.info = await window.buildFetcher.info();
          r.settings = await window.buildFetcher.settings.get();
          r.overlay = await window.buildFetcher.overlay.state();
          r.keepAwake = await window.buildFetcher.keepAwake.state();
          r.set = (await window.buildFetcher.settings.set({ overlay: { opacity: 0.8 }, bogus: 1 })).overlay.opacity;
          r.events = [];
          window.buildFetcher.overlay.onState((s) => r.events.push(s));
        } catch (e) { r.error = String(e); }
        window.__selftest = r;
      })()"#;
    let _ = w.eval(js);
    std::thread::spawn(move || {
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let _ = w.eval_with_callback("JSON.stringify(window.__selftest ?? null)", |r| eprintln!("[selftest] {r}"));
        }
    });
}

fn trusted(shell: &Shell, webview: &Webview) -> Result<(), String> {
    let origin = shell.origin();
    match webview.url() {
        Ok(u) if !origin.is_empty() && u.origin().ascii_serialization() == origin => Ok(()),
        _ => Err("Untrusted sender".into()),
    }
}

// ------------------------------------------------------------------ commands (window.buildFetcher)

#[tauri::command]
fn desktop_info(webview: Webview, shell: State<Arc<Shell>>) -> Result<DesktopInfo, String> {
    trusted(&shell, &webview)?;
    Ok(shell.info.clone())
}

/// Folders only: opening a file would *run* it.
#[tauri::command]
async fn open_folder(webview: Webview, shell: State<'_, Arc<Shell>>, path: String) -> Result<(), String> {
    trusted(&shell, &webview)?;
    let p = Path::new(&path);
    if !p.is_absolute() || !p.is_dir() {
        return Ok(());
    }
    tauri_plugin_opener::open_path(p, None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
fn settings_get(webview: Webview, shell: State<Arc<Shell>>) -> Result<DesktopSettings, String> {
    trusted(&shell, &webview)?;
    Ok(shell.settings())
}

#[tauri::command]
fn settings_set(webview: Webview, shell: State<Arc<Shell>>, patch: serde_json::Value) -> Result<DesktopSettings, String> {
    trusted(&shell, &webview)?;
    let next = {
        let mut s = shell.settings.lock().unwrap();
        *s = settings::merge_desktop_settings(&s, &patch);
        s.clone()
    };
    settings::save_desktop_settings(&next);
    shell.overlay_apply_settings();
    shell.apply_keep_awake();
    Ok(next)
}

#[tauri::command]
fn keep_awake_state(webview: Webview, shell: State<Arc<Shell>>) -> Result<KeepAwakeView, String> {
    trusted(&shell, &webview)?;
    Ok(shell.keep_awake_view())
}

#[tauri::command]
fn overlay_state(webview: Webview, shell: State<Arc<Shell>>) -> Result<bf_desktop::overlay::OverlayState, String> {
    trusted(&shell, &webview)?;
    Ok(shell.overlay.lock().unwrap().state())
}

// These two are async on purpose. A sync Tauri command runs inside the IPC callback on the main thread, and creating
// a WebView window from there deadlocks on Windows (the hidden overlay window never shows). Async moves them off that
// callback; the tray/hotkey paths already run on the main thread.
#[tauri::command]
async fn overlay_set_visible(webview: Webview, shell: State<'_, Arc<Shell>>, visible: bool) -> Result<bf_desktop::overlay::OverlayState, String> {
    trusted(&shell, &webview)?;
    shell.overlay_set_visible(visible);
    Ok(shell.overlay.lock().unwrap().state())
}

#[tauri::command]
async fn overlay_set_click_through(webview: Webview, shell: State<'_, Arc<Shell>>, on: bool) -> Result<bf_desktop::overlay::OverlayState, String> {
    trusted(&shell, &webview)?;
    shell.overlay_set_click_through(on);
    Ok(shell.overlay.lock().unwrap().state())
}

/// Only the overlay's own page sizes the overlay.
#[tauri::command]
fn overlay_fit_height(webview: Webview, shell: State<Arc<Shell>>, px: Option<f64>) -> Result<(), String> {
    trusted(&shell, &webview)?;
    if webview.label() == overlay::LABEL {
        if let Some(px) = px {
            shell.overlay_fit_height(px);
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ main

/// The folder the dashboard (dist/) comes from: next to the executable when installed, the repository in a
/// development build (BF_APP_DIR overrides both).
fn app_dir(packaged: bool) -> PathBuf {
    if let Some(d) = std::env::var_os("BF_APP_DIR") {
        return PathBuf::from(d);
    }
    if packaged {
        if let Some(d) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
            return d;
        }
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn main() {
    let packaged = !cfg!(debug_assertions);
    // One per-user folder for everything: %APPDATA%\build-fetcher with settings.json, data/ and logs/. The uninstaller
    // leaves it (Tauri's "delete app data" option removes only the WebView2 folder named after the identifier).
    let config_dir = bf_core::app_config::app_config_dir();
    // Set before any thread starts: the server reads these.
    if packaged && std::env::var_os("BF_DEFAULT_DATA_DIR").is_none() {
        std::env::set_var("BF_DEFAULT_DATA_DIR", config_dir.join("data"));
    }
    let dir = app_dir(packaged);
    if let Err(e) = std::env::set_current_dir(&dir) {
        eprintln!("[desktop] cannot use {}: {e}", dir.display());
    }
    let log_file = log::log_file(&config_dir);
    if let Err(e) = log::open_log(&log_file, packaged) {
        eprintln!("[desktop] no log file {}: {e}", log_file.display());
    }

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("build-fetcher").build().expect("tokio runtime");
    tauri::async_runtime::set(rt.handle().clone());

    let info = DesktopInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        packaged,
        user_data: config_dir.to_string_lossy().into_owned(),
        log_file: log_file.to_string_lossy().into_owned(),
    };

    let bridge = BRIDGE.to_string();

    let app = tauri::Builder::default()
        // A second launch opens the first one's window.
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            if let Some(s) = app.try_state::<Arc<Shell>>() {
                s.on_main(|s| s.show_main());
            }
        }))
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            desktop_info,
            open_folder,
            settings_get,
            settings_set,
            keep_awake_state,
            overlay_state,
            overlay_set_visible,
            overlay_set_click_through,
            overlay_fit_height
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let settings = settings::load_desktop_settings();
            let click_through = settings.overlay.click_through;
            let shell = Arc::new_cyclic(|weak: &std::sync::Weak<Shell>| {
                let w = weak.clone();
                let keep_awake = KeepAwake::new(
                    Arc::new(SystemPower),
                    Arc::new(move |_| {
                        if let Some(s) = w.upgrade() {
                            s.show_keep_awake();
                        }
                    }),
                    Arc::new(|line| eprintln!("{line}")),
                );
                Shell {
                    app: handle.clone(),
                    settings: Mutex::new(settings),
                    origin: Mutex::new(String::new()),
                    quitting: AtomicBool::new(false),
                    overlay: Mutex::new(overlay::Overlay::new(click_through)),
                    keep_awake,
                    flags: Mutex::new(Flags::default()),
                    tray: Mutex::new(None),
                    info,
                    bridge: bridge.clone(),
                    watcher: Mutex::new(None),
                }
            });
            app.manage(shell.clone());
            shell.create_main();
            tauri::async_runtime::spawn(boot(shell));
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("Build Fetcher could not start");

    app.run(|app, event| match event {
        // The tray keeps the app alive; quitting goes through the tray menu (or the window, without close-to-tray).
        RunEvent::ExitRequested { code: None, api, .. } => {
            if let Some(s) = app.try_state::<Arc<Shell>>() {
                if !s.quitting.load(Ordering::SeqCst) && s.settings().close_to_tray {
                    api.prevent_exit();
                }
            }
        }
        RunEvent::Exit => {
            if let Some(s) = app.try_state::<Arc<Shell>>() {
                s.cleanup();
            }
        }
        _ => {}
    });
    drop(rt);
}
