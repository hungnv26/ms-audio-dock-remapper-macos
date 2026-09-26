//! Windows backend: read-only Raw Input monitoring (by Usage Page/Usage, VID/PID
//! filtered) + tray icon + login autostart + single-instance mutex.
//!
//! All OS-specific code lives here; `main`/`ui` are platform-agnostic and talk
//! to this module only through `start_monitor` / `MonitorEvent` (see `mod.rs`).
//! A Linux (evdev/hidraw) or macOS (IOKit) backend would implement the same
//! surface without touching the rest of the app.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

/// Callback the resident monitor invokes for each `MonitorEvent`. Defined by the
/// UI layer (it forwards into the Slint event loop). `Send + 'static` so the
/// monitor can own and call it from its own OS thread.
type OnEvent = Box<dyn Fn(MonitorEvent) + Send + 'static>;

use windows_sys::Win32::Devices::HumanInterfaceDevice::{HidP_GetCaps, HIDP_CAPS};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ,
};
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentThreadId, ReleaseMutex};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, GetRawInputDeviceInfoW, GetRawInputDeviceList, RegisterRawInputDevices,
    HRAWINPUT, RAWINPUTDEVICE, RAWINPUTDEVICELIST, RAWINPUTHEADER, RIDEV_DEVNOTIFY,
    RIDEV_INPUTSINK, RIDI_DEVICENAME, RIDI_PREPARSEDDATA, RID_INPUT, RIM_TYPEHID,
};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, LoadIconW, LoadImageW, MessageBoxW, PostThreadMessageW, RegisterClassW,
    SetWindowLongPtrW, TranslateMessage, HICON, HWND_MESSAGE, IDI_APPLICATION, IMAGE_ICON,
    LR_DEFAULTSIZE, LR_LOADFROMFILE, MSG, WM_LBUTTONDBLCLK, WNDCLASSW,
};

use crate::config::Config;
use crate::i18n;
use crate::platform::MonitorEvent;

// --- constants -------------------------------------------------------------
const WM_INPUT: u32 = 0x00FF;
const WM_INPUT_DEVICE_CHANGE: u32 = 0x00FE;
const APP_WM_TRAY: u32 = 0x0400;
const WM_QUIT: u32 = 0x0012;
const HIDP_STATUS_SUCCESS: i32 = 0x0011_0000;
const ERROR_ALREADY_EXISTS: u32 = 183;
const TRAY_ID: u32 = 1;
const GWLP_USERDATA: i32 = -21;

/// Thread id of the resident monitor loop, so the UI thread can ask it to exit
/// (via `request_quit`) when the user chooses "Exit" from the main menu bar.
static MONITOR_TID: Mutex<Option<u32>> = Mutex::new(None);

/// Join handle for the resident monitor thread. `request_quit` waits on it so
/// the process does not exit before the monitor has removed its tray icon and
/// destroyed its window (otherwise a "ghost" tray icon lingers until hover).
static MONITOR_JOIN: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

/// The single-instance mutex, held for the whole process lifetime. Kept around
/// (rather than leaked) only so `release_single_instance` can hand the slot over
/// to a replacement process; see the software-renderer fallback in `ui`.
static INSTANCE_MUTEX: Mutex<Option<HANDLE>> = Mutex::new(None);

// --- types -----------------------------------------------------------------
#[derive(Clone)]
struct DeviceInfo {
    hdevice: HANDLE,
    #[allow(dead_code)]
    vid: u16,
    #[allow(dead_code)]
    pid: u16,
    usage_page: u16,
    usage: u16,
}

struct Ctx {
    on_event: OnEvent,
    config: Arc<Mutex<Config>>,
    /// Shell activation runs on this worker, never inside the Raw Input wndproc.
    action_tx: Sender<Config>,
    devices: RefCell<HashMap<HANDLE, DeviceInfo>>,
}

