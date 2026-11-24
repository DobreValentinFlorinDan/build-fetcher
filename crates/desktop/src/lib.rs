//! The desktop shell's logic (ports of `desktop/*.ts` and `shared/desktop.ts`) without a window toolkit, so it builds
//! and tests anywhere. desktop/tauri puts it on screen.

pub mod feed;
pub mod keep_awake;
pub mod leftover;
pub mod log;
pub mod overlay;
pub mod priority;
pub mod settings;
pub mod web_storage;

/// `DesktopInfo`.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopInfo {
    pub version: String,
    pub packaged: bool,
    pub user_data: String,
    pub log_file: String,
}

/// The AppUserModelID / bundle identifier 0.x used (electron-builder.yml appId).
pub const APP_ID: &str = "com.ireliacompanion.app";
/// A stable port keeps the dashboard's origin (and its localStorage) stable.
pub const PREFERRED_PORT: u16 = 45174;
