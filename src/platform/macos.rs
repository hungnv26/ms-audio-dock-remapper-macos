//! macOS backend: read-only HID monitoring (IOKit through `hidapi`, opened in
//! shared mode so macOS keeps handling the Dock's own volume / media keys), a
//! menu bar status item, LaunchAgent login autostart and a single-instance
//! file lock.
//!
//! macOS exposes the whole Dock HID interface through one device handle rather
//! than one handle per top-level collection as Windows does, so every button
//! arrives on the same read and is told apart by report ID. Verified against a
//! real Dock (VID 045E / PID 084D) on macOS 26:
//!
//! | report ID | collection            | press report | button          |
//! |-----------|-----------------------|--------------|-----------------|
//! | 0x9B      | vendor FF99 / 0001    | `9B 01`      | Teams           |
//! | 0x01      | consumer control      | `01 01`/`01 02` | volume up / down |
//! | 0x04      | consumer control      | `04 08`      | play / pause    |
//! | 0x08      | telephony             | `08 01`/`08 00` | mic mute (latched) |
//!
//! The Teams report is byte-identical to what the Windows backend sees.

use std::cell::RefCell;
use std::ffi::c_int;
use std::fs::{self, File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use hidapi::{DeviceInfo, HidApi, HidDevice};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::string::CFString;

use crate::autostart::MINIMIZED_FLAG;
use crate::config::{ActionKind, Button, Config, DeviceFilter};
use crate::i18n;
use crate::platform::MonitorEvent;

/// Report ID of the Teams-key input report inside the Dock's vendor collection
/// (usage page FF99, usage 0001).
const TEAMS_REPORT_ID: u8 = 0x9B;
/// Consumer-control report carrying volume increment (bit 0) / decrement (bit 1).
const VOLUME_REPORT_ID: u8 = 0x01;
/// Consumer-control report carrying play/pause (bit 3).
const MEDIA_REPORT_ID: u8 = 0x04;
/// Telephony report carrying the latched phone-mute state (bit 0).
const TELEPHONY_REPORT_ID: u8 = 0x08;
/// How often the monitor re-scans for the Dock while it is unplugged.
const RESCAN_INTERVAL: Duration = Duration::from_secs(1);
/// Read timeout: bounds how long a quit request waits for the read loop.
const READ_TIMEOUT_MS: i32 = 250;
/// launchd label of the login item written by [`set_autostart`].
const LAUNCH_AGENT_LABEL: &str = "net.hungngo.ms-audio-dock-remapper";
/// Label used by pre-1.0 builds; removed whenever the login item is rewritten
/// so two entries never start the app twice at login.
const LEGACY_LAUNCH_AGENT_LABEL: &str = "com.masterain.ms-audio-dock-remapper";
/// How many read timeouts pass between checks of the desired open mode.
const MODE_CHECK_TICKS: u32 = 4;

// NX_KEYTYPE_* codes of the system-defined media key events.
const NX_KEYTYPE_SOUND_UP: isize = 0;
const NX_KEYTYPE_SOUND_DOWN: isize = 1;
const NX_KEYTYPE_PLAY: isize = 16;

extern "C" {
    // From the hidapi C library the `hidapi` crate links statically. Selects
    // whether the next open seizes the device (1) or shares it (0).
    fn hid_darwin_set_open_exclusive(open_exclusive: c_int);
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> u8;
}

/// How the Dock's HID interface is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenMode {
    /// macOS keeps receiving every report; media keys keep their system role.
    Shared,
    /// Only this app receives reports; keys without an action are re-posted
    /// as system media keys (needs Accessibility trust).
    Exclusive,
}

/// The UI-provided event sink, shared between the monitor thread and the
/// main-thread menu handler. The `Mutex` is what makes the `Send`-only
/// callback usable from both.
type SharedOnEvent = Arc<Mutex<Box<dyn Fn(MonitorEvent) + Send + 'static>>>;

static QUIT: AtomicBool = AtomicBool::new(false);
static INSTANCE_LOCK: Mutex<Option<File>> = Mutex::new(None);