// --- public API ------------------------------------------------------------
pub fn start_monitor(on_event: impl Fn(MonitorEvent) + Send + 'static, config: Arc<Mutex<Config>>) {
    let handle = std::thread::spawn(move || unsafe {
        run(Box::new(on_event), config);
    });
    *MONITOR_JOIN.lock().unwrap() = Some(handle);
}

pub fn ensure_single_instance() -> bool {
    let name = wide("Global\\MsAudioDockRemapper.SingleInstance");
    unsafe {
        let h = CreateMutexW(ptr::null(), 1, name.as_ptr());
        if h == 0 {
            return false;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = ReleaseMutex(h);
            let _ = CloseHandle(h);
            return false;
        }
        *INSTANCE_MUTEX.lock().unwrap() = Some(h);
    }
    true
}

/// Gives up the single-instance slot so a process started right afterwards can
/// claim it. Used only when the app deliberately re-executes itself (the
/// software-renderer fallback); a normal exit closes the handle anyway.
///
/// Must run on the thread that created the mutex — it owns it — which is the
/// main thread in both cases.
pub fn release_single_instance() {
    let handle = INSTANCE_MUTEX.lock().unwrap().take();
    if let Some(h) = handle {
        unsafe {
            let _ = ReleaseMutex(h);
            let _ = CloseHandle(h);
        }
    }
}

pub fn alert(message: &str) {
    let msg = wide(message);
    let title = wide("MsAudioDock Remapper");
    unsafe {
        MessageBoxW(0, msg.as_ptr(), title.as_ptr(), 0); // MB_OK == 0
    }
}

/// Asks the resident monitor thread to exit when the user selects Exit from the
/// main menu bar, then waits for it to finish cleaning up (remove tray icon +
/// destroy its window). Blocking here prevents the process from exiting while
/// the tray icon is still registered, which would leave a "ghost" icon in the
/// notification area until the user moves the mouse over it.
pub fn request_quit() {
    if let Some(tid) = *MONITOR_TID.lock().unwrap() {
        unsafe {
            PostThreadMessageW(tid, WM_QUIT, 0, 0);
        }
    }
    // Wait for the monitor thread's cleanup (delete_tray + DestroyWindow) to run
    // before returning. `take()` releases the lock before join to avoid holding
    // it across a blocking wait.
    let handle = MONITOR_JOIN.lock().unwrap().take();
    if let Some(h) = handle {
        let _ = h.join();
    }
}

/// Declares the process as per-monitor DPI aware (V2) so every window —
/// including the "already running" `MessageBox` shown before the Slint event
/// loop starts — renders crisply on high-DPI displays instead of being scaled
/// (blurred) by DWM. Must be called before any window/MessageBox is shown.
pub fn set_dpi_aware() {
    use windows_sys::Win32::UI::HiDpi::{
        SetProcessDpiAwareness, SetProcessDpiAwarenessContext,
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, PROCESS_PER_MONITOR_DPI_AWARE,
    };
    unsafe {
        // Prefer per-monitor V2 (Win10 1703+); fall back to per-monitor V1
        // (Win8.1+). Ignore failure (already set / very old OS).
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            let _ = SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE);
        }
    }
}

/// Writes (or removes) the HKCU Run entry. When `start_minimized` is set the
/// registered command line carries `--minimized`, so the sign-in launch comes up
/// tray-only even if the config file cannot be read at that point.
pub fn set_autostart(enable: bool, start_minimized: bool) {
    let sub = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let value = wide("MsAudioDockRemapper");
    unsafe {
        let mut hkey: HKEY = std::mem::zeroed();
        if RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_WRITE, &mut hkey) != 0 {
            return;
        }
        if enable {
            let exe = std::env::current_exe()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let data = if start_minimized {
                format!("\"{exe}\" {}", crate::autostart::MINIMIZED_FLAG)
            } else {
                format!("\"{exe}\"")
            };
            let wdata = wide(&data);
            RegSetValueExW(
                hkey,
                value.as_ptr(),
                0,
                REG_SZ,
                wdata.as_ptr() as *const u8,
                (wdata.len() * 2) as u32,
            );
        } else {
            RegDeleteValueW(hkey, value.as_ptr());
        }
        RegCloseKey(hkey);
    }
}

