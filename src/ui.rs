//! Settings window, laid out like macOS System Settings: a sidebar with
//! sections on the left, grouped rows on the right. Widgets come from Slint's
//! platform style (cupertino on macOS, fluent on Windows; chosen in build.rs).
//!
//! Every change applies and saves immediately; there is no Save button.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::{
    invoke_from_event_loop, CloseRequestResponse, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer,
    Timer, TimerMode, VecModel, Weak,
};

use crate::actions;
use crate::autostart;
use crate::config::{ActionKind, Button as DockButton, Config};
use crate::i18n::t;
use crate::installed_apps::InstalledApp;
use crate::platform::{self, MonitorEvent};

slint::slint! {
    import { Button, ComboBox, LineEdit, ListView, ScrollView, Switch, Palette }
        from "std-widgets.slint";

    export struct ButtonEntry {
        name: string,
        summary: string,
        icon: image,
        tint: color,
    }

    export struct AppEntry {
        name: string,
        icon: image,
    }

    // System Settings look: light/dark pairs picked to match macOS. The
    // widget chrome itself (switches, popups, fields) comes from the style.
    global Theme {
        out property <bool> dark: Palette.color-scheme == ColorScheme.dark;
        out property <color> window: dark ? #1f1f1f : #f5f5f7;
        out property <color> sidebar: dark ? #2a2a2a : #ebebed;
        out property <color> sidebar-selected: dark ? #ffffff24 : #00000016;
        out property <color> sidebar-hover: dark ? #ffffff10 : #0000000a;
        out property <color> card: dark ? #2b2b2b : #ffffff;
        out property <color> card-border: dark ? #ffffff16 : #00000012;
        out property <color> divider: dark ? #ffffff14 : #0000000f;
        out property <color> row-hover: dark ? #ffffff0c : #0000000a;
        out property <color> row-selected: dark ? #ffffff16 : #00000012;
        out property <color> text: dark ? #f2f2f2 : #1d1d1f;
        out property <color> text2: dark ? #9d9da1 : #6e6e73;
        out property <color> accent: dark ? #0a84ff : #007aff;
        out property <color> green: #34c759;
        out property <color> gray: #8e8e93;
    }

    component IconBadge inherits Rectangle {
        in property <image> icon;
        in property <color> tint;
        in property <length> size: 22px;
        width: root.size;
        height: root.size;
        border-radius: root.size * 0.24;
        background: root.tint;
        Image {
            source: root.icon;
            colorize: white;
            width: root.size * 0.66;
            height: root.size * 0.66;
            x: (root.size - self.width) / 2;
            y: (root.size - self.height) / 2;
        }
    }

    component SidebarItem inherits Rectangle {
        in property <string> label;
        in property <image> icon;
        in property <color> tint;
        in property <bool> selected;
        callback clicked;
        height: 30px;
        border-radius: 6px;
        background: root.selected ? Theme.sidebar-selected
            : (ta.has-hover ? Theme.sidebar-hover : transparent);
        ta := TouchArea {
            clicked => { root.clicked(); }
        }
        HorizontalLayout {
            padding-left: 8px;
            padding-right: 8px;
            spacing: 9px;
            alignment: start;
            VerticalLayout {
                alignment: center;
                IconBadge { icon: root.icon; tint: root.tint; size: 20px; }
            }
            Text {
                text: root.label;
                font-size: 13px;
                color: Theme.text;
                vertical-alignment: center;
            }
        }
    }

    // A rounded, bordered card holding rows; optional bold title above.
    component Group inherits VerticalLayout {
        in property <string> title;
        spacing: 7px;
        if root.title != "": Text {
            text: root.title;
            font-size: 13px;
            font-weight: 600;
            color: Theme.text;
        }
        Rectangle {
            background: Theme.card;
            border-radius: 10px;
            border-width: 1px;
            border-color: Theme.card-border;
            clip: true;
            VerticalLayout {
                @children
            }
        }
    }

    // Label (+ optional detail line) on the left, controls on the right.
    component Row inherits Rectangle {
        in property <string> label;
        in property <string> detail;
        in property <bool> divider: true;
        in property <bool> selectable: false;
        in property <bool> selected: false;
        in property <image> icon;
        in property <color> tint: Theme.gray;
        in property <bool> show-icon: false;
        callback clicked;
        background: root.selected ? Theme.row-selected
            : (root.selectable && ta.has-hover ? Theme.row-hover : transparent);
        ta := TouchArea {
            enabled: root.selectable;
            clicked => { root.clicked(); }
        }
        VerticalLayout {
            HorizontalLayout {
                padding-left: 14px;
                padding-right: 12px;
                padding-top: 9px;
                padding-bottom: 9px;
                spacing: 12px;
                if root.show-icon: VerticalLayout {
                    alignment: center;
                    IconBadge { icon: root.icon; tint: root.tint; size: 26px; }
                }
                VerticalLayout {
                    alignment: center;
                    spacing: 2px;
                    horizontal-stretch: 1;
                    Text {
                        text: root.label;
                        font-size: 13px;
                        color: Theme.text;
                    }
                    if root.detail != "": Text {
                        text: root.detail;
                        font-size: 11px;
                        color: Theme.text2;
                        wrap: word-wrap;
                    }
                }
                VerticalLayout {
                    alignment: center;
                    horizontal-stretch: 0;
                    HorizontalLayout {
                        spacing: 8px;
                        alignment: end;
                        @children
                    }
                }
            }
            if root.divider: Rectangle {
                height: 1px;
                background: Theme.divider;
            }
        }
    }

    component PageTitle inherits Text {
        font-size: 22px;
        font-weight: 700;
        color: Theme.text;
    }

    component Caption inherits Text {
        font-size: 11px;
        color: Theme.text2;
        wrap: word-wrap;
    }

    export component AppWindow inherits Window {
        title: "Audio Dock";
        icon: root.app-icon;
        background: Theme.window;
        min-width: 700px;
        min-height: 600px;
        preferred-width: 780px;
        preferred-height: 820px;

        in property <image> app-icon;
        in-out property <int> page: 0;

        // Sidebar status
        in property <bool> connected: false;
        in property <string> status-text: "Not connected";
        in property <string> collections-text: "0";
        in property <string> device-text;
        in property <string> version;

        // Buttons page
        in property <[ButtonEntry]> buttons;
        in-out property <int> selected-button: 0;
        in property <string> detail-title;
        in property <string> detail-hint;
        in-out property <int> action-kind: 0;
        in property <[AppEntry]> apps;
        in-out property <string> app-filter;
        in property <int> selected-app: -1;
        in-out property <string> command;
        in-out property <string> arguments;
        in property <string> last-press-text: "No presses yet";
        in property <string> media-hint;

        // General page
        in-out property <bool> enabled: true;
        in-out property <bool> confirm-sound: true;
        in-out property <bool> launch-at-login: false;
        in-out property <bool> start-hidden: false;
        in property <string> start-hidden-hint;
        in property <bool> media-takeover-available: false;
        in property <string> accessibility-text;
        in property <bool> accessibility-granted: false;
        in property <bool> exclusive-active: false;
        // A media key has an action but the takeover is not active yet.
        in property <bool> needs-accessibility: false;

        callback button-selected(int);
        callback action-kind-changed(int);
        callback app-filter-changed(string);
        callback app-selected(int);
        callback command-changed(string);
        callback arguments-changed(string);
        callback test-action();
        callback setting-changed();
        callback open-accessibility-settings();
        callback open-repo();
        callback quit-app();

        HorizontalLayout {
            // ---- sidebar -------------------------------------------------
            Rectangle {
                width: 200px;
                background: Theme.sidebar;
                VerticalLayout {
                    padding: 12px;
                    padding-top: 16px;
                    spacing: 3px;
                    alignment: start;
                    HorizontalLayout {
                        padding-left: 6px;
                        padding-bottom: 14px;
                        spacing: 10px;
                        alignment: start;
                        VerticalLayout {
                            alignment: center;
                            Image { source: root.app-icon; width: 40px; height: 40px; }
                        }
                        VerticalLayout {
                            alignment: center;
                            spacing: 3px;
                            Text {
                                text: "Audio Dock";
                                font-size: 15px;
                                font-weight: 600;
                                color: Theme.text;
                            }
                            HorizontalLayout {
                                spacing: 5px;
                                alignment: start;
                                VerticalLayout {
                                    alignment: center;
                                    Rectangle {
                                        width: 8px;
                                        height: 8px;
                                        border-radius: 4px;
                                        background: root.connected ? Theme.green : Theme.gray;
                                    }
                                }
                                Text {
                                    text: root.status-text;
                                    font-size: 11px;
                                    color: Theme.text2;
                                    vertical-alignment: center;
                                }
                            }
                        }
                    }
                    SidebarItem {
                        label: "Buttons";
                        icon: @image-url("../public/icons/buttons.svg");
                        tint: #5e5ce6;
                        selected: root.page == 0;
                        clicked => { root.page = 0; }
                    }
                    SidebarItem {
                        label: "General";
                        icon: @image-url("../public/icons/general.svg");
                        tint: #8e8e93;
                        selected: root.page == 1;
                        clicked => { root.page = 1; }
                    }
                    SidebarItem {
                        label: "About";
                        icon: @image-url("../public/icons/about.svg");
                        tint: #0a84ff;
                        selected: root.page == 2;
                        clicked => { root.page = 2; }
                    }
                }
            }
            Rectangle { width: 1px; background: Theme.card-border; }

            // ---- content -------------------------------------------------
            ScrollView {
                viewport-width: self.visible-width;
                VerticalLayout {
                    padding: 26px;
                    padding-top: 22px;
                    spacing: 22px;
                    alignment: start;

                    if root.page == 0: VerticalLayout {
                        spacing: 22px;
                        alignment: start;
                        PageTitle { text: "Buttons"; }
                        Group {
                            title: "Dock buttons";
                            for entry[i] in root.buttons: Row {
                                label: entry.name;
                                icon: entry.icon;
                                tint: entry.tint;
                                show-icon: true;
                                selectable: true;
                                selected: root.selected-button == i;
                                divider: i < root.buttons.length - 1;
                                clicked => { root.button-selected(i); }
                                Text {
                                    text: entry.summary;
                                    font-size: 13px;
                                    color: Theme.text2;
                                    vertical-alignment: center;
                                }
                                Image {
                                    source: @image-url("../public/icons/chevron.svg");
                                    colorize: Theme.text2;
                                    width: 14px;
                                    height: 14px;
                                }
                            }
                        }
                        Group {
                            title: root.detail-title;
                            Row {
                                label: "When pressed";
                                detail: root.detail-hint;
                                ComboBox {
                                    width: 250px;
                                    model: ["No action", "Open an application", "Open a URL or run a command", "Play a sound"];
                                    current-index <=> root.action-kind;
                                    selected => { root.action-kind-changed(self.current-index); }
                                }
                            }
                            if root.action-kind == 1: Row {
                                label: "Application";
                                detail: "Pick the app to open or bring to the front";
                                LineEdit {
                                    width: 250px;
                                    placeholder-text: "Search applications";
                                    text <=> root.app-filter;
                                    edited => { root.app-filter-changed(self.text); }
                                }
                            }
                            if root.action-kind == 1: Rectangle {
                                height: 216px;
                                ListView {
                                    for app[i] in root.apps: Rectangle {
                                        height: 30px;
                                        background: root.selected-app == i ? Theme.accent
                                            : (item-ta.has-hover ? Theme.row-hover : transparent);
                                        item-ta := TouchArea {
                                            clicked => { root.app-selected(i); }
                                        }
                                        HorizontalLayout {
                                            padding-left: 14px;
                                            padding-right: 14px;
                                            spacing: 10px;
                                            VerticalLayout {
                                                alignment: center;
                                                Image { source: app.icon; width: 20px; height: 20px; }
                                            }
                                            Text {
                                                text: app.name;
                                                font-size: 13px;
                                                color: root.selected-app == i ? white : Theme.text;
                                                vertical-alignment: center;
                                            }
                                        }
                                    }
                                }
                                Rectangle { y: parent.height - 1px; height: 1px; background: Theme.divider; }
                            }
                            if root.action-kind == 2: Row {
                                label: "Command or URL";
                                detail: "A URL, a file, or a program to run";
                                LineEdit {
                                    width: 300px;
                                    placeholder-text: "https://example.com";
                                    text <=> root.command;
                                    edited => { root.command-changed(self.text); }
                                }
                            }
                            if root.action-kind == 2: Row {
                                label: "Arguments";
                                detail: "Optional, space separated";
                                LineEdit {
                                    width: 300px;
                                    text <=> root.arguments;
                                    edited => { root.arguments-changed(self.text); }
                                }
                            }
                            Row {
                                label: "Try the action";
                                detail: root.last-press-text;
                                divider: false;
                                Button {
                                    text: "Test";
                                    enabled: root.action-kind != 0;
                                    clicked => { root.test-action(); }
                                }
                            }
                        }
                        if root.needs-accessibility: Group {
                            Row {
                                label: "Accessibility access needed";
                                detail: "Until Audio Dock Remapper is allowed under Privacy & Security › Accessibility, macOS also performs this key's system function. The app switches over by itself once allowed.";
                                divider: false;
                                Button {
                                    text: "Open System Settings";
                                    clicked => { root.open-accessibility-settings(); }
                                }
                            }
                        }
                        Caption { text: root.media-hint; }
                    }

                    if root.page == 1: VerticalLayout {
                        spacing: 22px;
                        alignment: start;
                        PageTitle { text: "General"; }
                        Group {
                            title: "Remapping";
                            Row {
                                label: "Run button actions";
                                detail: "Turn off to pause all remapping without quitting";
                                Switch {
                                    checked <=> root.enabled;
                                    toggled => { root.setting-changed(); }
                                }
                            }
                            Row {
                                label: "Confirmation sound";
                                detail: "Play a short sound after an action runs";
                                divider: false;
                                Switch {
                                    checked <=> root.confirm-sound;
                                    toggled => { root.setting-changed(); }
                                }
                            }
                        }
                        Group {
                            title: "Startup";
                            Row {
                                label: "Launch at login";
                                Switch {
                                    checked <=> root.launch-at-login;
                                    toggled => { root.setting-changed(); }
                                }
                            }
                            Row {
                                label: "Start hidden";
                                detail: root.start-hidden-hint;
                                divider: false;
                                Switch {
                                    checked <=> root.start-hidden;
                                    toggled => { root.setting-changed(); }
                                }
                            }
                        }
                        if root.media-takeover-available: Group {
                            title: "Media keys";
                            Row {
                                label: "Accessibility access";
                                detail: root.accessibility-text;
                                divider: false;
                                Button {
                                    text: "Open System Settings";
                                    visible: !root.accessibility-granted;
                                    clicked => { root.open-accessibility-settings(); }
                                }
                            }
                        }
                    }

                    if root.page == 2: VerticalLayout {
                        spacing: 22px;
                        alignment: start;
                        PageTitle { text: "About"; }
                        HorizontalLayout {
                            spacing: 16px;
                            alignment: start;
                            VerticalLayout {
                                alignment: center;
                                Image { source: root.app-icon; width: 64px; height: 64px; }
                            }
                            VerticalLayout {
                                alignment: center;
                                spacing: 3px;
                                Text {
                                    text: "Audio Dock Remapper";
                                    font-size: 17px;
                                    font-weight: 700;
                                    color: Theme.text;
                                }
                                Caption { text: "Version " + root.version; }
                                Caption {
                                    text: "Remaps the buttons of the Microsoft Audio Dock. Monitoring is read-only: nothing is written to the device.";
                                }
                                Caption {
                                    text: "macOS port by Hung Ngo, based on Masterain's Windows remapper (MIT).";
                                }
                            }
                        }
                        Group {
                            title: "Device";
                            Row {
                                label: "Status";
                                Text { text: root.status-text; font-size: 13px; color: Theme.text2; vertical-alignment: center; }
                            }
                            Row {
                                label: "USB identity";
                                Text { text: root.device-text; font-size: 13px; color: Theme.text2; vertical-alignment: center; }
                            }
                            Row {
                                label: "Matching HID collections";
                                divider: false;
                                Text { text: root.collections-text; font-size: 13px; color: Theme.text2; vertical-alignment: center; }
                            }
                        }
                        Group {
                            Row {
                                label: "Source code";
                                detail: "github.com/hungnv26/ms-audio-dock-remapper-macos";
                                Button { text: "Open"; clicked => { root.open-repo(); } }
                            }
                            Row {
                                label: "Quit Audio Dock Remapper";
                                detail: "Stops listening to the Dock until the app is opened again";
                                divider: false;
                                Button { text: "Quit"; clicked => { root.quit-app(); } }
                            }
                        }
                    }
                }
            }
        }
    }
}

