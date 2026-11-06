//! The Windows pieces of the tray app: its notification-area icon, the menu and the
//! dialogs. None of these keep any state of the app's own.

use std::ffi::c_void;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, POINT, RECT, S_OK, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
};
use windows::Win32::UI::Controls::{
    TASKDIALOG_BUTTON, TASKDIALOG_NOTIFICATIONS, TASKDIALOGCONFIG, TASKDIALOGCONFIG_0,
    TD_INFORMATION_ICON, TD_WARNING_ICON, TDCBF_OK_BUTTON, TDF_ALLOW_DIALOG_CANCELLATION,
    TDF_ENABLE_HYPERLINKS, TDF_VERIFICATION_FLAG_CHECKED, TDN_HYPERLINK_CLICKED,
    TaskDialogIndirect,
};
use windows::Win32::UI::HiDpi::GetSystemMetricsForDpi;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER, Shell_NotifyIconGetRect,
    Shell_NotifyIconW, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos,
    HICON, HMENU, ICONINFO, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, PostMessageW,
    SM_CXSMICON, SW_SHOWNORMAL, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_NONOTIFY,
    TPM_RETURNCMD, TPM_RIGHTALIGN, TPM_RIGHTBUTTON, TPM_TOPALIGN, TrackPopupMenuEx, WM_NULL,
};
use windows::core::{BOOL, HRESULT, HSTRING, PCWSTR, w};

use crate::logo::{self, Glyph};

/// The id of the app's one notification-area icon.
const ICON_ID: u32 = 1;

/// The notification-area icon, drawn for the taskbar's size and colour.
pub struct TrayIcon {
    window: HWND,
    callback_message: u32,
    icon: HICON,
}

impl TrayIcon {
    pub fn new(window: HWND, callback_message: u32) -> Self {
        TrayIcon {
            window,
            callback_message,
            icon: HICON::default(),
        }
    }

    fn data(&self, tip: &str) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.window,
            uID: ICON_ID,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: self.callback_message,
            hIcon: self.icon,
            ..Default::default()
        };
        let tip: Vec<u16> = tip.encode_utf16().take(data.szTip.len() - 1).collect();
        data.szTip[..tip.len()].copy_from_slice(&tip);
        data
    }

    /// Redraw the icon and set the tooltip.
    pub fn draw(&mut self, glyph: Glyph, dimmed: bool, tip: &str) {
        let old = self.icon;
        self.icon = draw_icon(self.window, glyph, dimmed);
        // SAFETY: the data points at nothing that outlives the call.
        unsafe {
            if !Shell_NotifyIconW(NIM_MODIFY, &self.data(tip)).as_bool() {
                // Not there yet, or Explorer restarted: add it.
                let mut data = self.data(tip);
                let _ = Shell_NotifyIconW(NIM_ADD, &data);
                data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
            }
            if !old.is_invalid() {
                let _ = DestroyIcon(old);
            }
        }
    }

    /// Where the menu opens: above (or beside) the icon, or at the pointer if the icon
    /// can't be found (hidden in the taskbar's overflow).
    pub fn anchor(&self) -> POINT {
        let id = NOTIFYICONIDENTIFIER {
            cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
            hWnd: self.window,
            uID: ICON_ID,
            ..Default::default()
        };
        // SAFETY: plain queries.
        unsafe {
            if let Ok(rect) = Shell_NotifyIconGetRect(&id) {
                return POINT {
                    x: (rect.left + rect.right) / 2,
                    y: (rect.top + rect.bottom) / 2,
                };
            }
            let mut point = POINT::default();
            let _ = GetCursorPos(&mut point);
            point
        }
    }

    pub fn remove(&mut self) {
        // SAFETY: removes our icon, then frees its image.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.data(""));
            if !self.icon.is_invalid() {
                let _ = DestroyIcon(self.icon);
            }
        }
        self.icon = HICON::default();
    }
}

/// The logo as an icon at the size of the notification area's icons, dark on a light
/// taskbar and white on a dark one, faded when no unit is connected.
fn draw_icon(window: HWND, glyph: Glyph, dimmed: bool) -> HICON {
    // SAFETY: GDI objects made here are deleted here; the icon is the caller's.
    unsafe {
        let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(window).max(96);
        let size = GetSystemMetricsForDpi(SM_CXSMICON, dpi).max(16);
        let rgb = if crate::settings::light_taskbar() {
            [0x1E, 0x1E, 0x1E]
        } else {
            [0xFF, 0xFF, 0xFF]
        };
        let pixels = logo::tray(size as u32, glyph, rgb, if dimmed { 0.45 } else { 1.0 });

        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let Ok(colour) = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
            return HICON::default();
        };
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u8, pixels.len());
        let mask_bytes = vec![0u8; ((size + 15) / 16 * 2 * size) as usize];
        let mask = CreateBitmap(size, size, 1, 1, Some(mask_bytes.as_ptr() as *const c_void));
        let icon = CreateIconIndirect(&ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: colour,
        })
        .unwrap_or_default();
        let _ = DeleteObject(colour.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

/// One line of the menu.
pub enum Item {
    /// A greyed line saying what's going on.
    Info(String),
    Separator,
    Command {
        id: u32,
        text: &'static str,
        enabled: bool,
        checked: bool,
    },
}

/// Show `items` as a menu at `at` and return the chosen command's id (0 for none).
pub fn show_menu(window: HWND, at: POINT, items: &[Item]) -> u32 {
    // SAFETY: a menu made, shown and destroyed here.
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return 0;
        };
        for item in items {
            append(menu, item);
        }
        // Without this, the menu wouldn't close when clicking elsewhere.
        let _ = SetForegroundWindow(window);
        // Open towards the middle of the screen, away from the taskbar.
        let mut flags = TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY;
        let monitor = MonitorFromPoint(at, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let screen = if GetMonitorInfoW(monitor, &mut info).as_bool() {
            info.rcMonitor
        } else {
            RECT::default()
        };
        flags |= if at.y > (screen.top + screen.bottom) / 2 {
            TPM_BOTTOMALIGN
        } else {
            TPM_TOPALIGN
        };
        flags |= if at.x > (screen.left + screen.right) / 2 {
            TPM_RIGHTALIGN
        } else {
            TPM_LEFTALIGN
        };
        let chosen = TrackPopupMenuEx(menu, flags.0, at.x, at.y, window, None);
        let _ = PostMessageW(Some(window), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        chosen.0 as u32
    }
}

unsafe fn append(menu: HMENU, item: &Item) {
    // SAFETY: the text lives until the call returns; the menu copies it.
    unsafe {
        match item {
            Item::Separator => {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            }
            Item::Info(text) => {
                // `&` would underline the next letter.
                let text = HSTRING::from(shorten(text).replace('&', "&&"));
                let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, &text);
            }
            Item::Command {
                id,
                text,
                enabled,
                checked,
            } => {
                let mut flags = MF_STRING;
                if !enabled {
                    flags |= MF_GRAYED;
                }
                if *checked {
                    flags |= MF_CHECKED;
                }
                let _ = AppendMenuW(menu, flags, *id as usize, &HSTRING::from(*text));
            }
        }
    }
}