/// Raw Input is registered for the Teams collection only.
pub fn supported_buttons() -> &'static [crate::config::Button] {
    &[crate::config::Button::Teams]
}

pub fn bring_to_front() {}

pub fn supports_media_key_takeover() -> bool {
    false
}

pub fn accessibility_trusted(_prompt: bool) -> bool {
    true
}

pub fn autostart_enabled() -> bool {
    let sub = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let value = wide("MsAudioDockRemapper");
    unsafe {
        let mut hkey: HKEY = std::mem::zeroed();
        if RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_READ, &mut hkey) != 0 {
            return false;
        }
        let mut buf: [u16; 256] = [0; 256];
        let mut pcb: u32 = (buf.len() * 2) as u32;
        let r = RegQueryValueExW(
            hkey,
            value.as_ptr(),
            ptr::null_mut(),
            ptr::null_mut(),
            buf.as_mut_ptr() as *mut u8,
            &mut pcb,
        );
        RegCloseKey(hkey);
        r == 0
    }
}

// --- monitor loop ----------------------------------------------------------
unsafe fn run(on_event: OnEvent, config: Arc<Mutex<Config>>) {
    *MONITOR_TID.lock().unwrap() = Some(GetCurrentThreadId());

    let class_name = wide("MsAudioDockRemapperCls");
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(wndproc),
        hInstance: get_module_handle(),
        lpszClassName: class_name.as_ptr(),
        ..std::mem::zeroed()
    };
    RegisterClassW(&wc);

    // No tray context menu is created. A message-only window is sufficient for
    // Raw Input and the double-click callback, and never participates in focus.
    let hwnd = CreateWindowExW(
        0,
        class_name.as_ptr(),
        ptr::null(),
        0,
        0,
        0,
        0,
        0,
        HWND_MESSAGE,
        0,
        get_module_handle(),
        ptr::null_mut(),
    );
    if hwnd == 0 {
        alert("无法创建监听窗口（Raw Input 初始化失败）。");
        return;
    }

    let (action_tx, action_rx) = mpsc::channel::<Config>();
    let _action_worker = std::thread::spawn(move || {
        while let Ok(cfg) = action_rx.recv() {
            if let Err(e) = crate::actions::run(&cfg.buttons.teams, &cfg.settings) {
                crate::platform::alert(&format!("{} {e}", i18n::t("action_fail")));
            }
        }
    });

    let ctx = Box::new(Ctx {
        on_event,
        config,
        action_tx,
        devices: RefCell::new(HashMap::new()),
    });
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(ctx) as isize);

    rebuild(hwnd);
    create_tray(hwnd);

    let mut msg: MSG = std::mem::zeroed();
    loop {
        let r = GetMessageW(&mut msg, 0, 0, 0);
        if r == 0 || r == -1 {
            break;
        }
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    delete_tray(hwnd);
    let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ctx;
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    DestroyWindow(hwnd);
    if !raw.is_null() {
        drop(Box::from_raw(raw));
    }
    // Dropping Ctx closes action_tx. The worker is intentionally detached so a
    // misbehaving third-party Shell handler can never block application exit.
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_INPUT || msg == WM_INPUT_DEVICE_CHANGE || msg == APP_WM_TRAY {
        let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ctx;
        if !raw.is_null() {
            let ctx = &mut *raw;
            match msg {
                WM_INPUT => handle_raw_input(ctx, lparam as HRAWINPUT),
                WM_INPUT_DEVICE_CHANGE => rebuild(hwnd),
                APP_WM_TRAY => handle_tray(ctx, lparam as u32),
                _ => {}
            }
        }
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe fn handle_raw_input(ctx: &mut Ctx, hrawinput: HRAWINPUT) {
    let mut hdevice: HANDLE = 0;
    let mut reports: Vec<Vec<u8>> = Vec::new();
    if !parse_raw_input(hrawinput, &mut hdevice, &mut reports) {
        return;
    }

    let dev = match ctx.devices.borrow().get(&hdevice).cloned() {
        Some(d) => d,
        None => match read_device(hdevice) {
            Some(d) => {
                ctx.devices.borrow_mut().insert(hdevice, d.clone());
                d
            }
            None => return,
        },
    };

    // Never keep the shared config mutex while invoking external code. Shell
    // activation can pump messages or block in a third-party context-menu
    // handler; doing that inside wndproc previously froze both config saves and
    // the monitor message loop after changing the selected action.
    let cfg = ctx.config.lock().unwrap().clone();
    for r in &reports {
        if let Some(is_release) = match_teams(&dev, r, &cfg.device) {
            if !is_release {
                (ctx.on_event)(MonitorEvent::Press(crate::config::Button::Teams));
                if cfg.settings.enabled {
                    let _ = ctx.action_tx.send(cfg.clone());
                }
            }
        }
    }
}

unsafe fn parse_raw_input(
    hrawinput: HRAWINPUT,
    hdevice: &mut HANDLE,
    reports: &mut Vec<Vec<u8>>,
) -> bool {
    let header_size = std::mem::size_of::<RAWINPUTHEADER>();
    let mut size: u32 = 0;
    if GetRawInputData(
        hrawinput,
        RID_INPUT,
        ptr::null_mut(),
        &mut size,
        header_size as u32,
    ) == u32::MAX
    {
        return false;
    }
    if size == 0 {
        return false;
    }

    let mut buf = vec![0u8; size as usize];
    if GetRawInputData(
        hrawinput,
        RID_INPUT,
        buf.as_mut_ptr() as *mut c_void,
        &mut size,
        header_size as u32,
    ) == u32::MAX
    {
        return false;
    }

    if buf.len() < header_size {
        return false;
    }
    let dw_type = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if dw_type != RIM_TYPEHID {
        return false;
    }

    // RAWINPUTHEADER: dwType(4) dwSize(4) hDevice(8) wParam(8) on 64-bit.
    let hdev = isize::from_ne_bytes(buf[8..16].try_into().unwrap());
    *hdevice = hdev;

    let hid = header_size;
    let size_hid = u32::from_ne_bytes(buf[hid..hid + 4].try_into().unwrap()) as usize;
    let count = u32::from_ne_bytes(buf[hid + 4..hid + 8].try_into().unwrap()) as usize;
    let data = hid + 8;
    if size_hid == 0 || count == 0 {
        return false;
    }
    if data + size_hid * count > buf.len() {
        return false;
    }

    for i in 0..count {
        let start = data + i * size_hid;
        reports.push(buf[start..start + size_hid].to_vec());
    }
    true
}

unsafe fn rebuild(hwnd: HWND) {
    let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ctx;
    if raw.is_null() {
        return;
    }
    let ctx = &mut *raw;

    let devices = enumerate();

    // Only (re)register with Raw Input when the actual device set changed.
    //
    // `RegisterRawInputDevices` with `RIDEV_DEVNOTIFY` makes Windows immediately
    // post a `WM_INPUT_DEVICE_CHANGE` (GIDC_ARRIVAL) back to this window to signal
    // that the device is now subscribed. If we re-registered on *every*
    // device-change notification, that self-notification would re-enter `rebuild`
    // and re-register again, forever — a tight loop of ~thousands of empty
    // re-registrations per second that pinned the UI thread even while idle.
    // Comparing the HANDLE set against what we already have breaks the cycle:
    // the first registration's echo is a no-op, while real hot-plug arrivals /
    // removals still change the set and trigger a real re-register.
    let changed = {
        let map = ctx.devices.borrow();
        map.len() != devices.len() || devices.iter().any(|d| !map.contains_key(&d.hdevice))
    };

    {
        let mut map = ctx.devices.borrow_mut();
        map.clear();
        for d in &devices {
            map.insert(d.hdevice, d.clone());
        }
    }

    if changed && !devices.is_empty() {
        let regs: Vec<RAWINPUTDEVICE> = devices
            .iter()
            .map(|d| RAWINPUTDEVICE {
                usUsagePage: d.usage_page,
                usUsage: d.usage,
                dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY,
                hwndTarget: hwnd,
            })
            .collect();

        RegisterRawInputDevices(
            regs.as_ptr(),
            regs.len() as u32,
            std::mem::size_of::<RAWINPUTDEVICE>() as u32,
        );
    }

    (ctx.on_event)(MonitorEvent::Status(devices.len() as u32));
}

unsafe fn enumerate() -> Vec<DeviceInfo> {
    let mut num: u32 = 0;
    GetRawInputDeviceList(
        ptr::null_mut(),
        &mut num,
        std::mem::size_of::<RAWINPUTDEVICELIST>() as u32,
    );
    if num == 0 {
        return Vec::new();
    }

    let mut list: Vec<RAWINPUTDEVICELIST> = vec![std::mem::zeroed(); num as usize];
    if GetRawInputDeviceList(
        list.as_mut_ptr(),
        &mut num,
        std::mem::size_of::<RAWINPUTDEVICELIST>() as u32,
    ) == u32::MAX
    {
        return Vec::new();
    }

    let mut out = Vec::new();
    for entry in &list {
        if entry.dwType != RIM_TYPEHID {
            continue;
        }
        if let Some(d) = read_device(entry.hDevice) {
            out.push(d);
        }
    }
    out
}

unsafe fn read_device(hdevice: HANDLE) -> Option<DeviceInfo> {
    let path = get_device_name(hdevice)?;
    let (vid, pid) = parse_vid_pid(&path)?;
    if vid != 0x045E {
        return None; // only Microsoft Audio Dock families
    }
    let (usage_page, usage) = get_usage(hdevice)?;
    Some(DeviceInfo {
        hdevice,
        vid,
        pid,
        usage_page,
        usage,
    })
}

unsafe fn get_device_name(hdevice: HANDLE) -> Option<String> {
    let mut pcb: u32 = 0;
    GetRawInputDeviceInfoW(hdevice, RIDI_DEVICENAME, ptr::null_mut(), &mut pcb);
    if pcb == 0 {
        return None;
    }
    let mut buf: Vec<u16> = vec![0u16; pcb as usize];
    if GetRawInputDeviceInfoW(
        hdevice,
        RIDI_DEVICENAME,
        buf.as_mut_ptr() as *mut c_void,
        &mut pcb,
    ) == u32::MAX
    {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

unsafe fn get_usage(hdevice: HANDLE) -> Option<(u16, u16)> {
    let mut pcb: u32 = 0;
    GetRawInputDeviceInfoW(hdevice, RIDI_PREPARSEDDATA, ptr::null_mut(), &mut pcb);
    if pcb == 0 {
        return None;
    }
    let mut prep: Vec<u8> = vec![0u8; pcb as usize];
    if GetRawInputDeviceInfoW(
        hdevice,
        RIDI_PREPARSEDDATA,
        prep.as_mut_ptr() as *mut c_void,
        &mut pcb,
    ) == u32::MAX
    {
        return None;
    }
    let mut caps: HIDP_CAPS = std::mem::zeroed();
    if HidP_GetCaps(prep.as_ptr() as isize, &mut caps as *mut HIDP_CAPS) != HIDP_STATUS_SUCCESS {
        return None;
    }
    let usage = caps.Usage;
    let usage_page = caps.UsagePage;
    Some((usage_page, usage))
}

fn parse_vid_pid(path: &str) -> Option<(u16, u16)> {
    let vid_i = path.find("VID_")?;
    let vid = u16::from_str_radix(&path[vid_i + 4..vid_i + 8], 16).ok()?;
    let pid_i = path.find("PID_")?;
    let pid = u16::from_str_radix(&path[pid_i + 4..pid_i + 8], 16).ok()?;
    Some((vid, pid))
}

fn parse_hex(s: &str) -> Option<u16> {
    u16::from_str_radix(s.trim(), 16).ok()
}

/// Returns Some(is_release): false = press, true = release. None = not Teams.
fn match_teams(
    dev: &DeviceInfo,
    report: &[u8],
    filter: &crate::config::DeviceFilter,
) -> Option<bool> {
    let up = parse_hex(&filter.usage_page)?;
    let u = parse_hex(&filter.usage)?;
    if dev.usage_page != up || dev.usage != u {
        return None;
    }
    if report.len() < 2 {
        return None;
    }
    if report[0] != 0x9B {
        return None;
    }
    match report[1] {
        0x01 => Some(false),
        0x00 => Some(true),
        _ => None,
    }
}

// --- tray ------------------------------------------------------------------
unsafe fn create_tray(hwnd: HWND) {
    let icon: HICON = match crate::platform::tray_icon_path() {
        Some(p) => {
            let w = wide(&p.to_string_lossy());
            LoadImageW(
                0,
                w.as_ptr(),
                IMAGE_ICON,
                0,
                0,
                LR_LOADFROMFILE | LR_DEFAULTSIZE,
            ) as HICON
        }
        None => LoadIconW(0, IDI_APPLICATION),
    };
    let mut data: NOTIFYICONDATAW = std::mem::zeroed();
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = TRAY_ID;
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.uCallbackMessage = APP_WM_TRAY;
    data.hIcon = icon;
    let tip = wide("Microsoft Audio Dock Remapper");
    for (i, &w) in tip.iter().enumerate().take(127) {
        data.szTip[i] = w;
    }
    Shell_NotifyIconW(NIM_ADD, &data);
}

unsafe fn delete_tray(hwnd: HWND) {
    let mut data: NOTIFYICONDATAW = std::mem::zeroed();
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = TRAY_ID;
    Shell_NotifyIconW(NIM_DELETE, &data);
    // Nudge the notification area to repaint immediately. Without this the shell
    // may keep drawing the (now-deleted) icon until the mouse passes over it.
    refresh_notification_area();
}

/// Forces the taskbar notification area to re-scan and repaint by simulating a
/// mouse move across it. This clears any "ghost" icons left behind by owner
/// processes that have already removed their `Shell_NotifyIcon` entry.
unsafe fn refresh_notification_area() {
    use windows_sys::Win32::Foundation::{LPARAM, RECT};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowExW, FindWindowW, GetClientRect, SendMessageW, WM_MOUSEMOVE,
    };

    // Shell_TrayWnd > TrayNotifyWnd > SysPager > ToolbarWindow32 (holds icons).
    let tray = FindWindowW(wide("Shell_TrayWnd").as_ptr(), ptr::null());
    if tray == 0 {
        return;
    }
    let notify = FindWindowExW(tray, 0, wide("TrayNotifyWnd").as_ptr(), ptr::null());
    if notify == 0 {
        return;
    }
    let pager = FindWindowExW(notify, 0, wide("SysPager").as_ptr(), ptr::null());
    let toolbar = if pager != 0 {
        FindWindowExW(pager, 0, wide("ToolbarWindow32").as_ptr(), ptr::null())
    } else {
        0
    };
    let target = if toolbar != 0 { toolbar } else { notify };

    let mut rect: RECT = std::mem::zeroed();
    if GetClientRect(target, &mut rect) == 0 {
        return;
    }
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    let mut x = 0;
    while x < w {
        let mut y = 0;
        while y < h {
            let lparam = ((y as isize) << 16 | (x as isize & 0xFFFF)) as LPARAM;
            SendMessageW(target, WM_MOUSEMOVE, 0, lparam);
            y += 8;
        }
        x += 8;
    }
}

unsafe fn handle_tray(ctx: &mut Ctx, msg: u32) {
    if msg == WM_LBUTTONDBLCLK {
        (ctx.on_event)(MonitorEvent::TrayShow);
    }
}

// --- helpers ---------------------------------------------------------------
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn get_module_handle() -> windows_sys::Win32::Foundation::HINSTANCE {
    windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(ptr::null())
}
