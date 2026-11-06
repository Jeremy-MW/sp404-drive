// A GUI-subsystem program: no console, whoever starts it. Its one window is hidden; what
// the user sees is the notification-area icon and its menu.
#![windows_subsystem = "windows"]

//! SP-404 Drive's notification-area ("tray") app on Windows, the counterpart of the Mac's
//! menu-bar app (`macos/App/AppDelegate.swift`), with the same menu.
//!
//! - Switched on or plugged in: mounts the unit on S:, if "Mount Automatically" is on.
//!   Windows says when the unit's serial port appears (`WM_DEVICECHANGE`); nothing polls.
//! - Switched off or unplugged: the drive unmounts itself (its serial read fails at
//!   once), and the app hears of it when the drive's process ends.
//! - The menu: Mount, Mount Read-Only, Eject, Open in File Explorer, and the settings.
//! - Quit ejects first, as on the Mac.
//!
//! It mounts and ejects with the same code as `sp404drive mount` and `unmount`
//! (`sp404_drive::host`), so it gets the same checks and messages. The installer also
//! runs it with `--quit` (eject and quit the running copy) and `--add-to-path` /
//! `--remove-from-path`.

#[cfg(windows)]
mod logo;
#[cfg(windows)]
mod settings;
#[cfg(windows)]
mod ui;

#[cfg(windows)]
fn main() {
    app::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("sp404drive-tray is for Windows; on a Mac, SP-404 Drive.app is the menu-bar app");
    std::process::exit(1);
}

#[cfg(windows)]
mod app {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::time::Duration;