const REPO_URL: &str = "https://github.com/hungnv26/ms-audio-dock-remapper-macos";

const ACCESSIBILITY_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

/// Everything the callbacks need, shared through `Rc`.
struct State {
    config: Arc<Mutex<Config>>,
    apps: Vec<InstalledApp>,
    /// Indices into `apps` currently shown by the picker (after filtering).
    filtered: RefCell<Vec<usize>>,
    buttons: &'static [DockButton],
    selected: Cell<usize>,
    /// The Accessibility dialog is raised at most once per session.
    prompted: Cell<bool>,
    button_model: Rc<VecModel<ButtonEntry>>,
    app_model: Rc<VecModel<AppEntry>>,
}

impl State {
    fn current(&self) -> DockButton {
        self.buttons
            .get(self.selected.get())
            .copied()
            .unwrap_or(DockButton::Teams)
    }

    fn save(&self) {
        if let Err(e) = self.config.lock().unwrap().save() {
            platform::alert(&format!("{} {e}", t("save_fail")));
        }
    }

    /// Rewrites the summary column of every button row in place and the
    /// media-key caption under the list.
    fn refresh_buttons(&self, ui: &AppWindow) {
        let cfg = self.config.lock().unwrap();
        for (i, button) in self.buttons.iter().enumerate() {
            if let Some(mut entry) = self.button_model.row_data(i) {
                entry.summary = cfg.buttons.get(*button).summary().into();
                self.button_model.set_row_data(i, entry);
            }
        }
        let media_bound = DockButton::MEDIA
            .iter()
            .any(|b| cfg.buttons.get(*b).kind != ActionKind::None);
        drop(cfg);
        let exclusive = ui.get_exclusive_active();
        let granted = ui.get_accessibility_granted();
        let takeover = platform::supports_media_key_takeover();
        ui.set_needs_accessibility(takeover && media_bound && !granted);
        ui.set_media_hint(media_hint(media_bound, exclusive, granted).into());
    }

