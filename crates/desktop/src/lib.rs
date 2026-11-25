//! Build Fetcher's desktop shell logic (from irelia-companion's `desktop/*.ts`) without a window toolkit, so it
//! builds and tests anywhere. desktop/tauri puts it on screen.

pub mod feed;
pub mod keep_awake;
pub mod log;
pub mod overlay;
pub mod priority;
pub mod settings;

/// `DesktopInfo`, for the dashboard's About.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopInfo {
    pub version: String,
    pub packaged: bool,
    pub user_data: String,
    pub log_file: String,
}

/// The bundle identifier (tauri.conf.json), also Windows' AppUserModelID.
pub const APP_ID: &str = "com.buildfetcher.app";
/// A stable port keeps the dashboard's origin (and its localStorage) stable. Not Irelia Companion's (45174), so both
/// apps can run at once.
pub const PREFERRED_PORT: u16 = 45184;
