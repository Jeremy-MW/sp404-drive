// The logo, for the app icon drawn here (std only).
#[allow(dead_code)]
#[path = "src/bin/sp404drive-tray/logo.rs"]
mod logo;

use std::path::Path;

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

    resources();
}

/// The Windows resources: the app icon (drawn by `logo.rs`) and version information for
/// all three programs, and the manifest for the tray app (common controls 6 for its
/// dialogs, per-monitor DPI awareness for a sharp icon).
fn resources() {
    println!("cargo:rerun-if-changed=src/bin/sp404drive-tray/logo.rs");
    println!("cargo:rerun-if-changed=build.rs");
    let out = std::env::var("OUT_DIR").unwrap();
    let out = Path::new(&out);
    std::fs::write(out.join("app.ico"), logo::app_ico()).unwrap();

    let version = env!("CARGO_PKG_VERSION");
    let mut parts: Vec<u16> = version
        .split(['.', '-'])
        .filter_map(|p| p.parse().ok())
        .collect();
    parts.resize(4, 0);
    let numeric = parts
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let manifest = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="PolecatPlugins.SP404Drive.Tray" version="{}.{}.{}.{}"/>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0"
        processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>
"#,
        parts[0], parts[1], parts[2], parts[3]
    );
    std::fs::write(out.join("tray.manifest"), manifest).unwrap();

    let rc = |description: &str, file: &str, manifest: bool| {
        let mut rc = String::from("#pragma code_page(65001)\n1 ICON \"app.ico\"\n");
        if manifest {
            // RT_MANIFEST, CREATEPROCESS_MANIFEST_RESOURCE_ID
            rc.push_str("1 24 \"tray.manifest\"\n");
        }
        rc.push_str(&format!(
            r#"1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Polecat Plugins"
      VALUE "FileDescription", "{description}"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "{file}"
      VALUE "LegalCopyright", "Copyright © 2026 polecatplugins.com. GPL-3.0-only."
      VALUE "OriginalFilename", "{file}.exe"
      VALUE "ProductName", "SP-404 Drive"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
        ));
        rc
    };
    let tray_rc = out.join("sp404drive-tray.rc");
    std::fs::write(&tray_rc, rc("SP-404 Drive", "sp404drive-tray", true)).unwrap();
    embed_resource::compile_for(&tray_rc, ["sp404drive-tray"], embed_resource::NONE)
        .manifest_required()
        .unwrap();
    let cli_rc = out.join("sp404drive.rc");
    std::fs::write(&cli_rc, rc("SP-404 Drive command", "sp404drive", false)).unwrap();
    embed_resource::compile_for(&cli_rc, ["sp404drive"], embed_resource::NONE)
        .manifest_required()
        .unwrap();
    let host_rc = out.join("sp404drive-host.rc");
    std::fs::write(
        &host_rc,
        rc("SP-404 Drive (the drive)", "sp404drive-host", false),
    )
    .unwrap();
    embed_resource::compile_for(&host_rc, ["sp404drive-host"], embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