thread_local! {
    // `TrayIcon` is main-thread only; dropping it removes the status item.
    static STATUS_ITEM: RefCell<Option<TrayIcon>> = const { RefCell::new(None) };
}

fn emit(on_event: &SharedOnEvent, event: MonitorEvent) {
    let callback = on_event.lock().unwrap();
    (*callback)(event);
}

/// Every Dock button is visible on the shared interface.
pub fn supported_buttons() -> &'static [Button] {
    &Button::ALL
}

pub fn supports_media_key_takeover() -> bool {
    true
}

/// Accessibility trust, which gates `CGEventPost` (verified: from an untrusted
/// process the post is silently dropped). With `prompt`, macOS shows its
/// "would like to control this computer" dialog and adds the app to the list.
pub fn accessibility_trusted(prompt: bool) -> bool {
    if !prompt {
        return unsafe { AXIsProcessTrusted() } != 0;
    }
    let options = CFDictionary::from_CFType_pairs(&[(
        CFString::new("AXTrustedCheckOptionPrompt").as_CFType(),
        CFBoolean::true_value().as_CFType(),
    )]);
    unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) != 0 }
}

/// Exclusive as soon as a media key has an action (so the system stops acting
/// on it too) *and* the re-post path works; otherwise a seized Dock would
/// leave the volume keys dead.
fn desired_mode(config: &Arc<Mutex<Config>>) -> OpenMode {
    let wanted = {
        let cfg = config.lock().unwrap();
        cfg.settings.enabled
            && Button::MEDIA
                .iter()
                .any(|b| cfg.buttons.get(*b).kind != ActionKind::None)
    };
    if wanted && accessibility_trusted(false) {
        OpenMode::Exclusive
    } else {
        OpenMode::Shared
    }
}

/// Synthesizes one press+release of a system media key, the same way the HID
/// event driver would have reported the Dock's own key.
fn post_media_key(key: isize) {
    use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
    use objc2_core_graphics::{CGEvent, CGEventTapLocation};
    use objc2_foundation::NSPoint;

    for down in [true, false] {
        let flags = NSEventModifierFlags(if down { 0xa00 } else { 0xb00 });
        let data1 = (key << 16) | (if down { 0xa } else { 0xb } << 8);
        let event = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::SystemDefined,
            NSPoint::new(0.0, 0.0),
            flags,
            0.0,
            0,
            None,
            8,
            data1,
            -1,
        );
        if let Some(cg_event) = event.and_then(|e| e.CGEvent()) {
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&cg_event));
        }
    }
}

/// What the system would have done with the key: re-posted in exclusive mode
/// when the button has no action. Teams is ignored by macOS anyway, and mic
/// mute is handled inside the Dock's firmware.
fn forward_system_key(button: Button) {
    match button {
        Button::VolumeUp => post_media_key(NX_KEYTYPE_SOUND_UP),
        Button::VolumeDown => post_media_key(NX_KEYTYPE_SOUND_DOWN),
        Button::PlayPause => post_media_key(NX_KEYTYPE_PLAY),
        Button::Teams | Button::MicMute => {}
    }
}

/// Starts the resident monitor: the menu bar item (main thread), the action
/// worker and the HID read loop. Called from `ui::run` on the main thread
/// before the Slint event loop starts; NSApplication's run loop, driven by
/// Slint's winit backend, then dispatches the status item's menu actions.
pub fn start_monitor(on_event: impl Fn(MonitorEvent) + Send + 'static, config: Arc<Mutex<Config>>) {
    QUIT.store(false, Ordering::SeqCst);
    let on_event: SharedOnEvent = Arc::new(Mutex::new(Box::new(on_event)));

    install_status_item(on_event.clone());

    // Actions run off the monitor thread so a slow launch never delays the
    // next report, mirroring the Windows backend.
    let (action_tx, action_rx) = mpsc::channel::<(Button, Config)>();
    thread::spawn(move || {
        while let Ok((button, cfg)) = action_rx.recv() {
            if let Err(e) = crate::actions::run(cfg.buttons.get(button), &cfg.settings) {
                alert(&format!("{} {e}", i18n::t("action_fail")));
            }
        }
    });

    let monitor_events = on_event.clone();
    let spawned = thread::Builder::new()
        .name("dock-monitor".into())
        .spawn(move || monitor_loop(monitor_events, config, action_tx));
    if spawned.is_err() {
        emit(&on_event, MonitorEvent::Status(0));
    }
}

