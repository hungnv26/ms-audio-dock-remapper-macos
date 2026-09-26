# Contributing to MS Audio Dock Remapper for macOS

Thanks for helping. This document covers setting up a development machine,
the project layout and the rules that keep the app safe to run against real
hardware.

## Supported development platform

Development and testing happen on macOS (11 or later). The Windows backend
inherited from the original project still compiles, but changes to it cannot
be verified here; keep them minimal and say so in the pull request.

## Development dependencies

1. **Xcode Command Line Tools** (`xcode-select --install`) for the C compiler
   and system frameworks.
2. **Rust** via rustup: `brew install rustup && rustup default stable`,
   then `rustup component add rustfmt clippy`.
3. A **Microsoft Audio Dock** for end-to-end testing. Unit tests and the CI
   smoke run work without one.

No Homebrew `hidapi` is needed: the `hidapi` crate builds the library itself.

## Running a development build

```bash
cargo run
```

The binary runs outside an app bundle, which is fine for development: the
menu bar item, the HID listener and the settings window all work. Log lines go
to stderr. For the bundled app (needed to test login items, the Accessibility
grant and Finder behaviour) use `./build-macos.sh` and run the `.app` it
produces.

Only one instance runs at a time (a `flock` in `~/Library/Caches`). Quit the
installed copy before running a development build. `build-macos.sh` names the
executable inside the bundle after the product, because macOS shows the
process name in the menu bar when the app is activated.

## Project structure

| Path | Purpose |
| --- | --- |
| `src/main.rs` | Startup: config, single instance, hands over to the UI |
| `src/ui.rs` | Settings window (Slint markup + Rust wiring) |
| `src/config.rs` | JSON config, per-button actions, migration from older files |
| `src/actions.rs` | Executing an action (app, command/URL, sound) |
| `src/installed_apps.rs` | Application discovery and launching per platform |
| `src/platform/mod.rs` | Backend surface every platform implements |
| `src/platform/macos.rs` | hidapi listener, exclusive mode and media-key re-posting, menu bar item, LaunchAgent, single instance |
| `src/platform/windows.rs` | Raw Input, tray icon, registry autostart (inherited) |
| `src/platform/stub.rs` | Placeholder for other platforms |
| `build.rs` | Picks the Slint widget style per platform; Windows resources |
| `build-macos.sh`, `packaging/macos/` | App bundle assembly and Info.plist |
| `public/` | Icons (app icon, icns, SVG glyphs) |
| `.github/workflows/macos-ci.yml` | Checks, tests, bundle build and artifact |

## Development rules

### Keep device access read-only

The app must never write to the Dock: no output or feature reports, no
firmware or driver interaction. Exclusive mode only changes who receives the
input reports.

### Never leave the media keys dead

Exclusive mode is allowed only while the app can re-post system media keys
(Accessibility trust). If that stops being true, the listener must fall back
to shared mode on its own. Test both directions.

### Keep the input path responsive

The HID read loop only parses reports and forwards events. Anything that can
block (launching, alerts, network) runs on the worker thread.

### Preserve configuration compatibility

`Config::load` must keep reading every earlier layout. Add fields with serde
defaults, keep aliases for renamed ones, and cover migrations with tests.

### Keep platform-specific code isolated

macOS APIs live in `src/platform/macos.rs` and the macOS parts of
`installed_apps.rs`; the UI and config never touch them directly. Other
platforms must keep compiling.

### Write code comments in English

## Formatting and automated tests

Before opening a pull request run the same gates as CI:

```bash
cargo fmt --all -- --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
./build-macos.sh
```

When a change touches the listener, also test against the Dock: every button,
unplug/replug, and the switch between shared and exclusive mode.
