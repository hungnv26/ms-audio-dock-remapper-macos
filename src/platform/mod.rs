use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::config::{Button, Config};

// App icon, embedded at compile time and materialized to a temp file at runtime
// so both the Slint window (ICO) and the in-app header (PNG) can load it.
#[cfg(windows)]
const APP_ICON_ICO: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/public/app-icon.ico"));
// Small, pre-anti-aliased (supersampled) icon for the in-app header / menu.
// Using this instead of the 512px PNG avoids runtime shrink artifacts (jaggies).
// Also the source of the macOS status-bar icon.
pub(crate) const APP_ICON_HEADER: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/public/app-icon-header.png"
));

/// Writes `bytes` to a stable temp file (once) and returns its path. Returns
/// `None` only if writing fails, in which case callers fall back to defaults.
#[cfg(any(windows, target_os = "macos"))]
fn ensure_icon_file(bytes: &[u8], name: &str) -> Option<PathBuf> {
    let path = std::env::temp_dir().join(name);
    let needs_write = match std::fs::read(&path) {
        Ok(existing) => existing.len() != bytes.len(),
        Err(_) => true,
    };
    if needs_write && std::fs::write(&path, bytes).is_err() {
        return None;
    }
    Some(path)
}

/// Path to the small, pre-anti-aliased PNG used for the in-app header icon.
/// Keeping it small (64px, supersampled) avoids jagged runtime downscaling from
/// the full 512px asset. `None` where no custom icon is shipped.
#[cfg(any(windows, target_os = "macos"))]
pub fn header_icon_path() -> Option<PathBuf> {
    ensure_icon_file(APP_ICON_HEADER, "ms-audio-dock-remapper-icon-header.png")
}
#[cfg(not(any(windows, target_os = "macos")))]
pub fn header_icon_path() -> Option<PathBuf> {
    None
}

/// Path to the multi-size ICO for the Windows tray icon.
/// `None` on platforms where no custom icon is shipped.
#[cfg(windows)]
pub fn tray_icon_path() -> Option<PathBuf> {
    ensure_icon_file(APP_ICON_ICO, "ms-audio-dock-remapper-icon.ico")
}
#[cfg(not(windows))]
#[allow(dead_code)]
pub fn tray_icon_path() -> Option<PathBuf> {
    None
}

// One backend per OS, all exposing the same function surface; the wrappers
// below are the only thing `main` / `ui` / `autostart` talk to.
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(not(any(windows, target_os = "macos")))]
pub mod stub;
#[cfg(windows)]
pub mod windows;

#[cfg(target_os = "macos")]
use self::macos as backend;
#[cfg(not(any(windows, target_os = "macos")))]
use self::stub as backend;
#[cfg(windows)]
use self::windows as backend;

/// Events delivered from the OS-specific resident monitor thread to the UI
/// thread. Keeping this enum platform-agnostic lets `main`/`ui` stay identical
/// across Windows / Linux / macOS.
#[derive(Debug, Clone)]
pub enum MonitorEvent {
    /// A Dock button was pressed (the action itself runs on the backend's
    /// worker; this only updates the UI).
    Press(Button),
    /// Candidate Dock collections currently registered.
    Status(u32),
    /// Tray icon requested the settings window.
    TrayShow,
    /// Tray / status-item menu requested a full exit (macOS menu bar item).
    #[cfg_attr(windows, allow(dead_code))]
    Quit,
    /// The Dock is now open exclusively (true) or shared (false); macOS only.
    #[cfg_attr(windows, allow(dead_code))]
    Exclusive(bool),
    /// The status-item menu asked for a software replug of the Dock.
    #[cfg_attr(windows, allow(dead_code))]
    Reconnect,
}

/// Starts the OS-specific resident monitor (input listening + tray).
///
/// Instead of a channel sender, callers pass `on_event`: a `Send` callback the
/// monitor invokes on every `MonitorEvent`. The UI layer typically forwards each
/// event into the Slint event loop with `slint::invoke_from_event_loop` so the
/// app stays event-driven and the UI thread can idle (no periodic polling).
/// Implementations live in `windows.rs` / `macos.rs` / `stub.rs`, selected by cfg.
pub fn start_monitor(on_event: impl Fn(MonitorEvent) + Send + 'static, config: Arc<Mutex<Config>>) {
    backend::start_monitor(on_event, config);
}

/// Shows a modal alert (Windows MessageBox, macOS osascript alert; otherwise
/// stderr). Used for fatal startup errors and action failures so they are
/// never silent.
pub fn alert(message: &str) {
    backend::alert(message);
}

/// Asks the resident monitor thread to exit. On Windows this posts `WM_QUIT` to
/// the monitor thread; on macOS it stops the read loop and removes the status
/// item; elsewhere it is a no-op.
pub fn request_quit() {
    backend::request_quit();
}

/// Gives up the single-instance lock so a process started right afterwards can
/// claim it. Only used when the app deliberately re-executes itself (the
/// software-renderer fallback in `ui`); a normal exit releases it anyway.
pub fn release_single_instance() {
    backend::release_single_instance();
}

/// Claims the single-instance slot (named mutex on Windows, `flock` on macOS).
/// Returns false when another instance already holds it.
pub fn ensure_single_instance() -> bool {
    backend::ensure_single_instance()
}

/// Declares the process DPI-aware so windows render crisply on high-DPI
/// displays (notably the "already running" MessageBox shown before the UI loop).
pub fn set_dpi_aware() {
    backend::set_dpi_aware();
}

/// The Dock buttons this backend can observe. Windows registers only the
/// Teams collection with Raw Input; macOS reads the whole interface.
pub fn supported_buttons() -> &'static [Button] {
    backend::supported_buttons()
}

/// Whether this backend can take the Dock's media keys away from the system
/// (macOS exclusive open + re-posting). False elsewhere.
pub fn supports_media_key_takeover() -> bool {
    backend::supports_media_key_takeover()
}

/// Whether the process may synthesize system input (macOS Accessibility
/// trust). With `prompt`, the OS asks the user to grant it. Always true where
/// no such gate exists.
pub fn accessibility_trusted(prompt: bool) -> bool {
    backend::accessibility_trusted(prompt)
}

/// Whether [`reconnect_device`] can do anything on this platform.
pub fn supports_reconnect() -> bool {
    backend::supports_reconnect()
}

/// Software replug of the Dock's audio/button device (re-enumeration). Errors
/// carry a user-facing sentence, including the case where the Dock has left
/// the USB bus and only a power cycle can bring it back.
pub fn reconnect_device(config: &Config) -> Result<(), String> {
    backend::reconnect_device(config)
}

/// Makes this app the active one so a window shown from the tray / menu bar
/// item comes to the front (menu bar apps are not activated by a menu click).
pub fn bring_to_front() {
    backend::bring_to_front();
}

/// Whether a login entry for this app currently exists.
pub fn autostart_enabled() -> bool {
    backend::autostart_enabled()
}

/// Registers / unregisters the login entry (HKCU Run value on Windows,
/// LaunchAgent on macOS).
pub fn set_autostart(enable: bool, start_minimized: bool) {
    backend::set_autostart(enable, start_minimized);
}