/// Re-scans for the Dock, reads it until it goes away, repeats. Hot-plug is
/// handled by the outer loop: a read error means the device was removed.
fn monitor_loop(
    on_event: SharedOnEvent,
    config: Arc<Mutex<Config>>,
    action_tx: Sender<(Button, Config)>,
) {
    let mut api = match HidApi::new() {
        Ok(api) => api,
        Err(e) => {
            alert(&format!("{} {e}", i18n::t("init_fail")));
            emit(&on_event, MonitorEvent::Status(0));
            return;
        }
    };

    let mut last_status: Option<u32> = None;
    while !QUIT.load(Ordering::SeqCst) {
        let filter = config.lock().unwrap().device.clone();
        let path = match api.refresh_devices() {
            Ok(()) => {
                let matching: Vec<&DeviceInfo> = api
                    .device_list()
                    .filter(|d| matches_filter(d, &filter))
                    .collect();
                let count = matching.len() as u32;
                if last_status != Some(count) {
                    eprintln!("[ms-audio-dock-remapper] matching Dock collections: {count}");
                    emit(&on_event, MonitorEvent::Status(count));
                    last_status = Some(count);
                }
                matching.first().map(|d| d.path().to_owned())
            }
            Err(_) => None,
        };

        let Some(path) = path else {
            sleep_unless_quit(RESCAN_INTERVAL);
            continue;
        };
        let mode = desired_mode(&config);
        unsafe { hid_darwin_set_open_exclusive((mode == OpenMode::Exclusive) as c_int) };
        let Ok(device) = api.open_path(&path) else {
            sleep_unless_quit(RESCAN_INTERVAL);
            continue;
        };
        eprintln!("[ms-audio-dock-remapper] Dock opened in {mode:?} mode");
        emit(
            &on_event,
            MonitorEvent::Exclusive(mode == OpenMode::Exclusive),
        );
        read_reports(&device, mode, &on_event, &config, &action_tx);
    }
}

/// True when this enumerated collection is the one the config points at.
fn matches_filter(device: &DeviceInfo, filter: &DeviceFilter) -> bool {
    let hex = |s: &str| u16::from_str_radix(s.trim(), 16).ok();
    match (
        hex(&filter.vendor_id),
        hex(&filter.product_id),
        hex(&filter.usage_page),
        hex(&filter.usage),
    ) {
        (Some(vid), Some(pid), Some(usage_page), Some(usage)) => {
            device.vendor_id() == vid
                && device.product_id() == pid
                && device.usage_page() == usage_page
                && device.usage() == usage
        }
        _ => false,
    }
}

/// Blocks on the device until it disappears, the desired open mode changes
/// (the caller then reopens) or a quit is requested.
fn read_reports(
    device: &HidDevice,
    mode: OpenMode,
    on_event: &SharedOnEvent,
    config: &Arc<Mutex<Config>>,
    action_tx: &Sender<(Button, Config)>,
) {
    let mut buf = [0u8; 64];
    let mut ticks = 0u32;
    while !QUIT.load(Ordering::SeqCst) {
        match device.read_timeout(&mut buf, READ_TIMEOUT_MS) {
            Ok(0) => {
                ticks += 1;
                if ticks.is_multiple_of(MODE_CHECK_TICKS) && desired_mode(config) != mode {
                    return;
                }
            }
            Ok(n) => {
                if let Some(button) = match_press(&buf[..n]) {
                    emit(on_event, MonitorEvent::Press(button));
                    // Clone, then release the lock before handing the action
                    // to the worker; never hold the config mutex across a launch.
                    let cfg = config.lock().unwrap().clone();
                    let has_action = cfg.buttons.get(button).kind != ActionKind::None;
                    if cfg.settings.enabled && has_action {
                        let _ = action_tx.send((button, cfg));
                    } else if mode == OpenMode::Exclusive {
                        forward_system_key(button);
                    }
                }
            }
            Err(_) => return,
        }
    }
}