    /// Re-reads Accessibility trust and updates the texts that show it.
    fn refresh_accessibility(&self, ui: &AppWindow) {
        let granted = platform::accessibility_trusted(false);
        ui.set_accessibility_granted(granted);
        ui.set_accessibility_text(
            if granted {
                "Granted. A remapped media key does only your action."
            } else {
                "Needed so a remapped Play/Pause or Volume key stops reaching the system. Allow Audio Dock Remapper under Privacy & Security › Accessibility."
            }
            .into(),
        );
    }

    /// Raises the system Accessibility dialog (once per session) when a media
    /// key just received an action and the takeover cannot start without it.
    fn ensure_accessibility(&self, ui: &AppWindow) {
        if !platform::supports_media_key_takeover() || self.prompted.get() {
            return;
        }
        let bound = {
            let cfg = self.config.lock().unwrap();
            cfg.buttons.get(self.current()).kind != ActionKind::None
        };
        if bound && self.current().is_media() && !ui.get_accessibility_granted() {
            self.prompted.set(true);
            platform::accessibility_trusted(true);
            self.refresh_accessibility(ui);
        }
    }

    /// Loads the selected button's action into the detail group.
    fn refresh_detail(&self, ui: &AppWindow) {
        let button = self.current();
        let action = self.config.lock().unwrap().buttons.get(button).clone();
        ui.set_selected_button(self.selected.get() as i32);
        ui.set_detail_title(format!("{} button", button.label()).into());
        ui.set_detail_hint(button.builtin_behavior().into());
        ui.set_action_kind(action.kind.index() as i32);
        ui.set_command(action.command.into());
        ui.set_arguments(action.arguments.into());
        ui.set_app_filter("".into());
        self.rebuild_apps(ui, "");
        self.ensure_accessibility(ui);
    }

