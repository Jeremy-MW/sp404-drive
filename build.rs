fn main() {
    // WinFsp is only ever delay-loaded (src/windows/mod.rs loads the installed DLL before
    // first use). Same flags as winfsp::build::winfsp_link_delayload, for MSVC x64/arm64.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }
    let dll = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "winfsp-a64.dll",
        Ok("x86") => "winfsp-x86.dll",
        _ => "winfsp-x64.dll",
    };
    println!("cargo:rustc-link-lib=dylib=delayimp");
    println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
}