/// Maps an input report to the button whose *press* it announces. Releases
/// (`xx 00`) and unrelated reports yield `None`. The telephony mute report is
/// a latched state, so both transitions count as a press: each one is a tap.
fn match_press(report: &[u8]) -> Option<Button> {
    if report.len() < 2 {
        return None;
    }
    match (report[0], report[1]) {
        (TEAMS_REPORT_ID, 0x01) => Some(Button::Teams),
        (VOLUME_REPORT_ID, bits) if bits & 0x01 != 0 => Some(Button::VolumeUp),
        (VOLUME_REPORT_ID, bits) if bits & 0x02 != 0 => Some(Button::VolumeDown),
        (MEDIA_REPORT_ID, bits) if bits & 0x08 != 0 => Some(Button::PlayPause),
        (TELEPHONY_REPORT_ID, _) => Some(Button::MicMute),
        _ => None,
    }
}

fn sleep_unless_quit(total: Duration) {
    let step = Duration::from_millis(100);
    let mut slept = Duration::ZERO;
    while slept < total && !QUIT.load(Ordering::SeqCst) {
        thread::sleep(step);
        slept += step;
    }
}

// --- menu bar status item ----------------------------------------------------

fn install_status_item(on_event: SharedOnEvent) {
    let menu = Menu::new();
    let open_item = MenuItem::new(i18n::t("tray_open"), true, None);
    let quit_item = MenuItem::new(i18n::t("menu_exit"), true, None);
    if let Err(e) = menu.append_items(&[&open_item, &PredefinedMenuItem::separator(), &quit_item]) {
        eprintln!("[ms-audio-dock-remapper] status menu unavailable: {e}");
        return;
    }

    let open_id = open_item.id().clone();
    let quit_id = quit_item.id().clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let mapped = if *event.id() == open_id {
            MonitorEvent::TrayShow
        } else if *event.id() == quit_id {
            MonitorEvent::Quit
        } else {
            return;
        };
        emit(&on_event, mapped);
    }));

    let mut builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Microsoft Audio Dock Remapper");
    if let Some(icon) = status_icon() {
        builder = builder.with_icon(icon);
    }
    match builder.build() {
        Ok(item) => {
            eprintln!("[ms-audio-dock-remapper] menu bar item installed");
            STATUS_ITEM.with(|slot| *slot.borrow_mut() = Some(item));
        }
        Err(e) => eprintln!("[ms-audio-dock-remapper] status item unavailable: {e}"),
    }
}

/// Decodes the embedded 64px header PNG into a status-bar icon. `None` (and
/// therefore the default icon) if the asset ever stops being 8-bit RGBA.
fn status_icon() -> Option<Icon> {
    let decoder = png::Decoder::new(std::io::Cursor::new(crate::platform::APP_ICON_HEADER));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    buf.truncate(info.buffer_size());
    Icon::from_rgba(buf, info.width, info.height).ok()
}

// --- process-level helpers ---------------------------------------------------

/// Modal alert through `osascript` (no AppKit dependency in this layer), plus
/// stderr so a headless launch still leaves a trace.
pub fn alert(message: &str) {
    eprintln!("[ms-audio-dock-remapper] {message}");
    let script = format!(
        "display alert \"MS Audio Dock Remapper\" message \"{}\" as warning",
        message.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let _ = Command::new("osascript").args(["-e", &script]).status();
}

/// Stops the read loop and removes the status item (main thread only for the
/// latter; from another thread the thread-local slot is simply empty).
pub fn request_quit() {
    QUIT.store(true, Ordering::SeqCst);
    STATUS_ITEM.with(|slot| {
        slot.borrow_mut().take();
    });
}

/// No-op: AppKit windows are DPI-aware by construction.
pub fn set_dpi_aware() {}

/// Takes an exclusive `flock` on a file in the user's cache directory. The lock
/// dies with the process, so a crash never leaves a stale "already running".
pub fn ensure_single_instance() -> bool {
    let dir = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("ms-audio-dock-remapper");
    let _ = fs::create_dir_all(&dir);
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("instance.lock"))
    else {
        // Cannot lock: better to run than to refuse over a filesystem problem.
        return true;
    };
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if locked {
        *INSTANCE_LOCK.lock().unwrap() = Some(file);
    }
    locked
}