    /// Filters the picker by `query` and highlights the bound app, if listed.
    fn rebuild_apps(&self, ui: &AppWindow, query: &str) {
        let query = query.trim().to_lowercase();
        let target = self
            .config
            .lock()
            .unwrap()
            .buttons
            .get(self.current())
            .app_target
            .clone();
        let mut filtered = Vec::new();
        let mut entries = Vec::new();
        let mut selected = -1;
        for (i, app) in self.apps.iter().enumerate() {
            if !query.is_empty() && !app.name.to_lowercase().contains(&query) {
                continue;
            }
            if app.target.eq_ignore_ascii_case(&target) {
                selected = filtered.len() as i32;
            }
            filtered.push(i);
            entries.push(AppEntry {
                name: app.name.clone().into(),
                icon: app_icon(app),
            });
        }
        *self.filtered.borrow_mut() = filtered;
        self.app_model.set_vec(entries);
        ui.set_selected_app(selected);
    }
}

/// Builds the window, wires every control to the config and runs the Slint
/// event loop until quit. `start_minimized` (persisted setting or the
/// `--minimized` login switch) keeps the window hidden; the resident monitor
/// and the tray / menu bar item start either way.
pub fn run(config: Arc<Mutex<Config>>, start_minimized: bool) {
    let mut apps = match crate::installed_apps::list() {
        Ok(apps) => apps,
        Err(error) => {
            platform::alert(&format!("{} {error}", t("app_list_fail")));
            Vec::new()
        }
    };
    // Keep bound apps visible even when they disappeared from the folders, so
    // merely opening the window never silently changes a binding.
    {
        let cfg = config.lock().unwrap();
        for button in DockButton::ALL {
            let action = cfg.buttons.get(button);
            if action.kind == ActionKind::App
                && !action.app_target.is_empty()
                && !apps
                    .iter()
                    .any(|app| app.target.eq_ignore_ascii_case(&action.app_target))
            {
                apps.push(InstalledApp {
                    name: format!("{} (missing)", action.app_name),
                    target: action.app_target.clone(),
                    registered: false,
                    icon_rgba: Vec::new(),
                });
            }
        }
    }

    let ui = AppWindow::new().unwrap();
    if let Some(icon_path) = platform::header_icon_path() {
        if let Ok(img) = slint::Image::load_from_path(&icon_path) {
            ui.set_app_icon(img);
        }
    }
    ui.set_version(env!("CARGO_PKG_VERSION").into());
    ui.set_start_hidden_hint(start_hidden_hint().into());
    {
        let cfg = config.lock().unwrap();
        ui.set_device_text(
            format!(
                "VID {} · PID {} · usage {}/{}",
                cfg.device.vendor_id,
                cfg.device.product_id,
                cfg.device.usage_page,
                cfg.device.usage
            )
            .into(),
        );
        ui.set_enabled(cfg.settings.enabled);
        ui.set_confirm_sound(cfg.settings.play_confirmation_beep);
        ui.set_start_hidden(cfg.settings.start_hidden);
    }
    ui.set_launch_at_login(autostart::is_enabled());
    ui.set_media_takeover_available(platform::supports_media_key_takeover());
    ui.window().set_size(slint::LogicalSize::new(780.0, 820.0));

    let buttons = platform::supported_buttons();
    let button_model = Rc::new(VecModel::from(
        buttons
            .iter()
            .map(|b| ButtonEntry {
                name: b.label().into(),
                summary: "".into(),
                icon: button_icon(*b),
                tint: button_tint(*b),
            })
            .collect::<Vec<_>>(),
    ));
    ui.set_buttons(ModelRc::from(button_model.clone()));
    let app_model = Rc::new(VecModel::from(Vec::new()));
    ui.set_apps(ModelRc::from(app_model.clone()));

    let state = Rc::new(State {
        config: config.clone(),
        apps,
        filtered: RefCell::new(Vec::new()),
        buttons,
        selected: Cell::new(0),
        prompted: Cell::new(false),
        button_model,
        app_model,
    });
    state.refresh_accessibility(&ui);
    state.refresh_buttons(&ui);
    state.refresh_detail(&ui);

    // ---- callbacks ----------------------------------------------------------
    {
        let st = state.clone();
        let uiw = ui.as_weak();
        ui.on_button_selected(move |index| {
            if let (Some(ui), Ok(index)) = (uiw.upgrade(), usize::try_from(index)) {
                if index < st.buttons.len() {
                    st.selected.set(index);
                    st.refresh_detail(&ui);
                }
            }
        });
    }
    {
        let st = state.clone();
        let uiw = ui.as_weak();
        ui.on_action_kind_changed(move |index| {
            let kind = ActionKind::from_index(usize::try_from(index).unwrap_or(0));
            let button = st.current();
            st.config.lock().unwrap().buttons.get_mut(button).kind = kind;
            st.save();
            if let Some(ui) = uiw.upgrade() {
                st.refresh_buttons(&ui);
                st.rebuild_apps(&ui, ui.get_app_filter().as_str());
                st.ensure_accessibility(&ui);
            }
        });
    }
    {
        let st = state.clone();
        let uiw = ui.as_weak();
        ui.on_app_filter_changed(move |query| {
            if let Some(ui) = uiw.upgrade() {
                st.rebuild_apps(&ui, query.as_str());
            }
        });
    }
    {
        let st = state.clone();
        let uiw = ui.as_weak();
        ui.on_app_selected(move |index| {
            let Ok(index) = usize::try_from(index) else {
                return;
            };
            let Some(app) = st
                .filtered
                .borrow()
                .get(index)
                .and_then(|i| st.apps.get(*i))
                .cloned()
            else {
                return;
            };
            {
                let mut cfg = st.config.lock().unwrap();
                let action = cfg.buttons.get_mut(st.current());
                action.kind = ActionKind::App;
                action.app_name = app.name.trim_end_matches(" (missing)").to_string();
                action.app_target = app.target;
            }
            st.save();
            if let Some(ui) = uiw.upgrade() {
                st.refresh_buttons(&ui);
                ui.set_selected_app(index as i32);
                st.ensure_accessibility(&ui);
            }
        });
    }
    {
        let st = state.clone();
        let uiw = ui.as_weak();
        ui.on_command_changed(move |text| {
            st.config
                .lock()
                .unwrap()
                .buttons
                .get_mut(st.current())
                .command = text.to_string();
            st.save();
            if let Some(ui) = uiw.upgrade() {
                st.refresh_buttons(&ui);
            }
        });
    }
    {
        let st = state.clone();
        ui.on_arguments_changed(move |text| {
            st.config
                .lock()
                .unwrap()
                .buttons
                .get_mut(st.current())
                .arguments = text.to_string();
            st.save();
        });
    }
    {
        let st = state.clone();
        ui.on_test_action(move || {
            let cfg = st.config.lock().unwrap().clone();
            if let Err(e) = actions::run(cfg.buttons.get(st.current()), &cfg.settings) {
                platform::alert(&format!("{} {e}", t("action_fail")));
            }
        });
    }
    {
        let st = state.clone();
        let uiw = ui.as_weak();
        ui.on_setting_changed(move || {
            let Some(ui) = uiw.upgrade() else {
                return;
            };
            let launch = ui.get_launch_at_login();
            let hidden = ui.get_start_hidden();
            {
                let mut cfg = st.config.lock().unwrap();
                cfg.settings.enabled = ui.get_enabled();
                cfg.settings.play_confirmation_beep = ui.get_confirm_sound();
                cfg.settings.launch_at_login = launch;
                cfg.settings.start_hidden = hidden;
            }
            // The login entry carries `--minimized` too, so a silent start does
            // not depend on the config file being readable at sign-in.
            autostart::set_enabled(launch, hidden);
            st.save();
        });
    }
    ui.on_open_accessibility_settings(|| {
        let _ = open::that_detached(ACCESSIBILITY_SETTINGS_URL);
    });
    ui.on_open_repo(|| {
        let _ = open::that_detached(REPO_URL);
    });
    ui.on_quit_app(|| {
        platform::request_quit();
        let _ = slint::quit_event_loop();
    });

    // The title-bar close button hides the window; the process keeps running
    // for the tray / menu bar item. Quitting is explicit (About page or menu).
    {
        let uiw = ui.as_weak();
        ui.window().on_close_requested(move || {
            if let Some(ui) = uiw.upgrade() {
                ui.window().hide().ok();
            }
            CloseRequestResponse::KeepWindowShown
        });
    }

    // ---- monitor events, event-driven ----------------------------------------
    // Events are buffered in a channel (so nothing is lost before the loop
    // starts) and drained on the UI thread via `invoke_from_event_loop`.
    let (tx, rx) = mpsc::channel::<MonitorEvent>();
    let rx = Arc::new(Mutex::new(rx));
    let ui_weak = ui.as_weak();
    // The pump runs on the UI thread, so it may hold the (non-Send) state; it
    // is parked in a thread-local slot for the Send closure to reach it.
    PUMP_STATE.with(|slot| *slot.borrow_mut() = Some(state.clone()));
    let on_event = {
        let rx = rx.clone();
        let ui_weak = ui_weak.clone();
        move |ev: MonitorEvent| {
            let _ = tx.send(ev);
            let rx = rx.clone();
            let ui_weak = ui_weak.clone();
            let _ = invoke_from_event_loop(move || pump_events(&rx, &ui_weak));
        }
    };
    platform::start_monitor(on_event, config.clone());
    {
        let rx = rx.clone();
        let ui_weak = ui_weak.clone();
        let flush = Timer::default();
        flush.start(TimerMode::SingleShot, Duration::from_millis(0), move || {
            pump_events(&rx, &ui_weak);
        });
    }

    let outcome = if start_minimized {
        slint::run_event_loop_until_quit()
    } else {
        ui.window()
            .show()
            .and_then(|()| slint::run_event_loop_until_quit())
    };
    if let Err(error) = outcome {
        handle_backend_failure(&error);
    }
}

