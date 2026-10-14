//! Combines Cargo features with the target platform into two cfgs, so the
//! rest of the crate can ask "is this engine really in the build?" directly.

use std::env;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(system_engine)");
    println!("cargo::rustc-check-cfg=cfg(chromium_engine)");

    let feature = |name: &str| env::var_os(format!("CARGO_FEATURE_{name}")).is_some();
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // The web view that ships with the operating system.
    let system_engine = match target_os.as_str() {
        "windows" => feature("WEBVIEW2"),
        "macos" | "linux" | "dragonfly" | "freebsd" | "netbsd" | "openbsd" => feature("WEBKIT"),
        _ => false,
    };
    if system_engine {
        println!("cargo::rustc-cfg=system_engine");
    }

    // The Chromium backend is implemented for macOS only so far.
    if feature("CHROMIUM") && target_os == "macos" {
        println!("cargo::rustc-cfg=chromium_engine");
    }
}
