//! Port of desktop/settings.test.ts: defaults, settings saved by an older version, and page patches, which are
//! untrusted and validated field by field.

use irelia::app_config::update_app_config;
use irelia_desktop::settings::{defaults, load_desktop_settings, merge_desktop_settings};
use serde_json::json;

#[test]
fn desktop_settings() {
    let root = tempfile::tempdir().unwrap();
    std::env::set_var("IRELIA_CONFIG_DIR", root.path());

    // keeping the computer awake while fetching is on by default, also for settings saved before it existed
    let d = defaults();
    assert!(d.keep_awake_while_fetching);
    let mut older = d.to_value();
    older.as_object_mut().unwrap().shift_remove("keepAwakeWhileFetching");
    older["closeToTray"] = json!(false);
    update_app_config(vec![("desktop", Some(older))]);
    let loaded = load_desktop_settings();
    assert!(loaded.keep_awake_while_fetching);
    assert!(!loaded.close_to_tray, "the rest of what was saved is kept");

    // the switch turns it off and on; anything but a boolean leaves it as it is
    let off = merge_desktop_settings(&d, &json!({ "keepAwakeWhileFetching": false }));
    assert!(!off.keep_awake_while_fetching);
    assert!(!merge_desktop_settings(&off, &json!({ "keepAwakeWhileFetching": "yes" })).keep_awake_while_fetching);
    assert!(!merge_desktop_settings(&off, &json!({ "closeToTray": true })).keep_awake_while_fetching, "another setting does not reset it");
    assert!(merge_desktop_settings(&off, &json!({ "keepAwakeWhileFetching": true })).keep_awake_while_fetching);

    // keeping the screen on too is off by default and validated like the rest
    assert!(!d.keep_screen_on_while_fetching);
    let mut older = d.to_value();
    older.as_object_mut().unwrap().shift_remove("keepScreenOnWhileFetching");
    update_app_config(vec![("desktop", Some(older))]);
    assert!(!load_desktop_settings().keep_screen_on_while_fetching, "settings saved before it existed");
    let on = merge_desktop_settings(&d, &json!({ "keepScreenOnWhileFetching": true }));
    assert!(on.keep_screen_on_while_fetching);
    assert!(merge_desktop_settings(&on, &json!({ "keepScreenOnWhileFetching": 1 })).keep_screen_on_while_fetching);
    assert!(merge_desktop_settings(&on, &json!({ "keepAwakeWhileFetching": false })).keep_screen_on_while_fetching, "another setting does not reset it");

    // other keys of config.json are kept when the desktop settings are saved (0.x and 1.0 share the file)
    update_app_config(vec![("riotApiKey", Some(json!("RGAPI-x")))]);
    irelia_desktop::settings::save_desktop_settings(&on);
    let cfg = irelia::app_config::read_app_config();
    assert_eq!(cfg.get("riotApiKey"), Some(&json!("RGAPI-x")));
    assert_eq!(cfg["desktop"]["keepScreenOnWhileFetching"], json!(true));
}