thread_local! {
    static PUMP_STATE: RefCell<Option<Rc<State>>> = const { RefCell::new(None) };
}

fn media_hint(media_bound: bool, exclusive: bool, granted: bool) -> &'static str {
    if platform::supported_buttons().len() <= 1 {
        "On Windows only the Teams key can be observed by this app."
    } else if exclusive {
        "Remapped media keys do only your action; the others keep their system role through the app. \
         Holding a volume key no longer repeats."
    } else if media_bound && granted {
        "Taking the media keys over from the system…"
    } else if media_bound {
        "Play/Pause and Volume actions currently run in addition to the system's own handling of \
         those keys, until Accessibility access is granted."
    } else {
        "The Teams key is ignored by the system, which makes it the natural one to remap. Media keys \
         keep their system role until you give them an action."
    }
}

fn start_hidden_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "Show only the menu bar icon at launch"
    } else {
        "Show only the tray icon at launch"
    }
}

fn button_icon(button: DockButton) -> slint::Image {
    let svg: &[u8] = match button {
        DockButton::Teams => include_bytes!("../public/icons/teams.svg"),
        DockButton::PlayPause => include_bytes!("../public/icons/play-pause.svg"),
        DockButton::VolumeDown => include_bytes!("../public/icons/volume-down.svg"),
        DockButton::VolumeUp => include_bytes!("../public/icons/volume-up.svg"),
        DockButton::MicMute => include_bytes!("../public/icons/mic.svg"),
    };
    slint::Image::load_from_svg_data(svg).unwrap_or_default()
}

