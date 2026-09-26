use std::sync::{Arc, Mutex};

use crate::config::{Button, Config};
use crate::platform::MonitorEvent;

/// Placeholder backend for platforms without an input implementation yet. The
/// architecture isolates all OS input code here; a real Linux (evdev/hidraw)
/// backend would replace this, while `main`/`ui` stay unchanged. See
/// `macos.rs` for a complete hidapi-based example.
pub fn start_monitor(
    _on_event: impl Fn(MonitorEvent) + Send + 'static,
    _config: Arc<Mutex<Config>>,
) {
    eprintln!(
        "[ms-audio-dock-remapper] Input monitoring is only implemented on Windows and macOS so far. \
         Build/run there to use the Dock Teams-key remapper."
    );
}

pub fn alert(message: &str) {
    eprintln!("[ms-audio-dock-remapper] {message}");
}

pub fn request_quit() {}

pub fn release_single_instance() {}

pub fn ensure_single_instance() -> bool {
    true
}

pub fn set_dpi_aware() {}

pub fn supported_buttons() -> &'static [Button] {
    &Button::ALL
}

pub fn bring_to_front() {}

pub fn supports_media_key_takeover() -> bool {
    false
}

pub fn accessibility_trusted(_prompt: bool) -> bool {
    true
}

pub fn autostart_enabled() -> bool {
    false
}

pub fn set_autostart(_enable: bool, _start_minimized: bool) {
    // TODO(linux): write ~/.config/autostart/ms-audio-dock-remapper.desktop
}
