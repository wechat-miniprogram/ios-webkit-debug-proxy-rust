// build.rs - build script
// Use pkg-config to discover and link libimobiledevice, libplist, libusbmuxd

fn install_hint(lib_name: &str) -> String {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let hint = match target_os.as_str() {
        "macos" => format!(
            "Cannot find {lib_name}, please make sure it is installed:\n  \
             macOS:  brew install {lib_name}"
        ),
        "windows" => format!(
            "Cannot find {lib_name}, please make sure it is installed:\n  \
             Windows: install via vcpkg or build from source with MSYS2/MinGW"
        ),
        _ => format!(
            "Cannot find {lib_name}, please make sure it is installed:\n  \
             Linux:  sudo apt install {lib_name}-dev  (or equivalent for your distro)"
        ),
    };
    hint
}

fn main() {
    // Link libimobiledevice (contains idevice_*, lockdownd_* functions)
    pkg_config::Config::new()
        .atleast_version("1.0")
        .probe("libimobiledevice-1.0")
        .expect(&install_hint("libimobiledevice"));

    // Link libusbmuxd (contains usbmuxd_* functions)
    pkg_config::Config::new()
        .atleast_version("2.0")
        .probe("libusbmuxd-2.0")
        .expect(&install_hint("libusbmuxd"));

    // On Windows, link to ws2_32 for socket support
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        println!("cargo:rustc-link-lib=ws2_32");
    }
}
