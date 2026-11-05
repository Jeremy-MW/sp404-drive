//! The background process that serves the drive.
//!
//! `sp404drive mount` has Explorer start `sp404drive-host.exe` (see [`start`]), and
//! returns once the host reports through a named pipe that the drive is mounted, or why
//! it is not. Started by Explorer, the host is outside whatever started `sp404drive`: no
//! terminal, nor the Windows job a terminal may keep its programs in, can end it. The
//! host is built as a Windows GUI program that never creates a window, so it never has
//! a window or a console. It logs to a file, and runs until `sp404drive unmount` or until
//! the device goes away.
//!
//! The two processes coordinate through named kernel objects, one set per mount point:
//!
//! - an event the host waits on, set by `sp404drive unmount`;
//! - a mutex the host owns while it runs. It is released (or, if the host dies,
//!   abandoned) when the host ends, which wakes `sp404drive unmount` waiting for the
//!   drive to be gone.

use std::ffi::c_void;
use std::io::{Read, Write};
use std::os::windows::io::FromRawHandle;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, WAIT_ABANDONED,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND;
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IDispatch,
    IServiceProvider,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, MUTEX_MODIFY_STATE, OpenEventW,
    OpenMutexW, ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE, SetEvent, WaitForSingleObject,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    CSIDL_DESKTOP, IShellBrowser, IShellDispatch2, IShellFolderViewDual, IShellWindows,
    SID_STopLevelBrowser, SVGIO_BACKGROUND, SWC_DESKTOP, SWFO_NEEDDISPATCH, ShellWindows,
};
use windows::core::{BSTR, HSTRING, Interface};

use crate::link::Link;
use crate::link::windows::{SerialPort, find_ports};
use crate::store::DeviceStore;
use crate::vfs::{Drive, Options};
use crate::winfs;

/// File name of the host program, installed next to `sp404drive.exe`.
pub const HOST_EXE: &str = "sp404drive-host.exe";

/// What the host writes to its report pipe: this line once mounted, or `error: <reason>`.
pub const REPORT_MOUNTED: &str = "mounted";
pub const REPORT_ERROR: &str = "error: ";

pub struct MountArgs {
    pub mount_point: String,
    pub port: Option<String>,
    pub read_only: bool,
    pub cache_secs: u64,
}

/// A mount point as it appears in object and file names: `S:` -> `S`.
fn tag(mount_point: &str) -> String {
    mount_point
        .trim_end_matches(['\\', '/', ':'])
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn unmount_event_name(mount_point: &str) -> HSTRING {
    HSTRING::from(format!("Local\\sp404drive-unmount-{}", tag(mount_point)))
}

fn running_mutex_name(mount_point: &str) -> HSTRING {
    HSTRING::from(format!("Local\\sp404drive-running-{}", tag(mount_point)))
}

/// Where the host serving `mount_point` logs: `%LOCALAPPDATA%\sp404drive\logs\S.log`.
pub fn log_path(mount_point: &str) -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("sp404drive")
        .join("logs")
        .join(format!("{}.log", tag(mount_point)))
}

/// Serial ports with an SP-404MKII, one line each.
pub fn list_ports() -> anyhow::Result<Vec<String>> {
    Ok(find_ports()?
        .into_iter()
        .map(|p| format!("{}  {}", p.name, p.instance_id))
        .collect())
}

/// Serve the drive on the mount point `running` claims. Runs in the host. `report` is
/// called exactly once: `Ok` when the drive is mounted, or `Err` with the reason it could
/// not be.
pub fn serve(
    args: &MountArgs,
    running: Running,
    report: impl FnOnce(Result<(), String>),
) -> anyhow::Result<()> {
    let mut report = Some(report);
    let result = {
        let mut on_mounted = || {
            if let Some(report) = report.take() {
                report(Ok(()));
            }
        };
        serve_inner(args, &mut on_mounted)
    };
    if let Err(e) = &result
        && let Some(report) = report.take()
    {
        report(Err(format!("{e:#}")));
    }
    // Only now, with the drive gone, may `sp404drive unmount` return.
    drop(running);
    result
}

fn serve_inner(args: &MountArgs, on_mounted: &mut dyn FnMut()) -> anyhow::Result<()> {
    let mount_point = args.mount_point.as_str();
    let (stop_tx, stop_rx) = mpsc::channel::<String>();
    {
        let tx = stop_tx.clone();
        listen_for_unmount(mount_point, move || {
            let _ = tx.send("unmount requested".into());
        })?;
    }
    // Unplugging the device, or switching it off, fails the link's pending read at once.
    let dev = open_link(args.port.as_deref(), move || {
        let _ = stop_tx.send("the SP-404MKII was disconnected".into());
    })?;
    log::info!("connected to the SP-404MKII on {}", dev.port_name());
    let drive = Arc::new(Drive::new(
        Arc::new(DeviceStore::new(dev)),
        Options {
            read_only: args.read_only,
            cache_ttl: Duration::from_secs(args.cache_secs),
        },
    ));
    winfs::run(drive, mount_point, on_mounted, stop_rx)
}

