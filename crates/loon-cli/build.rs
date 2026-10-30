//! Emits the target triple the binary was compiled for.
//!
//! The updater must download the asset that matches *this* build. Reading the triple at
//! runtime is not possible from the standard library, and hardcoding `x86_64-unknown-linux-gnu`
//! would hand an aarch64 or macOS user the wrong binary — which the checksum would happily
//! validate, because it is a genuine asset for a different platform.

fn main() {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let vendor = std::env::var("CARGO_CFG_TARGET_VENDOR").unwrap_or_default();

    // Rebuild only when the target changes, not on every edit.
    println!("cargo:rerun-if-changed=build.rs");
    for key in [
        "CARGO_CFG_TARGET_ARCH",
        "CARGO_CFG_TARGET_OS",
        "CARGO_CFG_TARGET_ENV",
        "CARGO_CFG_TARGET_VENDOR",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }

    let target = match (arch.as_str(), os.as_str(), env.as_str()) {
        // The GNU and musl flavours of Linux differ in libc and are not interchangeable.
        ("x86_64", "linux", "gnu") => "x86_64-unknown-linux-gnu".to_string(),
        ("x86_64", "linux", "musl") => "x86_64-unknown-linux-musl".to_string(),
        ("aarch64", "linux", "gnu") => "aarch64-unknown-linux-gnu".to_string(),
        ("aarch64", "linux", "musl") => "aarch64-unknown-linux-musl".to_string(),
        ("x86_64", "macos", _) => "x86_64-apple-darwin".to_string(),
        ("aarch64", "macos", _) => "aarch64-apple-darwin".to_string(),
        ("x86_64", "windows", _) => "x86_64-pc-windows-msvc".to_string(),
        ("aarch64", "windows", _) => "aarch64-pc-windows-msvc".to_string(),
        // Anything else is still named honestly: a release published for this triple can be
        // matched even if the standard library has no shorthand for it here.
        (arch, os, env) if !arch.is_empty() && !os.is_empty() => {
            if vendor.is_empty() || env.is_empty() {
                format!("{arch}-unknown-{os}")
            } else {
                format!("{arch}-{vendor}-{os}-{env}")
            }
        }
        _ => "unknown-unknown-unknown".to_string(),
    };
    println!("cargo:rustc-env=LOON_BUILD_TARGET={target}");
}