    use sp404_drive::host::{self, HOST_EXE, Served, TRAY_MOUNTED_MESSAGE, TRAY_WINDOW_CLASS};
    use sp404_drive::link::windows::find_ports;
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
    };
    use windows::Win32::System::Ioctl::GUID_DEVINTERFACE_COMPORT;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::{
        CreateMutexW, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    use windows::Win32::UI::Shell::NIN_SELECT;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DBT_DEVICEARRIVAL, DBT_DEVICEREMOVECOMPLETE, DBT_DEVTYP_DEVICEINTERFACE,
        DEV_BROADCAST_DEVICEINTERFACE_W, DEV_BROADCAST_HDR, DEVICE_NOTIFY_WINDOW_HANDLE,
        DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowW, GetMessageW,
        GetWindowThreadProcessId, MSG, PostMessageW, PostQuitMessage, RegisterClassExW,
        RegisterDeviceNotificationW, RegisterWindowMessageW, WINDOW_EX_STYLE, WM_APP,
        WM_CONTEXTMENU, WM_DESTROY, WM_DEVICECHANGE, WM_DISPLAYCHANGE, WM_DPICHANGED,
        WM_ENDSESSION, WM_SETTINGCHANGE, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
    };
    use windows::core::{HSTRING, PCWSTR, w};

    use crate::logo::Glyph;
    use crate::settings;
    use crate::ui::{self, Dialog, Item, TrayIcon};

    /// Where the app mounts the unit, as `sp404drive mount` does by default.
    const MOUNT_POINT: &str = "S:";
    /// How long a mount waits for the drive to report, as `sp404drive mount`.
    const MOUNT_WAIT: Duration = Duration::from_secs(30);
    /// How long an eject waits for files still being saved, as `sp404drive unmount`.
    const UNMOUNT_WAIT: Duration = Duration::from_secs(60);
    /// How long `--quit` waits for the running copy to eject and quit.
    const QUIT_WAIT: Duration = Duration::from_secs(90);

    /// The notification icon's messages.
    const WM_TRAY: u32 = WM_APP + 2;
    /// A mount or eject finished on its thread; the result is in the channel.
    const WM_DONE: u32 = WM_APP + 3;
    /// The drive's process ended.
    const WM_DRIVE_GONE: u32 = WM_APP + 4;
    /// Another copy was started: show the menu.
    const WM_SHOW_MENU: u32 = WM_APP + 5;
    /// The installer (`--quit`): eject and quit without asking.
    const WM_QUIT_NOW: u32 = WM_APP + 6;
    /// An SP-404MKII's port appeared or went: look at the ports again.
    const WM_RESCAN: u32 = WM_APP + 7;
    /// The taskbar changed (colour, size, or Explorer restarted): redraw the icon.
    const WM_REDRAW: u32 = WM_APP + 8;

    // Menu commands.
    const CMD_MOUNT: u32 = 1;
    const CMD_MOUNT_READ_ONLY: u32 = 2;
    const CMD_EJECT: u32 = 3;
    const CMD_OPEN: u32 = 4;
    const CMD_AUTO_MOUNT: u32 = 5;
    const CMD_SIGN_IN: u32 = 6;
    const CMD_ABOUT: u32 = 7;
    const CMD_QUIT: u32 = 8;

    // Dialog buttons.
    const BUTTON_DONT_QUIT: i32 = 100;
    const BUTTON_QUIT_ANYWAY: i32 = 101;
    const BUTTON_TASKBAR_SETTINGS: i32 = 102;

    /// A mount or eject's result, sent back from its thread.
    enum Done {
        Mounted {
            automatic: bool,
            result: Result<(), String>,
        },
        Ejected {
            result: Result<(), String>,
            quit: Quit,
        },
    }

    /// What to do once an eject is done.
    #[derive(Clone, Copy, PartialEq)]
    enum Quit {
        No,
        /// Quit, or ask if the eject failed (the menu's Eject and Quit).
        Ask,
        /// Quit whatever happened (the installer).
        Always,
    }

    /// Things to do with the app's state not borrowed: each can run a message loop (a
    /// dialog, Explorer, the shell), which can call back into the window procedure.
    enum Effect {
        UpdateIcon,
        Alert(String, String),
        AskQuitAnyway(String),
        Open(String),
        Welcome,
        About,
        Exit,
    }

    struct App {
        window: HWND,
        instance: HINSTANCE,
        icon: TrayIcon,
        /// The units connected now: their serial ports (`COM4`).
        ports: Vec<String>,
        /// The drive served on S:, as of the last look; while true, a thread waits for
        /// the drive's process to end.
        mounted: bool,
        read_only: bool,
        /// What the app is doing now ("Mounting…"), shown in the menu; `None` when idle.
        busy: Option<&'static str>,
        /// The last automatic mount's failure, shown in the menu until the next change.
        last_problem: Option<String>,
        /// A unit that was switched on while the app was busy, or while its last drive
        /// was still going: mounted when that's done.
        waiting_port: Option<String>,
        /// A dialog is open: the menu waits until it's closed.
        modal: bool,
        done_tx: Sender<Done>,
        done_rx: Receiver<Done>,
        effects: VecDeque<Effect>,
        /// Explorer's message when the taskbar is recreated (after a restart of Explorer).
        taskbar_created: u32,
    }

    thread_local! {
        static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    }

    /// Run `f` on the app's state, if it exists and isn't already borrowed (a callback
    /// from inside an Effect), then carry out the effects it asked for.
    fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
        let result = APP.with(|app| {
            let mut app = app.try_borrow_mut().ok()?;
            Some(f(app.as_mut()?))
        });
        run_effects();
        result
    }

    fn run_effects() {
        loop {
            let next = APP.with(|app| {
                let mut app = app.try_borrow_mut().ok()?;
                let app = app.as_mut()?;
                let effect = app.effects.pop_front()?;
                Some((effect, app.window, app.instance))
            });
            let Some((effect, window, instance)) = next else {
                return;
            };
            match effect {
                Effect::UpdateIcon => update_icon(),
                Effect::Alert(title, text) => modal(|| {
                    ui::dialog(
                        instance,
                        &Dialog {
                            title: &title,
                            text: &text,
                            app_icon: false,
                            buttons: &[],
                            checkbox: None,
                            information: false,
                        },
                    );
                }),
                Effect::AskQuitAnyway(problem) => {
                    let text = format!(
                        "{problem}\n\nIf you quit anyway, the drive stays mounted until the unit is \
                         switched off or `sp404drive unmount` is run."
                    );
                    let (button, _) = modal(|| {
                        ui::dialog(
                            instance,
                            &Dialog {
                                title: "Couldn't eject the SP-404MKII",
                                text: &text,
                                app_icon: false,
                                buttons: &[
                                    (BUTTON_DONT_QUIT, "Don't Quit"),
                                    (BUTTON_QUIT_ANYWAY, "Quit Anyway"),
                                ],
                                checkbox: None,
                                information: false,
                            },
                        )
                    });
                    if button == BUTTON_QUIT_ANYWAY {
                        exit(window);
                    }
                }
                Effect::Open(target) => ui::open(&target),
                Effect::Welcome => welcome(instance),
                Effect::About => about(instance),
                Effect::Exit => exit(window),
            }
        }
    }

    /// Run a dialog with the menu held off.
    fn modal<R>(f: impl FnOnce() -> R) -> R {
        APP.with(|app| {
            if let Ok(mut app) = app.try_borrow_mut()
                && let Some(app) = app.as_mut()
            {
                app.modal = true;
            }
        });
        let result = f();
        APP.with(|app| {
            if let Ok(mut app) = app.try_borrow_mut()
                && let Some(app) = app.as_mut()
            {
                app.modal = false;
            }
        });
        result
    }

    fn exit(window: HWND) {
        APP.with(|app| {
            if let Ok(mut app) = app.try_borrow_mut()
                && let Some(app) = app.as_mut()
            {
                app.icon.remove();
            }
        });
        // SAFETY: our own window; WM_DESTROY ends the message loop.
        unsafe {
            let _ = DestroyWindow(window);
        }
    }

    fn post(window: isize, message: u32) {
        // SAFETY: a message with no pointers to our own window.
        unsafe {
            let _ = PostMessageW(Some(HWND(window as *mut _)), message, WPARAM(0), LPARAM(0));
        }
    }

    impl App {
        fn exe_dir() -> PathBuf {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(PathBuf::from))
                .unwrap_or_default()
        }

        /// Look again at the drive and the units, and redraw.
        fn refresh(&mut self) {
            if !self.mounted
                && let Some(served) = Served::open(MOUNT_POINT)
            {
                self.mounted = true;
                self.read_only = served.read_only;
                let window = self.window.0 as isize;
                let spawned = std::thread::Builder::new()
                    .name("drive-watch".into())
                    .spawn(move || {
                        served.wait_until_gone();
                        post(window, WM_DRIVE_GONE);
                    });
                if spawned.is_err() {
                    self.mounted = false;
                }
            }
            self.effects.push_back(Effect::UpdateIcon);
        }

        /// Read the units connected now; the ports that appeared and went since last time.
        fn rescan(&mut self) -> (Vec<String>, Vec<String>) {
            let now: Vec<String> = find_ports()
                .map(|ports| ports.into_iter().map(|p| p.name).collect())
                .unwrap_or_default();
            let appeared = now
                .iter()
                .filter(|p| !self.ports.contains(p))
                .cloned()
                .collect();
            let gone = self
                .ports
                .iter()
                .filter(|p| !now.contains(p))
                .cloned()
                .collect();
            self.ports = now;
            (appeared, gone)
        }

        fn unit_appeared(&mut self, port: String) {
            self.last_problem = None;
            self.refresh();
            if !settings::auto_mount() {
                return;
            }
            self.waiting_port = Some(port);
            self.mount_if_waiting();
        }

        fn unit_disappeared(&mut self, port: &str) {
            if self.waiting_port.as_deref() == Some(port) {
                self.waiting_port = None;
            }
            // The drive unmounts itself; the watch thread says when it's gone.
            self.refresh();
        }

        /// Mount the unit that was switched on, unless the app is busy or a drive is still
        /// served (then this runs again when that's done).
        fn mount_if_waiting(&mut self) {
            let Some(port) = self.waiting_port.clone() else {
                return;
            };
            if !settings::auto_mount()
                || self.busy.is_some()
                || self.mounted
                || !self.ports.contains(&port)
            {
                return;
            }
            self.mount(Some(port), false, true);
        }

        fn mount(&mut self, port: Option<String>, read_only: bool, automatic: bool) {
            if self.busy.is_some() {
                return;
            }
            self.waiting_port = None;
            self.busy = Some("Mounting…");
            self.refresh();
            let mut args = vec![
                MOUNT_POINT.to_string(),
                "--cache-secs".into(),
                "5".into(),
                "--log-level".into(),
                "info".into(),
            ];
            if read_only {
                args.push("--read-only".into());
            }
            if let Some(port) = port {
                args.extend(["--port".into(), port]);
            }
            let host_exe = Self::exe_dir().join(HOST_EXE);
            self.work(move || {
                let result = if host_exe.exists() {
                    match host::start(&host_exe, &args, MOUNT_WAIT) {
                        Ok(reported) => reported,
                        Err(e) => Err(format!("{e:#}")),
                    }
                } else {
                    Err(format!("{} is missing", host_exe.display()))
                };
                Done::Mounted { automatic, result }
            });
        }

        fn eject(&mut self, quit: Quit) {
            if self.busy.is_some() {
                return;
            }
            if !self.mounted {
                if quit != Quit::No {
                    self.effects.push_back(Effect::Exit);
                }
                return;
            }
            self.busy = Some("Ejecting…");
            self.refresh();
            self.work(move || {
                let result = match host::unmount(MOUNT_POINT, UNMOUNT_WAIT) {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(format!(
                        "{MOUNT_POINT} is still unmounting after {} s (saving files?). Don't \
                         switch the unit off or start Roland's app yet.",
                        UNMOUNT_WAIT.as_secs()
                    )),
                    Err(e) => Err(format!("{e:#}")),
                };
                Done::Ejected { result, quit }
            });
        }

        /// Run `job` on a thread of its own; its result comes back through `WM_DONE`.
        fn work(&mut self, job: impl FnOnce() -> Done + Send + 'static) {
            let tx = self.done_tx.clone();
            let window = self.window.0 as isize;
            let spawned = std::thread::Builder::new()
                .name("drive-job".into())
                .spawn(move || {
                    let _ = tx.send(job());
                    post(window, WM_DONE);
                });
            if let Err(e) = spawned {
                self.busy = None;
                self.effects.push_back(Effect::Alert(
                    "SP-404 Drive".into(),
                    format!("Couldn't start: {e}"),
                ));
                self.refresh();
            }
        }

        fn finished(&mut self, done: Done) {
            self.busy = None;
            match done {
                Done::Mounted { automatic, result } => {
                    self.refresh();
                    match result {
                        Ok(()) => {
                            self.last_problem = None;
                            if !automatic {
                                self.effects
                                    .push_back(Effect::Open(format!("{MOUNT_POINT}\\")));
                            }
                        }
                        // No dialog for something nobody clicked; the menu says what happened.
                        Err(problem) if automatic => self.last_problem = Some(problem),
                        Err(problem) => self.effects.push_back(Effect::Alert(
                            "Couldn't mount the SP-404MKII".into(),
                            problem,
                        )),
                    }
                }
                Done::Ejected { result, quit } => {
                    self.refresh();
                    match (result, quit) {
                        (Ok(()), Quit::No) => {}
                        (Ok(()), _) | (Err(_), Quit::Always) => {
                            self.effects.push_back(Effect::Exit);
                            return;
                        }
                        (Err(problem), Quit::Ask) => {
                            self.effects.push_back(Effect::AskQuitAnyway(problem));
                        }
                        (Err(problem), Quit::No) => self.effects.push_back(Effect::Alert(
                            "Couldn't eject the SP-404MKII".into(),
                            problem,
                        )),
                    }
                }
            }
            self.refresh();
            self.mount_if_waiting();
        }

        fn state(&self) -> String {
            if let Some(busy) = self.busy {
                return busy.to_string();
            }
            if self.mounted {
                return if self.read_only {
                    format!("SP-404MKII: mounted on {MOUNT_POINT}, read-only")
                } else {
                    format!("SP-404MKII: mounted on {MOUNT_POINT}")
                };
            }
            if self.ports.is_empty() {
                "SP-404MKII: not connected".into()
            } else {
                "SP-404MKII: connected, not mounted".into()
            }
        }

        fn menu(&self) -> Vec<Item> {
            let connected = !self.ports.is_empty();
            let mounted = self.mounted;
            let idle = self.busy.is_none();
            let command = |id, text, enabled| Item::Command {
                id,
                text,
                enabled,
                checked: false,
            };
            let mut items = vec![Item::Info(self.state())];
            if let Some(problem) = &self.last_problem
                && !mounted
                && idle
            {
                items.push(Item::Info(format!("Couldn't mount: {problem}")));
            }
            items.extend([
                Item::Separator,
                command(CMD_MOUNT, "Mount", connected && !mounted && idle),
                command(
                    CMD_MOUNT_READ_ONLY,
                    "Mount Read-Only",
                    connected && !mounted && idle,
                ),
                command(CMD_EJECT, "Eject", mounted && idle),
                command(CMD_OPEN, "Open in File Explorer", mounted),
                Item::Separator,
                Item::Command {
                    id: CMD_AUTO_MOUNT,
                    text: "Mount Automatically When Switched On",
                    enabled: true,
                    checked: settings::auto_mount(),
                },
                Item::Command {
                    id: CMD_SIGN_IN,
                    text: "Open at Sign-in",
                    enabled: true,
                    checked: settings::open_at_sign_in(),
                },
                Item::Separator,
                command(CMD_ABOUT, "About SP-404MkII Drive", true),
                command(
                    CMD_QUIT,
                    if mounted {
                        "Eject and Quit"
                    } else {
                        "Quit SP-404 Drive"
                    },
                    idle,
                ),
            ]);
            items
        }

        fn command(&mut self, id: u32) {
            let first_port = self.ports.first().cloned();
            match id {
                CMD_MOUNT => self.mount(first_port, false, false),
                CMD_MOUNT_READ_ONLY => self.mount(first_port, true, false),
                CMD_EJECT => self.eject(Quit::No),
                CMD_OPEN => self
                    .effects
                    .push_back(Effect::Open(format!("{MOUNT_POINT}\\"))),
                CMD_AUTO_MOUNT => {
                    if let Err(e) = settings::set_auto_mount(!settings::auto_mount()) {
                        self.effects.push_back(Effect::Alert(
                            "Couldn't change Mount Automatically".into(),
                            e.message(),
                        ));
                    }
                    self.refresh();
                }
                CMD_SIGN_IN => {
                    let exe = std::env::current_exe().unwrap_or_default();
                    if let Err(e) =
                        settings::set_open_at_sign_in(&exe, !settings::open_at_sign_in())
                    {
                        self.effects.push_back(Effect::Alert(
                            "Couldn't change Open at Sign-in".into(),
                            e.message(),
                        ));
                    }
                }
                CMD_ABOUT => self.effects.push_back(Effect::About),
                CMD_QUIT => self.eject(Quit::Ask),
                _ => {}
            }
        }
    }

    /// Redraw the icon from the state now: a ring while nothing is mounted (faded when no
    /// unit is connected), a solid disc while the drive is mounted.
    fn update_icon() {
        APP.with(|app| {
            if let Ok(mut app) = app.try_borrow_mut()
                && let Some(app) = app.as_mut()
            {
                let glyph = if app.mounted {
                    Glyph::Disc
                } else {
                    Glyph::Ring
                };
                let dimmed = !app.mounted && app.ports.is_empty();
                let tip = app.state();
                app.icon.draw(glyph, dimmed, &tip);
            }
        });
    }

    fn show_menu() {
        let prepared = APP.with(|app| {
            let app = app.try_borrow().ok()?;
            let app = app.as_ref()?;
            (!app.modal).then(|| (app.window, app.icon.anchor(), app.menu()))
        });
        let Some((window, at, items)) = prepared else {
            return;
        };
        let chosen = ui::show_menu(window, at, &items);
        if chosen != 0 {
            with_app(|app| app.command(chosen));
        }
    }

    fn welcome(instance: HINSTANCE) {
        let text = "Switch the SP-404MKII on, or plug it in: it appears in File Explorer as \
                    drive S:. Quit Roland's SP-404MKII app first; only one program can use the \
                    unit at a time.\n\nWindows may tuck the icon away under the ^ on the taskbar. \
                    To keep it in view, open Taskbar Settings, then Other system tray icons, and \
                    switch on SP-404 Drive.";
        let (button, ticked) = modal(|| {
            ui::dialog(
                instance,
                &Dialog {
                    title: "SP-404 Drive is in the notification area",
                    text,
                    app_icon: true,
                    buttons: &[(1, "OK"), (BUTTON_TASKBAR_SETTINGS, "Taskbar Settings")],
                    // The drive is only mounted on switch-on while the app is running.
                    checkbox: Some("Open SP-404 Drive at sign-in"),
                    information: true,
                },
            )
        });
        if ticked && !settings::open_at_sign_in() {
            let exe = std::env::current_exe().unwrap_or_default();
            if let Err(e) = settings::set_open_at_sign_in(&exe, true) {
                with_app(|app| {
                    app.effects.push_back(Effect::Alert(
                        "Couldn't set SP-404 Drive to open at sign-in".into(),
                        e.message(),
                    ))
                });
            }
        }
        if button == BUTTON_TASKBAR_SETTINGS {
            ui::open("ms-settings:taskbar");
        }
    }

    /// The app's name and version, and a link to polecatplugins.com, as the Mac's About
    /// panel.
    fn about(instance: HINSTANCE) {
        let title = format!("SP-404MkII Drive {}", env!("CARGO_PKG_VERSION"));
        modal(|| {
            ui::dialog(
                instance,
                &Dialog {
                    title: &title,
                    text: "From <a href=\"https://polecatplugins.com\">polecatplugins.com</a>",
                    app_icon: true,
                    buttons: &[],
                    checkbox: None,
                    information: true,
                },
            )
        });
    }

    /// Whether a device-change message is about an SP-404MKII's serial port.
    ///
    /// # Safety
    /// `lparam` must be the message's `DEV_BROADCAST_HDR` pointer (or null).
    unsafe fn is_sp404_port(lparam: LPARAM) -> bool {
        let header = lparam.0 as *const DEV_BROADCAST_HDR;
        // SAFETY: as the caller promises; the name is NUL-terminated.
        unsafe {
            if header.is_null() || (*header).dbch_devicetype != DBT_DEVTYP_DEVICEINTERFACE {
                return false;
            }
            let interface = header as *const DEV_BROADCAST_DEVICEINTERFACE_W;
            let name = PCWSTR((*interface).dbcc_name.as_ptr())
                .to_string()
                .unwrap_or_default()
                .to_ascii_uppercase();
            name.contains(&format!(
                "VID_{:04X}&PID_{:04X}",
                sp404_drive::link::USB_VID,
                sp404_drive::link::USB_PID
            ))
        }
    }

    unsafe extern "system" fn window_proc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let taskbar_created = APP.with(|app| {
            app.try_borrow()
                .ok()
                .and_then(|a| a.as_ref().map(|a| a.taskbar_created))
        });
        match message {
            WM_TRAY => {
                // NOTIFYICON_VERSION_4: the event is in the low word.
                let event = (lparam.0 & 0xFFFF) as u32;
                if event == NIN_SELECT || event == WM_CONTEXTMENU {
                    show_menu();
                }
                return LRESULT(0);
            }
            WM_SHOW_MENU => {
                show_menu();
                return LRESULT(0);
            }
            WM_DONE => {
                with_app(|app| {
                    while let Ok(done) = app.done_rx.try_recv() {
                        app.finished(done);
                    }
                });
                return LRESULT(0);
            }
            WM_DRIVE_GONE => {
                with_app(|app| {
                    app.mounted = false;
                    app.read_only = false;
                    // A drive mounted again meanwhile (by hand) is picked up here.
                    app.refresh();
                    // Switched off and straight back on: mount it now the old drive's gone.
                    app.mount_if_waiting();
                });
                return LRESULT(0);
            }
            TRAY_MOUNTED_MESSAGE => {
                with_app(|app| app.refresh());
                return LRESULT(0);
            }
            WM_QUIT_NOW => {
                with_app(|app| app.eject(Quit::Always));
                return LRESULT(0);
            }
            // Sent messages can arrive while the app's state is in use (during a call to
            // the shell), so these only post a message to act on.
            WM_DEVICECHANGE => {
                let event = wparam.0 as u32;
                // SAFETY: lparam is this message's DEV_BROADCAST_HDR.
                if (event == DBT_DEVICEARRIVAL || event == DBT_DEVICEREMOVECOMPLETE)
                    && unsafe { is_sp404_port(lparam) }
                {
                    post(window.0 as isize, WM_RESCAN);
                }
                return LRESULT(1);
            }
            // The taskbar's colour or size may have changed.
            WM_SETTINGCHANGE | WM_DISPLAYCHANGE | WM_DPICHANGED => {
                post(window.0 as isize, WM_REDRAW);
            }
            WM_RESCAN => {
                with_app(|app| {
                    let (appeared, gone) = app.rescan();
                    for port in &gone {
                        app.unit_disappeared(port);
                    }
                    for port in appeared {
                        app.unit_appeared(port);
                    }
                    app.refresh();
                });
                return LRESULT(0);
            }
            WM_REDRAW => {
                update_icon();
                return LRESULT(0);
            }
            WM_ENDSESSION => {
                // Signing out or shutting down: Windows ends the drive's process too.
                if wparam.0 != 0 {
                    with_app(|app| app.icon.remove());
                }
                return LRESULT(0);
            }
            WM_DESTROY => {
                // SAFETY: ends this thread's message loop.
                unsafe { PostQuitMessage(0) };
                return LRESULT(0);
            }
            m if Some(m) == taskbar_created && m != 0 => {
                // Explorer restarted: put the icon back.
                post(window.0 as isize, WM_REDRAW);
                return LRESULT(0);
            }
            _ => {}
        }
        // SAFETY: the default handling.
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    }

    /// `--quit`: ask the running copy to eject and quit, and wait until it has.
    fn quit_running_copy() -> i32 {
        // SAFETY: finding the window, then waiting on its process with a deadline.
        unsafe {
            let Ok(window) = FindWindowW(&HSTRING::from(TRAY_WINDOW_CLASS), PCWSTR::null()) else {
                return 0;
            };
            let mut pid = 0;
            GetWindowThreadProcessId(window, Some(&mut pid));
            let process = OpenProcess(PROCESS_SYNCHRONIZE, false, pid);
            let _ = PostMessageW(Some(window), WM_QUIT_NOW, WPARAM(0), LPARAM(0));
            if let Ok(process) = process {
                WaitForSingleObject(process, QUIT_WAIT.as_millis() as u32);
                let _ = CloseHandle(process);
            }
        }
        0
    }

    fn path_change(add: bool) -> i32 {
        match settings::change_system_path(&App::exe_dir(), add) {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    pub fn main() {
        match std::env::args().nth(1).as_deref() {
            Some("--quit") => std::process::exit(quit_running_copy()),
            Some("--add-to-path") => std::process::exit(path_change(true)),
            Some("--remove-from-path") => std::process::exit(path_change(false)),
            Some("--write-icon") => {
                let path = std::env::args().nth(2).unwrap_or_else(|| "app.ico".into());
                let written = std::fs::write(path, crate::logo::app_ico());
                std::process::exit(written.is_err() as i32);
            }
            _ => {}
        }

        // SAFETY: Win32 set-up on this (the UI) thread; the mutex lives as long as the
        // process.
        unsafe {
            // One copy per user session. Started again (from the Start menu), show the
            // running copy's menu instead, as the Mac app does when opened again.
            let _single = CreateMutexW(None, false, w!("Local\\sp404drive-tray"));
            if GetLastError() == ERROR_ALREADY_EXISTS {
                if let Ok(window) = FindWindowW(&HSTRING::from(TRAY_WINDOW_CLASS), PCWSTR::null()) {
                    let _ = PostMessageW(Some(window), WM_SHOW_MENU, WPARAM(0), LPARAM(0));
                }
                return;
            }

            let instance: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
            let class = HSTRING::from(TRAY_WINDOW_CLASS);
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: PCWSTR(class.as_ptr()),
                ..Default::default()
            };
            RegisterClassExW(&wc);
            // A hidden top-level window rather than a message-only one: only top-level
            // windows hear Explorer's "taskbar created" broadcast.
            let Ok(window) = CreateWindowExW(
                WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0),
                &class,
                w!("SP-404 Drive"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                None,
            ) else {
                return;
            };

            // Serial ports appearing and going: an SP-404MKII switched on or off.
            let filter = DEV_BROADCAST_DEVICEINTERFACE_W {
                dbcc_size: size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32,
                dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE.0,
                dbcc_classguid: GUID_DEVINTERFACE_COMPORT,
                ..Default::default()
            };
            let _notify = RegisterDeviceNotificationW(
                window.into(),
                &filter as *const _ as *const _,
                DEVICE_NOTIFY_WINDOW_HANDLE,
            );

            let (done_tx, done_rx) = channel();
            let mut app = App {
                window,
                instance,
                icon: TrayIcon::new(window, WM_TRAY),
                ports: Vec::new(),
                mounted: false,
                read_only: false,
                busy: None,
                last_problem: None,
                waiting_port: None,
                modal: false,
                done_tx,
                done_rx,
                effects: VecDeque::new(),
                taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            };
            app.rescan();
            app.refresh();
            if settings::auto_mount()
                && !app.mounted
                && let Some(port) = app.ports.first()
            {
                // Opened (or started at sign-in) with the unit already on.
                app.waiting_port = Some(port.clone());
                app.mount_if_waiting();
            }
            if !settings::welcomed() {
                let _ = settings::set_welcomed();
                app.effects.push_back(Effect::Welcome);
            }
            APP.with(|cell| *cell.borrow_mut() = Some(app));
            run_effects();

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                DispatchMessageW(&msg);
            }
        }
    }
}