fn open_link(
    port: Option<&str>,
    on_closed: impl FnOnce() + Send + 'static,
) -> anyhow::Result<Link> {
    let name = match port {
        Some(name) => name.to_string(),
        None => find_ports()?
            .into_iter()
            .next()
            .map(|p| p.name)
            .context("no SP-404MKII found (USB 0582:02E7)")?,
    };
    let serial = SerialPort::open(&name)
        .with_context(|| format!("opening {name} (is Roland's SP-404MKII app connected?)"))?;
    Ok(Link::new(&name, Arc::new(serial), on_closed)?)
}

/// The mutex that marks a mount point as served, owned for the life of the host. Claim
/// it on the host's main thread (a mutex belongs to the thread that owns it) before
/// touching anything shared with another host, such as the log.
pub struct Running(HANDLE);

impl Running {
    pub fn claim(mount_point: &str) -> anyhow::Result<Self> {
        // SAFETY: plain named-mutex creation, owned by this (the main) thread.
        unsafe {
            let mutex = CreateMutexW(None, true, &running_mutex_name(mount_point))?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(mutex);
                bail!("{mount_point} is already served by sp404drive");
            }
            Ok(Running(mutex))
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // SAFETY: releasing and closing the mutex this thread owns.
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

/// Wait in the background for `sp404drive unmount` aimed at `mount_point`, then call
/// `on_signal`.
fn listen_for_unmount(
    mount_point: &str,
    on_signal: impl FnOnce() + Send + 'static,
) -> anyhow::Result<()> {
    // SAFETY: plain named-event creation; the handle is closed after the wait.
    let event = unsafe { CreateEventW(None, true, false, &unmount_event_name(mount_point))? };
    let raw = event.0 as usize;
    std::thread::Builder::new()
        .name("unmount-listener".into())
        .spawn(move || {
            let event = HANDLE(raw as *mut c_void);
            // SAFETY: the handle stays open for the life of this thread.
            unsafe {
                WaitForSingleObject(event, INFINITE);
                let _ = CloseHandle(event);
            }
            on_signal();
        })?;
    Ok(())
}

/// Ask the host serving `mount_point` to unmount, and wait up to `wait` for it to finish.
/// Returns whether it finished in time.
pub fn unmount(mount_point: &str, wait: Duration) -> anyhow::Result<bool> {
    // SAFETY: open, signal and wait on named kernel objects; every handle is closed.
    unsafe {
        let running = OpenMutexW(
            SYNCHRONIZATION_SYNCHRONIZE | MUTEX_MODIFY_STATE,
            false,
            &running_mutex_name(mount_point),
        )
        .map_err(|_| anyhow!("nothing is mounted at {mount_point} by sp404drive"))?;
        let signalled = OpenEventW(EVENT_MODIFY_STATE, false, &unmount_event_name(mount_point))
            .and_then(|event| {
                let set = SetEvent(event);
                let _ = CloseHandle(event);
                set
            });
        if let Err(e) = signalled {
            let _ = CloseHandle(running);
            return Err(anyhow!("asking {mount_point} to unmount: {e}"));
        }
        let waited = WaitForSingleObject(running, wait.as_millis() as u32);
        let done = match waited {
            WAIT_OBJECT_0 | WAIT_ABANDONED => {
                let _ = ReleaseMutex(running);
                true
            }
            WAIT_TIMEOUT => false,
            other => {
                let _ = CloseHandle(running);
                return Err(anyhow!("waiting for {mount_point} to unmount: {other:?}"));
            }
        };
        let _ = CloseHandle(running);
        Ok(done)
    }
}

/// Tell `sp404drive mount`, through the pipe it named, how mounting went. Does nothing
/// without a pipe (the host started some other way).
pub fn report_to_pipe(pipe: Option<&str>, status: Result<(), String>) {
    let Some(pipe) = pipe else {
        return;
    };
    let line = match status {
        Ok(()) => REPORT_MOUNTED.to_string(),
        Err(e) => format!("{REPORT_ERROR}{}", e.replace(['\r', '\n'], " ")),
    };
    match std::fs::OpenOptions::new().write(true).open(pipe) {
        Ok(mut out) => {
            let _ = writeln!(out, "{line}");
        }
        Err(e) => log::warn!("could not report to {pipe}: {e}"),
    }
}

/// Start the host with `args` and wait up to `deadline` for its report: `Ok(Ok(()))` once
/// mounted, `Ok(Err(reason))` if it could not mount.
pub fn start(
    host_exe: &Path,
    args: &[String],
    deadline: Duration,
) -> anyhow::Result<Result<(), String>> {
    let pipe = format!(r"\\.\pipe\sp404drive-report-{}", std::process::id());
    // SAFETY: a plain inbound named pipe; ownership passes to the reader thread.
    let server = unsafe {
        CreateNamedPipeW(
            &HSTRING::from(pipe.as_str()),
            PIPE_ACCESS_INBOUND,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            0,
            4096,
            0,
            None,
        )
    };
    if server.is_invalid() {
        bail!("creating {pipe}: {}", windows::core::Error::from_win32());
    }
    let raw = server.0 as usize;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("host-report".into())
        .spawn(move || {
            let server = HANDLE(raw as *mut c_void);
            // SAFETY: waits for the host to connect, then reads until it closes its end;
            // the File takes ownership of the handle.
            let connected = unsafe { ConnectNamedPipe(server, None) };
            let result = match connected {
                Err(e) if e.code() != ERROR_PIPE_CONNECTED.to_hresult() => Err(e.to_string()),
                _ => {
                    let mut file = unsafe { std::fs::File::from_raw_handle(server.0) };
                    let mut report = String::new();
                    file.read_to_string(&mut report)
                        .map(|_| report)
                        .map_err(|e| e.to_string())
                }
            };
            let _ = tx.send(result);
        })?;

    let mut args = args.to_vec();
    args.extend(["--report-pipe".to_string(), pipe]);
    shell_execute(host_exe, &args)?;

    // A deadline on one wait, not a check loop: the reader thread wakes this at once.
    let report = match rx.recv_timeout(deadline) {
        Ok(Ok(report)) => report,
        Ok(Err(e)) => bail!("reading the drive's report: {e}"),
        Err(_) => bail!(
            "the drive's background process did not report within {} s",
            deadline.as_secs()
        ),
    };
    Ok(match report.lines().next() {
        Some(REPORT_MOUNTED) => Ok(()),
        Some(line) if line.starts_with(REPORT_ERROR) => Err(line[REPORT_ERROR.len()..].to_string()),
        _ => Err("the drive's background process stopped without reporting".to_string()),
    })
}

/// Have Explorer run `exe` with `args`, hidden, as if from the Run box. The process is
/// then Explorer's child, independent of this one, its terminal and any job it is in.
/// The route through the desktop's shell view to `IShellDispatch2::ShellExecute` is the
/// documented way to do this.
fn shell_execute(exe: &Path, args: &[String]) -> anyhow::Result<()> {
    let command_line = args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
    let dir = exe.parent().unwrap_or(Path::new("."));
    // SAFETY: COM calls on interfaces obtained here; each is released when dropped.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER)
            .context("reaching Explorer")?;
        let desktop = VARIANT::from(CSIDL_DESKTOP as i32);
        let mut hwnd = 0i32;
        let found: IDispatch = windows
            .FindWindowSW(
                &desktop,
                &VARIANT::default(),
                SWC_DESKTOP,
                &mut hwnd,
                SWFO_NEEDDISPATCH,
            )
            .context("Explorer's desktop isn't running; sp404drive needs it to start the drive")?;
        let browser: IShellBrowser = found
            .cast::<IServiceProvider>()
            .context("Explorer's desktop: service provider")?
            .QueryService(&SID_STopLevelBrowser)
            .context("Explorer's desktop: top-level browser")?;
        let view = browser
            .QueryActiveShellView()
            .context("Explorer's desktop: shell view")?;
        let background: IDispatch = view
            .GetItemObject(SVGIO_BACKGROUND)
            .context("Explorer's desktop: view background")?;
        let folder_view: IShellFolderViewDual = background
            .cast()
            .context("Explorer's desktop: folder view")?;
        let shell: IShellDispatch2 = folder_view
            .Application()
            .context("Explorer's desktop: shell application")?
            .cast()
            .context("Explorer's desktop: shell dispatch")?;
        shell
            .ShellExecute(
                &BSTR::from(exe.to_string_lossy().as_ref()),
                &VARIANT::from(command_line.as_str()),
                &VARIANT::from(dir.to_string_lossy().as_ref()),
                &VARIANT::from(""),
                &VARIANT::from(0i32), // SW_HIDE; the host has no window anyway
            )
            .context("asking Explorer to start the drive")?;
    }
    Ok(())
}

/// Quote one argument for a Windows command line, by the rules `CommandLineToArgvW` and
/// the Rust runtime parse with.
fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_arguments() {
        assert_eq!(quote("S:"), "S:");
        assert_eq!(quote(r"C:\My Drives\SP404"), r#""C:\My Drives\SP404""#);
        assert_eq!(quote(r"C:\a b\"), r#""C:\a b\\""#);
        assert_eq!(quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote(""), r#""""#);
    }

    #[test]
    fn tags_mount_points() {
        assert_eq!(tag("S:"), "S");
        assert_eq!(tag(r"C:\SP404\"), "C__SP404");
    }
}
