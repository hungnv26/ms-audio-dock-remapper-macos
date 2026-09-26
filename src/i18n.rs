//! User-facing strings that live outside the Slint markup: alerts raised by
//! the platform backends and the status-item menu. English only.

/// Returns the string for `key`, or the key itself when it is unknown so a
/// typo never produces a blank message.
pub fn t(key: &str) -> &str {
    const TABLE: &[(&str, &str)] = &[
        ("tray_open", "Open Settings…"),
        ("tray_reconnect", "Reconnect Dock"),
        ("reconnect_fail", "Could not reconnect the Dock: "),
        ("menu_exit", "Quit MS Audio Dock Remapper for macOS"),
        (
            "single_instance",
            "MS Audio Dock Remapper for macOS is already running.",
        ),
        ("init_fail", "Failed to initialize the Dock listener: "),
        (
            "render_fail",
            "Failed to initialize the graphics backend, the settings window cannot be shown: ",
        ),
        ("save_fail", "Failed to save settings: "),
        ("action_fail", "The button action failed: "),
        ("app_list_fail", "Failed to list installed applications: "),
    ];
    TABLE
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| *v)
        .unwrap_or(key)
}