fn button_tint(button: DockButton) -> slint::Color {
    let argb = match button {
        DockButton::Teams => 0xff5b5fc7,
        DockButton::PlayPause => 0xffff9f0a,
        DockButton::VolumeDown => 0xff30b0c7,
        DockButton::VolumeUp => 0xff30b0c7,
        DockButton::MicMute => 0xffff3b30,
    };
    slint::Color::from_argb_encoded(argb)
}

fn app_icon(app: &InstalledApp) -> slint::Image {
    const ICON_SIZE: u32 = 32;
    if app.icon_rgba.len() != (ICON_SIZE * ICON_SIZE * 4) as usize {
        return slint::Image::default();
    }
    let buffer =
        SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&app.icon_rgba, ICON_SIZE, ICON_SIZE);
    slint::Image::from_rgba8_premultiplied(buffer)
}

/// Slint's default renderer needs a working GPU driver. Machines without one
/// fail inside `show()` / `run_event_loop_until_quit`; the software renderer
/// is compiled in, so try once more with it forced on before giving up.
fn handle_backend_failure(error: &slint::PlatformError) {
    if relaunch_with_software_renderer() {
        return;
    }
    platform::alert(&format!("{} {error}", t("render_fail")));
}

const SOFTWARE_BACKEND: &str = "winit-software";