/// A line no wider than a menu should be.
fn shorten(text: &str) -> String {
    const MAX: usize = 90;
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    let cut: String = text.chars().take(MAX - 1).collect();
    format!("{}…", cut.trim_end())
}

/// Open a folder, file or link the way Explorer would.
pub fn open(target: &str) {
    // SAFETY: strings live across the call.
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &HSTRING::from(target),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Follows links in a dialog's text.
unsafe extern "system" fn on_dialog_event(
    _window: HWND,
    event: TASKDIALOG_NOTIFICATIONS,
    _wparam: WPARAM,
    lparam: LPARAM,
    _data: isize,
) -> HRESULT {
    if event == TDN_HYPERLINK_CLICKED {
        // SAFETY: for this event, lparam is the link's NUL-terminated target.
        let target = unsafe { PCWSTR(lparam.0 as *const u16).to_string() };
        if let Ok(target) = target {
            open(&target);
        }
    }
    S_OK
}

/// What a dialog shows.
pub struct Dialog<'a> {
    pub title: &'a str,
    pub text: &'a str,
    /// The app's icon (the logo) instead of the warning sign.
    pub app_icon: bool,
    /// Buttons as (id, label), the default first; none for a plain OK.
    pub buttons: &'a [(i32, &'a str)],
    /// A checkbox under the text, ticked to start with.
    pub checkbox: Option<&'a str>,
    /// Plain information rather than a warning.
    pub information: bool,
}

/// Show a dialog; the button chosen (`IDOK` = 1 for a plain OK, `IDCANCEL` = 2 for
/// Escape) and whether the checkbox was left ticked.
pub fn dialog(instance: HINSTANCE, d: &Dialog) -> (i32, bool) {
    let window_title = HSTRING::from("SP-404 Drive");
    let title = HSTRING::from(d.title);
    let text = HSTRING::from(d.text);
    let checkbox = d.checkbox.map(HSTRING::from);
    let labels: Vec<HSTRING> = d.buttons.iter().map(|(_, l)| HSTRING::from(*l)).collect();
    let buttons: Vec<TASKDIALOG_BUTTON> = d
        .buttons
        .iter()
        .zip(&labels)
        .map(|((id, _), label)| TASKDIALOG_BUTTON {
            nButtonID: *id,
            pszButtonText: PCWSTR(label.as_ptr()),
        })
        .collect();
    let mut config = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        hInstance: instance,
        dwFlags: TDF_ENABLE_HYPERLINKS | TDF_ALLOW_DIALOG_CANCELLATION,
        pszWindowTitle: PCWSTR(window_title.as_ptr()),
        pszMainInstruction: PCWSTR(title.as_ptr()),
        pszContent: PCWSTR(text.as_ptr()),
        pfCallback: Some(on_dialog_event),
        ..Default::default()
    };
    // TASKDIALOGCONFIG is packed: set the union whole.
    let icon = if d.app_icon {
        // The icon resource, id 1 (build.rs).
        PCWSTR(std::ptr::without_provenance(1))
    } else if d.information {
        TD_INFORMATION_ICON
    } else {
        TD_WARNING_ICON
    };
    config.Anonymous1 = TASKDIALOGCONFIG_0 { pszMainIcon: icon };
    if buttons.is_empty() {
        config.dwCommonButtons = TDCBF_OK_BUTTON;
    } else {
        config.pButtons = buttons.as_ptr();
        config.cButtons = buttons.len() as u32;
        config.nDefaultButton = buttons[0].nButtonID;
    }
    if let Some(checkbox) = &checkbox {
        config.pszVerificationText = PCWSTR(checkbox.as_ptr());
        let flags = config.dwFlags;
        config.dwFlags = flags | TDF_VERIFICATION_FLAG_CHECKED;
    }
    let mut button = 0;
    let mut ticked = BOOL(0);
    // SAFETY: every string and the buttons outlive the call.
    let shown = unsafe { TaskDialogIndirect(&config, Some(&mut button), None, Some(&mut ticked)) };
    if shown.is_err() {
        return (2, false);
    }
    (button, ticked.as_bool())
}
