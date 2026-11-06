//! What the tray app keeps in the registry: its settings, Open at Sign-in, and (for the
//! installer) the command's folder on the system PATH.

use std::ffi::c_void;
use std::path::Path;

use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, LPARAM, WIN32_ERROR, WPARAM,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_BINARY, REG_DWORD, REG_EXPAND_SZ,
    REG_ROUTINE_FLAGS, REG_SZ, REG_VALUE_TYPE, RRF_NOEXPAND, RRF_RT_REG_BINARY, RRF_RT_REG_DWORD,
    RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
};
use windows::core::{HSTRING, w};

const SETTINGS: &str = r"Software\Polecat Plugins\SP-404 Drive";
const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Where Windows records apps switched off under Settings > Apps > Startup.
const STARTUP_APPROVED: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const RUN_VALUE: &str = "SP-404 Drive";
const ENVIRONMENT: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
const PERSONALIZE: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";

fn check(e: WIN32_ERROR) -> windows::core::Result<()> {
    if e == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(e.into())
    }
}

fn get_dword(root: HKEY, key: &str, value: &str) -> Option<u32> {
    let mut data = 0u32;
    let mut len = size_of::<u32>() as u32;
    // SAFETY: reads at most `len` bytes into `data`.
    let e = unsafe {
        RegGetValueW(
            root,
            &HSTRING::from(key),
            &HSTRING::from(value),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut c_void),
            Some(&mut len),
        )
    };
    (e == ERROR_SUCCESS).then_some(data)
}

fn set_dword(root: HKEY, key: &str, value: &str, data: u32) -> windows::core::Result<()> {
    // SAFETY: writes the four bytes of `data`; creates the key if missing.
    check(unsafe {
        RegSetKeyValueW(
            root,
            &HSTRING::from(key),
            &HSTRING::from(value),
            REG_DWORD.0,
            Some(&data as *const u32 as *const c_void),
            size_of::<u32>() as u32,
        )
    })
}

/// A value's raw bytes and type, unexpanded; `None` if it doesn't exist.
fn get_raw(
    root: HKEY,
    key: &str,
    value: &str,
    flags: REG_ROUTINE_FLAGS,
) -> windows::core::Result<Option<(REG_VALUE_TYPE, Vec<u8>)>> {
    let (key, value) = (HSTRING::from(key), HSTRING::from(value));
    loop {
        let mut len = 0u32;
        // SAFETY: asks for the size only.
        let e = unsafe { RegGetValueW(root, &key, &value, flags, None, None, Some(&mut len)) };
        if e == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        check(e)?;
        let mut data = vec![0u8; len as usize];
        let mut kind = REG_VALUE_TYPE::default();
        // SAFETY: reads at most `len` bytes into `data`.
        let e = unsafe {
            RegGetValueW(
                root,
                &key,
                &value,
                flags,
                Some(&mut kind),
                Some(data.as_mut_ptr() as *mut c_void),
                Some(&mut len),
            )
        };
        // Grown between the two calls: ask again.
        if e == windows::Win32::Foundation::ERROR_MORE_DATA {
            continue;
        }
        check(e)?;
        data.truncate(len as usize);
        return Ok(Some((kind, data)));
    }
}

fn get_string(
    root: HKEY,
    key: &str,
    value: &str,
) -> windows::core::Result<Option<(REG_VALUE_TYPE, String)>> {
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
    Ok(get_raw(root, key, value, flags)?.map(|(kind, bytes)| {
        let wide: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
        (kind, String::from_utf16_lossy(&wide[..end]))
    }))
}

fn set_string(
    root: HKEY,
    key: &str,
    value: &str,
    kind: REG_VALUE_TYPE,
    data: &str,
) -> windows::core::Result<()> {
    let wide = HSTRING::from(data);
    let bytes = (wide.len() + 1) * 2;
    // SAFETY: `wide` is NUL-terminated and `bytes` long; creates the key if missing.
    check(unsafe {
        RegSetKeyValueW(
            root,
            &HSTRING::from(key),
            &HSTRING::from(value),
            kind.0,
            Some(wide.as_ptr() as *const c_void),
            bytes as u32,
        )
    })
}