/// Re-executes this process with the software renderer forced on, handing
/// over the tray icon and the single-instance slot first. False when a backend
/// was already pinned (so a still-broken relaunch never loops) or the
/// replacement could not be started.
fn relaunch_with_software_renderer() -> bool {
    if std::env::var_os("SLINT_BACKEND").is_some() {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    platform::request_quit();
    platform::release_single_instance();
    std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env("SLINT_BACKEND", SOFTWARE_BACKEND)
        .spawn()
        .is_ok()
}

fn now_hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// Drains every buffered `MonitorEvent` on the UI thread.
fn pump_events(rx: &Arc<Mutex<Receiver<MonitorEvent>>>, ui_weak: &Weak<AppWindow>) {
    let Some(ui) = ui_weak.upgrade() else {
        return;
    };
    while let Ok(ev) = rx.lock().unwrap().try_recv() {
        apply_event(&ui, ev);
    }
}

fn apply_event(ui: &AppWindow, ev: MonitorEvent) {
    match ev {
        MonitorEvent::Press(button) => {
            ui.set_last_press_text(
                format!("Last press: {} at {}", button.label(), now_hms()).into(),
            );
        }
        MonitorEvent::Status(n) => {
            ui.set_connected(n > 0);
            ui.set_status_text(if n > 0 { "Connected" } else { "Not connected" }.into());
            ui.set_collections_text(n.to_string().into());
        }
        MonitorEvent::Exclusive(exclusive) => {
            ui.set_exclusive_active(exclusive);
            PUMP_STATE.with(|slot| {
                if let Some(state) = slot.borrow().as_ref() {
                    state.refresh_accessibility(ui);
                    state.refresh_buttons(ui);
                }
            });
        }
        MonitorEvent::TrayShow => {
            let _ = ui.show();
        }
        MonitorEvent::Quit => {
            platform::request_quit();
            let _ = slint::quit_event_loop();
        }
    }
}