pub fn release_single_instance() {
    INSTANCE_LOCK.lock().unwrap().take();
}

// --- login autostart (LaunchAgent) -------------------------------------------

fn launch_agent_path_for(label: &str) -> Option<PathBuf> {
    dirs::home_dir().map(|home| {
        home.join("Library")
            .join("LaunchAgents")
            .join(format!("{label}.plist"))
    })
}

fn launch_agent_path() -> Option<PathBuf> {
    launch_agent_path_for(LAUNCH_AGENT_LABEL)
}

pub fn autostart_enabled() -> bool {
    launch_agent_path().is_some_and(|p| p.is_file())
        || launch_agent_path_for(LEGACY_LAUNCH_AGENT_LABEL).is_some_and(|p| p.is_file())
}

/// Writes (or removes) a per-user LaunchAgent that runs this executable at
/// login. The agent is deliberately not bootstrapped/booted-out live: with
/// `RunAtLoad` that would start a second instance now, and `bootout` would
/// kill this very process when it was itself started by launchd.
pub fn set_autostart(enable: bool, start_minimized: bool) {
    let Some(path) = launch_agent_path() else {
        return;
    };
    if let Some(legacy) = launch_agent_path_for(LEGACY_LAUNCH_AGENT_LABEL) {
        if legacy.exists() {
            let _ = fs::remove_file(legacy);
        }
    }
    if !enable {
        if path.exists() {
            let _ = fs::remove_file(&path);
        }
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut args = vec![exe.to_string_lossy().into_owned()];
    if start_minimized {
        args.push(MINIMIZED_FLAG.to_string());
    }
    let program_args: String = args
        .iter()
        .map(|a| format!("\t\t<string>{}</string>\n", xml_escape(a)))
        .collect();
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LAUNCH_AGENT_LABEL}</string>
	<key>ProgramArguments</key>
	<array>
{program_args}	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>ProcessType</key>
	<string>Interactive</string>
</dict>
</plist>
"#
    );
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Err(e) = fs::write(&path, plist) {
        eprintln!("[ms-audio-dock-remapper] failed to write LaunchAgent: {e}");
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::{match_press, status_icon};
    use crate::config::Button;

    #[test]
    fn embedded_header_png_decodes_into_a_status_icon() {
        assert!(
            status_icon().is_some(),
            "public/app-icon-header.png must stay 8-bit RGBA"
        );
    }

    #[test]
    fn maps_every_captured_press_report_to_its_button() {
        assert_eq!(match_press(&[0x9B, 0x01]), Some(Button::Teams));
        assert_eq!(match_press(&[0x04, 0x08]), Some(Button::PlayPause));
        assert_eq!(match_press(&[0x01, 0x02]), Some(Button::VolumeDown));
        assert_eq!(match_press(&[0x01, 0x01]), Some(Button::VolumeUp));
        assert_eq!(match_press(&[0x08, 0x01]), Some(Button::MicMute));
        // Latched state: the unmute tap reports 00 and is a press too.
        assert_eq!(match_press(&[0x08, 0x00]), Some(Button::MicMute));
    }

    #[test]
    fn ignores_releases_and_unknown_reports() {
        assert_eq!(match_press(&[0x9B, 0x00]), None);
        assert_eq!(match_press(&[0x01, 0x00]), None);
        assert_eq!(match_press(&[0x04, 0x00]), None);
        assert_eq!(match_press(&[0x39, 0x20, 0x01]), None);
        assert_eq!(match_press(&[0x9B]), None);
    }
}