fn delete(root: HKEY, key: &str, value: &str) -> windows::core::Result<()> {
    // SAFETY: deletes one value.
    let e = unsafe { RegDeleteKeyValueW(root, &HSTRING::from(key), &HSTRING::from(value)) };
    if e == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        check(e)
    }
}

/// "Mount Automatically When Switched On"; on unless switched off.
pub fn auto_mount() -> bool {
    get_dword(HKEY_CURRENT_USER, SETTINGS, "MountAutomatically") != Some(0)
}

pub fn set_auto_mount(on: bool) -> windows::core::Result<()> {
    set_dword(HKEY_CURRENT_USER, SETTINGS, "MountAutomatically", on as u32)
}

/// Whether the first-run message has been shown.
pub fn welcomed() -> bool {
    get_dword(HKEY_CURRENT_USER, SETTINGS, "Welcomed") == Some(1)
}

pub fn set_welcomed() -> windows::core::Result<()> {
    set_dword(HKEY_CURRENT_USER, SETTINGS, "Welcomed", 1)
}

/// Whether the app starts at sign-in: listed in the Run key, and not switched off under
/// Settings > Apps > Startup (an odd first byte in StartupApproved means off).
pub fn open_at_sign_in() -> bool {
    let listed = matches!(get_string(HKEY_CURRENT_USER, RUN, RUN_VALUE), Ok(Some(_)));
    let switched_off = matches!(
        get_raw(HKEY_CURRENT_USER, STARTUP_APPROVED, RUN_VALUE, RRF_RT_REG_BINARY),
        Ok(Some((REG_BINARY, bytes))) if bytes.first().is_some_and(|b| b & 1 == 1)
    );
    listed && !switched_off
}

/// Start `exe` at sign-in, or not. Switching it on also clears a switch-off made in
/// Settings, so the menu's tick means what it says.
pub fn set_open_at_sign_in(exe: &Path, on: bool) -> windows::core::Result<()> {
    delete(HKEY_CURRENT_USER, STARTUP_APPROVED, RUN_VALUE)?;
    if on {
        let command = format!("\"{}\"", exe.display());
        set_string(HKEY_CURRENT_USER, RUN, RUN_VALUE, REG_SZ, &command)
    } else {
        delete(HKEY_CURRENT_USER, RUN, RUN_VALUE)
    }
}

/// Whether the taskbar is light (Settings > Personalisation > Colours, "Choose your
/// default Windows mode"); dark if unset.
pub fn light_taskbar() -> bool {
    get_dword(HKEY_CURRENT_USER, PERSONALIZE, "SystemUsesLightTheme") == Some(1)
}

/// One PATH entry, as compared: no trailing backslash, ignoring case.
fn path_key(entry: &str) -> String {
    entry.trim().trim_end_matches('\\').to_lowercase()
}

/// Add `dir` to the system PATH, or take it off (`add` false). Keeps the value's type
/// (REG_EXPAND_SZ) and every other entry as it is, then tells running programs. Needs
/// administrator rights: the installer runs it.
pub fn change_system_path(dir: &Path, add: bool) -> windows::core::Result<()> {
    let dir = dir.display().to_string();
    let (kind, path) = get_string(HKEY_LOCAL_MACHINE, ENVIRONMENT, "Path")?
        .unwrap_or((REG_EXPAND_SZ, String::new()));
    let mut entries: Vec<&str> = path.split(';').filter(|e| !e.trim().is_empty()).collect();
    let present = entries.iter().any(|e| path_key(e) == path_key(&dir));
    if add == present {
        return Ok(());
    }
    if add {
        entries.push(&dir);
    } else {
        entries.retain(|e| path_key(e) != path_key(&dir));
    }
    let kind = if kind == REG_SZ {
        REG_SZ
    } else {
        REG_EXPAND_SZ
    };
    set_string(
        HKEY_LOCAL_MACHINE,
        ENVIRONMENT,
        "Path",
        kind,
        &entries.join(";"),
    )?;
    // SAFETY: a broadcast with a static string; a hung window doesn't hold it up.
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(w!("Environment").as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            5000,
            None,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_entries_compare_loosely() {
        assert_eq!(
            path_key(r"C:\Program Files\Polecat Plugins\SP-404 Drive\"),
            path_key(r"c:\program files\polecat plugins\sp-404 drive")
        );
    }
}
