fn main() {
    // The app's own commands get permissions (allow-<command>), granted to our pages in capabilities/bridge.json.
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(&[
        "desktop_info",
        "open_folder",
        "settings_get",
        "settings_set",
        "keep_awake_state",
        "overlay_state",
        "overlay_set_visible",
        "overlay_set_click_through",
        "overlay_fit_height",
        "backup_pick_save",
        "backup_pick_open",
        "restart_app",
    ])))
    .expect("tauri build");
}
