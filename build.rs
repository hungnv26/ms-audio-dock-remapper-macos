//! Build script: embed the application icon (and version info) into the
//! Windows executable. `rc.exe` (the Windows Resource Compiler, shipped with
//! the Windows SDK / Visual Studio) must be reachable; on this machine it is
//! installed but not on `PATH`, so we locate it under the Windows SDK and
//! prepend it to `PATH` before invoking `winres`.

fn main() {
    // Widget style for the `slint!` macro: native-looking controls on each
    // platform. Read by the Slint compiler through SLINT_STYLE at macro
    // expansion, which is why it is set here and not in Cargo.toml.
    let style = if cfg!(target_os = "macos") {
        "cupertino"
    } else {
        "fluent"
    };
    println!("cargo:rustc-env=SLINT_STYLE={style}");
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(target_os = "macos")]
    {
        // Software replug of the Dock (IOUSBLib); see the C file for details.
        println!("cargo:rerun-if-changed=src/platform/usb_reenumerate.c");
        cc::Build::new()
            .file("src/platform/usb_reenumerate.c")
            .compile("usb_reenumerate");
        println!("cargo:rustc-link-lib=framework=IOKit");
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
    }

    #[cfg(windows)]
    {
        ensure_rc_on_path();

        let mut res = winres::WindowsResource::new();
        res.set_icon("public/app-icon.ico");
        res.set("ProductName", "MS Audio Dock Remapper");
        res.set(
            "FileDescription",
            "Remap the Microsoft Audio Dock Teams key",
        );
        res.set("LegalCopyright", "MIT");
        res.compile()
            .expect("failed to embed Windows resources (rc.exe)");
    }
}

/// Find `rc.exe` inside the Windows SDK and add its directory to `PATH` so the
/// resource compiler invoked by `winres` can be located. Searches the common
/// SDK install locations and accepts the first match.
#[cfg(windows)]
fn ensure_rc_on_path() {
    let candidates = [
        r"C:\Program Files (x86)\Windows Kits\10\bin",
        r"C:\Program Files\Windows Kits\10\bin",
    ];

    for base in candidates {
        let Ok(entries) = std::fs::read_dir(base) else {
            continue;
        };
        for entry in entries.flatten() {
            let rc = entry.path().join("x64").join("rc.exe");
            if rc.is_file() {
                if let Some(dir) = rc.parent() {
                    let dir = dir.to_string_lossy().to_string();
                    let path = std::env::var("PATH").unwrap_or_default();
                    if !path.split(';').any(|p| p.eq_ignore_ascii_case(&dir)) {
                        std::env::set_var("PATH", format!("{dir};{path}"));
                    }
                }
                return;
            }
        }
    }
}
