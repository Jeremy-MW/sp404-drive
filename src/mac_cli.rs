//! `sp404drive mount` and `unmount` on macOS. The drive is an FSKit extension inside
//! `SP-404 Drive.app`, run by macOS in a process of its own, so there is no host process
//! here: `mount` runs `mount -F -t sp404 <url> <folder>` and returns once it's mounted,
//! and `unmount` runs `umount <folder>`, which returns once the extension has saved
//! everything, waited out the unit's session and let go of the port.
//!
//! The settings travel in the resource URL's query (`port`, `cache-secs`, `read-only`),
//! because the extension opens the drive in `loadResource`, before `-o` options arrive.
//! `-r` is passed as well, so the kernel mounts read-only.

use std::ffi::{CStr, CString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};

/// How long `mount` waits for `mount -F` (normally under a second).
const MOUNT_WAIT: Duration = Duration::from_secs(30);
/// How long `unmount` waits for `umount`: saving files still open, then up to 3 s for
/// the unit's session.
const UNMOUNT_WAIT: Duration = Duration::from_secs(60);
/// The FSKit file system's short name (`FSShortName` in the extension's Info.plist).
const FS_TYPE: &str = "sp404";
const EXTENSION_ID: &str = "com.polecatplugins.sp404drive.fsmodule";
const LOG_HINT: &str =
    "log show --last 5m --predicate 'subsystem == \"com.polecatplugins.sp404drive\"'";
const ENABLE_HINT: &str = "System Settings > General > Login Items & Extensions > \
     Extensions: By Category > File System Extensions, and switch on SP-404MKII";

pub fn list_ports() -> anyhow::Result<Vec<String>> {
    Ok(sp404_drive::link::find_ports()?
        .into_iter()
        .map(|p| format!("{}  {}", p.name, p.instance_id))
        .collect())
}

pub fn mount(
    mount_point: &str,
    port: Option<String>,
    read_only: bool,
    cache_secs: u64,
    _log_level: &str,
) -> anyhow::Result<()> {
    check_macos_version()?;
    let cache_secs = u32::try_from(cache_secs).context("--cache-secs is too large")?;
    let folder = folder_path(mount_point)?;
    let port = choose_port(port)?;
    let default = folder_path(crate::DEFAULT_MOUNT)?;

    for m in sp404_mounts()? {
        if m.on == folder {
            bail!("the SP-404MKII is already mounted at {}", folder.display());
        }
        // A mount made by hand without a port took the first unit found.
        if m.port().is_none_or(|p| p == port) {
            let unmount = if m.on == default {
                "sp404drive unmount".to_string()
            } else {
                format!("sp404drive unmount {}", m.on.display())
            };
            bail!(
                "the SP-404MKII on {port} is already mounted at {} (`{unmount}` first)",
                m.on.display()
            );
        }
    }

    if stale_records(&folder).is_ok_and(|found| !found.is_empty()) {
        return Err(StaleMount { folder }.into());
    }

    if folder == default
        && let Some(parent) = folder.parent()
    {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let created = prepare_folder(&folder)?;
    let url = resource_url(&port, cache_secs, read_only);
    let started = local_time_now();
    let mut cmd = Command::new("/sbin/mount");
    cmd.args(["-F", "-t", FS_TYPE]);
    if read_only {
        cmd.arg("-r");
    }
    cmd.arg(&url).arg(&folder);
    let result = run(cmd, MOUNT_WAIT);
    let failed = |reason: anyhow::Error| {
        if created {
            remove_folder(&folder);
        }
        Err(reason)
    };
    let output = match result {
        Ok(Some(output)) => output,
        Ok(None) => {
            // `mount` is still running; leave the folder, it may yet be mounted on.
            bail!(
                "mount -F is still running after {} s; check with `mount | grep sp404` \
                 (log: {LOG_HINT})",
                MOUNT_WAIT.as_secs()
            )
        }
        Err(e) => return failed(anyhow!("running /sbin/mount: {e}")),
    };
    if !output.status.success() {
        if fskit_stuck_since(&started) {
            return failed(
                StaleMount {
                    folder: folder.clone(),
                }
                .into(),
            );
        }
        return failed(anyhow!(mount_error(&output, &port)));
    }
    if !is_sp404_mount(&folder) {
        return failed(anyhow!(
            "mount said it worked, but {} isn't an SP-404MKII drive (log: {LOG_HINT})",
            folder.display()
        ));
    }
    println!(
        "SP-404MKII mounted at {}{} (port {port}).",
        folder.display(),
        if read_only { ", read-only" } else { "" }
    );
    if folder == default
        && let Some(parent) = folder.parent()
    {
        let record = parent.join(sidebar::RECORD);
        match sidebar::show(&folder, &record) {
            sidebar::Shown::InSidebar => {
                println!("In Finder it's SP-404MKII, under Locations in the sidebar.")
            }
            sidebar::Shown::RemovedByUser => println!(
                "In Finder it's SP-404MKII, under Go > Computer. (It was removed from the \
                 sidebar; drag it back there to show it again.)"
            ),
            sidebar::Shown::Failed => {
                println!("In Finder it's SP-404MKII, under Go > Computer.")
            }
        }
    }
    println!("Unmount with `sp404drive unmount`, or eject it in Finder.");
    Ok(())
}

pub fn unmount(mount_point: &str) -> anyhow::Result<()> {
    let default = folder_path(crate::DEFAULT_MOUNT)?;
    let mut folder = folder_path(mount_point)?;
    if !is_sp404_mount(&folder) {
        let mounts = sp404_mounts()?;
        // `sp404drive unmount` with no folder: whichever SP-404MKII is mounted.
        if folder == default && mounts.len() == 1 {
            folder = mounts[0].on.clone();
        } else if mounts.is_empty() {
            bail!("the SP-404MKII isn't mounted");
        } else {
            let list: Vec<String> = mounts.iter().map(|m| m.on.display().to_string()).collect();
            bail!(
                "nothing from sp404drive is mounted at {}; mounted: {}",
                folder.display(),
                list.join(", ")
            );
        }
    }
    let mut cmd = Command::new("/sbin/umount");
    cmd.arg(&folder);
    let output = match run(cmd, UNMOUNT_WAIT).context("running /sbin/umount")? {
        Some(output) => output,
        None => {
            // Not done: the drive may still be saving or holding the port, so whatever
            // runs next (unplugging, Roland's app) mustn't take this as unmounted.
            bail!(
                "{} is still unmounting after {} s (saving files?). Don't unplug the \
                 SP-404MKII or start Roland's app yet; `mount` shows when it's gone. See \
                 {LOG_HINT}",
                folder.display(),
                UNMOUNT_WAIT.as_secs()
            );
        }
    };
    if !output.status.success() {
        let text = stderr_text(&output);
        if text.contains("Resource busy") {
            bail!(
                "{} is in use: a program has a file open there, or a terminal is in a \
                 folder inside it. Close it and try again.",
                folder.display()
            );
        }
        bail!("umount failed: {text}");
    }
    if is_sp404_mount(&folder) {
        bail!("umount returned, but {} is still mounted", folder.display());
    }
    // The default folder exists only for the drive; take it away again (only if empty).
    if folder == default {
        remove_folder(&folder);
    }
    println!("Unmounted {}.", folder.display());
    Ok(())
}

/// FSKit file systems on generic URLs, and these settings, need macOS 26.
fn check_macos_version() -> anyhow::Result<()> {
    let version = sysctl_string(c"kern.osproductversion").unwrap_or_default();
    let major: u32 = version
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0);
    if major < 26 {
        bail!(
            "SP-404 Drive needs macOS 26 or later; this Mac has macOS {}",
            if version.is_empty() {
                "(unknown)"
            } else {
                &version
            }
        );
    }
    Ok(())
}

fn sysctl_string(name: &CStr) -> Option<String> {
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: sysctlbyname writes at most `len` bytes into `buf` and updates `len`.
    let r = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        return None;
    }
    let text = CStr::from_bytes_until_nul(&buf[..len]).ok()?;
    Some(text.to_string_lossy().into_owned())
}

/// The folder as an absolute path, with `~/` expanded. Not canonicalized: the folder may
/// not exist yet.
fn folder_path(mount_point: &str) -> anyhow::Result<PathBuf> {
    let path = match mount_point.strip_prefix("~/") {
        Some(rest) => {
            let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME isn't set"))?;
            Path::new(&home).join(rest)
        }
        None => PathBuf::from(mount_point),
    };
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    // Mount points come back from the system without a trailing slash, and with
    // symlinks in the parent resolved (`/tmp` is `/private/tmp`).
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => match parent.canonicalize() {
            Ok(parent) => Ok(parent.join(name)),
            Err(_) => Ok(path),
        },
        _ => Ok(path),
    }
}

/// The port to use: the one asked for (a `/dev/` path or a name in `/dev`), which must
/// belong to an SP-404MKII, or the first unit found.
fn choose_port(asked: Option<String>) -> anyhow::Result<String> {
    let units = sp404_drive::link::find_ports().context("listing serial ports")?;
    match asked {
        None => units.into_iter().next().map(|p| p.name).ok_or_else(|| {
            anyhow!(
                "no SP-404MKII found. Connect it over USB and switch it on \
                 (`sp404drive ports` lists the units found)."
            )
        }),
        Some(name) => {
            let name = if name.starts_with('/') {
                name
            } else {
                format!("/dev/{name}")
            };
            if !units.iter().any(|p| p.name == name) {
                if Path::new(&name).exists() {
                    bail!("{name} isn't an SP-404MKII (`sp404drive ports` lists them)");
                }
                bail!("no serial port {name} (`sp404drive ports` lists the SP-404MKII units)");
            }
            Ok(name)
        }
    }
}

/// Whether a folder entry counts for "empty": Finder's `.DS_Store` doesn't. Finder may
/// write one into the empty mount folder while nothing is mounted there (it stays after
/// an Eject).
fn counts(entry: &io::Result<fs::DirEntry>) -> bool {
    !entry.as_ref().is_ok_and(|e| e.file_name() == ".DS_Store")
}

/// Remove an empty mount folder (and a lone `.DS_Store` in it); leave anything else.
fn remove_folder(folder: &Path) {
    let Ok(entries) = fs::read_dir(folder) else {
        return;
    };
    let entries: Vec<_> = entries.collect();
    if entries.iter().any(counts) {
        return;
    }
    let _ = fs::remove_file(folder.join(".DS_Store"));
    let _ = fs::remove_dir(folder);
}

/// Make sure the folder exists and is empty; returns whether it was created.
fn prepare_folder(folder: &Path) -> anyhow::Result<bool> {
    match fs::read_dir(folder) {
        Ok(mut entries) => {
            if entries.any(|e| counts(&e)) {
                bail!(
                    "{} isn't empty; mounting there would hide what's in it",
                    folder.display()
                );
            }
            Ok(false)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(folder).with_context(|| format!("creating {}", folder.display()))?;
            Ok(true)
        }
        Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => {
            bail!("{} isn't a folder", folder.display())
        }
        Err(e) => Err(anyhow!("{}: {e}", folder.display())),
    }
}

/// `sp404://unit?port=/dev/cu.usbmodem1101&cache-secs=5[&read-only]`.
fn resource_url(port: &str, cache_secs: u32, read_only: bool) -> String {
    let mut url = format!(
        "sp404://unit?port={}&cache-secs={cache_secs}",
        query_escape(port)
    );
    if read_only {
        url.push_str("&read-only");
    }
    url
}

/// Percent-encode everything but unreserved characters and `/` (allowed in a query).
fn query_escape(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// What went wrong, in words, from `mount -F`'s messages. When the extension can't open
/// the drive, mount prints "Loading resource: ... <error>", then "Unable to invoke task";
/// when macOS can't start the extension at all, only the latter.
fn mount_error(output: &Output, port: &str) -> String {
    let text = stderr_text(output);
    if text.contains("Loading resource") {
        if text.contains("Resource busy") {
            return format!(
                "the SP-404MKII's port ({port}) is in use by another program, probably \
                 Roland's SP-404MKII app. Quit it and try again."
            );
        }
        if text.contains("not supported by device") {
            return format!(
                "the SP-404MKII wasn't found on {port}. Is it still connected and switched on?"
            );
        }
        if text.contains("Invalid argument") {
            return format!("the drive refused its settings (log: {LOG_HINT})");
        }
        return format!("{text} (log: {LOG_HINT})");
    }
    if text.contains("Unable to invoke task") {
        return if extension_registered() {
            format!(
                "macOS didn't start the SP-404MKII file system. Is it switched on? {ENABLE_HINT}."
            )
        } else {
            format!(
                "the SP-404MKII file system isn't installed. Put SP-404 Drive.app in \
                 /Applications and open it once, then {ENABLE_HINT}."
            )
        };
    }
    format!("mount failed: {text}")
}

/// Whether macOS knows the extension (the app is installed and was opened once). It says
/// nothing about whether it's switched on.
fn extension_registered() -> bool {
    Command::new("/usr/bin/pluginkit")
        .args(["-m", "-i", EXTENSION_ID])
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(EXTENSION_ID))
        .unwrap_or(true)
}

fn stderr_text(output: &Output) -> String {
    let text = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = text
        .lines()
        .map(|l| {
            l.trim_start_matches("mount: ")
                .trim_start_matches("umount: ")
        })
        .filter(|l| !l.is_empty())
        .collect();
    lines.join("; ")
}

/// Run a command to the end, waiting at most `wait` for it (one wait with a deadline,
/// on a thread that waits for the process). `None` if it's still running.
fn run(mut cmd: Command, wait: Duration) -> io::Result<Option<Output>> {
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(wait) {
        Ok(result) => result.map(Some),
        Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::other("waiter thread died")),
    }
}

/// A mounted SP-404MKII volume.
struct Mounted {
    /// The folder it's mounted on.
    on: PathBuf,
    /// The resource URL it was mounted from.
    from: String,
}

impl Mounted {
    /// The `port` setting in its URL, if any.
    fn port(&self) -> Option<String> {
        let query = self.from.split_once('?')?.1;
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix("port="))
            .map(query_unescape)
    }
}

fn query_unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = text.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn c_text(chars: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = chars
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Every mounted SP-404MKII volume, from the kernel's mount table.
fn sp404_mounts() -> anyhow::Result<Vec<Mounted>> {
    // SAFETY: getfsstat with a null buffer returns the number of mounts; with a buffer,
    // fills at most `bufsize` bytes of statfs records and returns how many it wrote.
    unsafe {
        let n = libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT);
        if n < 0 {
            return Err(io::Error::last_os_error()).context("reading the mount table");
        }
        // Room for a few mounts appearing in between.
        let cap = n as usize + 8;
        let mut buf: Vec<libc::statfs> = Vec::with_capacity(cap);
        let bytes = (cap * std::mem::size_of::<libc::statfs>()) as libc::c_int;
        let got = libc::getfsstat(buf.as_mut_ptr(), bytes, libc::MNT_NOWAIT);
        if got < 0 {
            return Err(io::Error::last_os_error()).context("reading the mount table");
        }
        buf.set_len(got as usize);
        Ok(buf
            .iter()
            .filter(|s| c_text(&s.f_fstypename) == FS_TYPE)
            .map(|s| Mounted {
                on: PathBuf::from(c_text(&s.f_mntonname)),
                from: c_text(&s.f_mntfromname),
            })
            .collect())
    }
}

/// fskitd's record of the FSKit volumes mounted, readable by anyone, written by root.
const LIVEFS_SETTINGS: &str = "/Library/Application Support/livefsd/settings.plist";

/// fskitd is stuck on an earlier mount: it has a stale record of a mount on the folder, or
/// it refused this mount for a reason only a restart clears. The exit status is
/// `STALE_MOUNT_STATUS`, so the app can tell this failure apart.
#[derive(Debug)]
pub struct StaleMount {
    folder: PathBuf,
}

/// The exit status for a `StaleMount`.
pub const STALE_MOUNT_STATUS: u8 = 3;

impl std::fmt::Display for StaleMount {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        // The folder in full: sudo may not keep HOME, which `~` needs.
        write!(
            f,
            "macOS's file system service (fskitd) is stuck on an earlier SP-404MKII mount, \
             so it won't mount on {0} until it's restarted: \
             `sudo sp404drive clear-stale-mount '{0}'`",
            self.folder.display()
        )
    }
}

impl std::error::Error for StaleMount {}

/// The indexes of fskitd's records of a mount on `folder`. When the extension is ended
/// while mounted (macOS did it once, as the app was replaced), fskitd can't finish the
/// unmount and keeps the record; every later mount on that folder then fails, with
/// "Failed to store the mount point in settings file!" (NSCocoaErrorDomain 516) in its log
/// and "Unable to invoke task" from `mount -F`. A record is stale only while nothing is
/// mounted there.
fn stale_records(folder: &Path) -> anyhow::Result<Vec<usize>> {
    if is_sp404_mount(folder) {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    // `plutil -extract` fails past the last record (or with no file, or no records).
    for index in 0.. {
        let output = Command::new("/usr/bin/plutil")
            .args([
                "-extract",
                &format!("mounts.{index}.mountedOn"),
                "raw",
                "-o",
                "-",
            ])
            .arg(LIVEFS_SETTINGS)
            .stderr(Stdio::null())
            .output()
            .context("running /usr/bin/plutil")?;
        if !output.status.success() {
            break;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        if Path::new(text.trim_end_matches('\n')) == folder {
            found.push(index);
        }
    }
    Ok(found)
}

/// What fskitd logs when it refuses a mount that only a restart lets through: a stale
/// record of a mount on the folder (also caught before mounting, from the file), or a task
/// it thinks is still running from an earlier mount. That one came after a mount and
/// unmount just before the app was replaced and relaunched, and stayed after the
/// extension process had ended.
const FSKIT_STUCK: [&str; 2] = [
    "Failed to store the mount point in settings file",
    "Can't start new task, previous task is running",
];

/// The local time as `log show --start` takes it.
fn local_time_now() -> String {
    Command::new("/bin/date")
        .arg("+%Y-%m-%d %H:%M:%S")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Whether fskitd has logged one of the `FSKIT_STUCK` refusals since `start`. Any user can
/// read its log.
fn fskit_stuck_since(start: &str) -> bool {
    if start.is_empty() {
        return false;
    }
    let predicate = FSKIT_STUCK
        .iter()
        .map(|m| format!("eventMessage CONTAINS \"{m}\""))
        .collect::<Vec<_>>()
        .join(" OR ");
    let mut cmd = Command::new("/usr/bin/log");
    cmd.args([
        "show",
        "--style",
        "compact",
        "--start",
        start,
        "--predicate",
    ])
    .arg(format!("process == \"fskitd\" AND ({predicate})"))
    .stderr(Stdio::null());
    match run(cmd, Duration::from_secs(20)) {
        Ok(Some(output)) => String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| FSKIT_STUCK.iter().any(|m| line.contains(m))),
        _ => false,
    }
}

/// `sp404drive clear-stale-mount`, as root: removes fskitd's stale records of a mount on
/// the folder, if any, then restarts fskitd (launchd starts it again when it's next
/// needed). The restart is what counts: fskitd keeps its records in memory too, and with
/// the file fixed but fskitd running on, the next mount still failed.
pub fn clear_stale_mount(mount_point: &str) -> anyhow::Result<()> {
    let folder = folder_path(mount_point)?;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        bail!(
            "this needs administrator rights: `sudo sp404drive clear-stale-mount '{}'`",
            folder.display()
        );
    }
    if is_sp404_mount(&folder) {
        bail!(
            "the SP-404MKII is mounted on {}; nothing to clear",
            folder.display()
        );
    }
    let found = stale_records(&folder)?;
    // Highest first, so the indexes still to go stay valid.
    for index in found.iter().rev() {
        let status = Command::new("/usr/bin/plutil")
            .args(["-remove", &format!("mounts.{index}")])
            .arg(LIVEFS_SETTINGS)
            .status()
            .context("running /usr/bin/plutil")?;
        if !status.success() {
            bail!("plutil couldn't remove record {index} from {LIVEFS_SETTINGS}");
        }
    }
    // Exit status 1 means fskitd wasn't running, which is just as good.
    Command::new("/usr/bin/killall")
        .arg("fskitd")
        .stderr(Stdio::null())
        .status()
        .context("running /usr/bin/killall")?;
    println!("Restarted macOS's file system service (fskitd); mount again now.");
    Ok(())
}

/// Finder's sidebar. macOS files an FSKit volume with no disk under Recent Servers and
/// never adds it to the sidebar by itself (measured on macOS 26.6: mounting, remounting,
/// and restarting Finder and sharedfilelistd with it mounted), so `mount` adds it to
/// Locations, once. The list's snapshot holds only visible items, and "Remove from
/// Sidebar" hides an item rather than deleting it, so a record file beside the mount
/// folder says it was added: missing from the list after that, the user removed it.
mod sidebar {
    use std::ffi::{OsStr, c_void};
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;

    /// The record file's name, in the mount folder's parent.
    pub const RECORD: &str = "sidebar-added";

    pub enum Shown {
        InSidebar,
        RemovedByUser,
        Failed,
    }

    type CFTypeRef = *const c_void;

    // LSSharedFileListItemResolutionFlags.
    const NO_USER_INTERACTION: u32 = 1 << 0;
    const DO_NOT_MOUNT_VOLUMES: u32 = 1 << 1;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(cf: CFTypeRef);
        fn CFArrayGetCount(array: CFTypeRef) -> isize;
        fn CFArrayGetValueAtIndex(array: CFTypeRef, index: isize) -> CFTypeRef;
        fn CFURLCreateFromFileSystemRepresentation(
            allocator: CFTypeRef,
            buffer: *const u8,
            length: isize,
            is_directory: u8,
        ) -> CFTypeRef;
        fn CFURLGetFileSystemRepresentation(
            url: CFTypeRef,
            resolve_against_base: u8,
            buffer: *mut u8,
            max_length: isize,
        ) -> u8;
    }

    // Deprecated since macOS 10.11 but still what Finder's lists are made of; there's no
    // replacement for adding to the sidebar.
    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        static kLSSharedFileListFavoriteVolumes: CFTypeRef;
        static kLSSharedFileListItemLast: CFTypeRef;
        fn LSSharedFileListCreate(
            allocator: CFTypeRef,
            list_type: CFTypeRef,
            options: CFTypeRef,
        ) -> CFTypeRef;
        fn LSSharedFileListCopySnapshot(list: CFTypeRef, seed: *mut u32) -> CFTypeRef;
        fn LSSharedFileListItemCopyResolvedURL(
            item: CFTypeRef,
            flags: u32,
            error: *mut CFTypeRef,
        ) -> CFTypeRef;
        fn LSSharedFileListInsertItemURL(
            list: CFTypeRef,
            after: CFTypeRef,
            name: CFTypeRef,
            icon: CFTypeRef,
            url: CFTypeRef,
            properties: CFTypeRef,
            properties_to_clear: CFTypeRef,
        ) -> CFTypeRef;
    }

    /// A Core Foundation object this code owns (from a Create or Copy call); may be null.
    struct Owned(CFTypeRef);

    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: an owned, non-null Core Foundation object, released once.
                unsafe { CFRelease(self.0) }
            }
        }
    }

    /// Makes sure the volume mounted on `folder` is in the sidebar, unless the user
    /// removed it.
    pub fn show(folder: &Path, record: &Path) -> Shown {
        // SAFETY: Core Foundation and Launch Services calls with valid arguments; what they
        // create or copy is released by `Owned`.
        unsafe {
            let list = Owned(LSSharedFileListCreate(
                ptr::null(),
                kLSSharedFileListFavoriteVolumes,
                ptr::null(),
            ));
            if list.0.is_null() {
                return Shown::Failed;
            }
            if listed(&list, folder) {
                let _ = fs::write(record, b"");
                return Shown::InSidebar;
            }
            if record.exists() {
                return Shown::RemovedByUser;
            }
            let bytes = folder.as_os_str().as_bytes();
            let url = Owned(CFURLCreateFromFileSystemRepresentation(
                ptr::null(),
                bytes.as_ptr(),
                bytes.len() as isize,
                1,
            ));
            if url.0.is_null() {
                return Shown::Failed;
            }
            let item = Owned(LSSharedFileListInsertItemURL(
                list.0,
                kLSSharedFileListItemLast,
                ptr::null(),
                ptr::null(),
                url.0,
                ptr::null(),
                ptr::null(),
            ));
            if item.0.is_null() {
                return Shown::Failed;
            }
            let _ = fs::write(record, b"");
            Shown::InSidebar
        }
    }

    /// Whether a visible item in the list resolves to `folder`.
    fn listed(list: &Owned, folder: &Path) -> bool {
        // SAFETY: as in `show`; items from the snapshot are borrowed from it.
        unsafe {
            let mut seed = 0u32;
            let items = Owned(LSSharedFileListCopySnapshot(list.0, &mut seed));
            if items.0.is_null() {
                return false;
            }
            for index in 0..CFArrayGetCount(items.0) {
                let item = CFArrayGetValueAtIndex(items.0, index);
                let url = Owned(LSSharedFileListItemCopyResolvedURL(
                    item,
                    NO_USER_INTERACTION | DO_NOT_MOUNT_VOLUMES,
                    ptr::null_mut(),
                ));
                if url.0.is_null() {
                    continue;
                }
                let mut path = [0u8; libc::PATH_MAX as usize];
                if CFURLGetFileSystemRepresentation(
                    url.0,
                    1,
                    path.as_mut_ptr(),
                    path.len() as isize,
                ) != 0
                {
                    let len = path.iter().position(|&b| b == 0).unwrap_or(path.len());
                    if Path::new(OsStr::from_bytes(&path[..len])) == folder {
                        return true;
                    }
                }
            }
            false
        }
    }
}

/// Whether an SP-404MKII volume is mounted on exactly this folder.
fn is_sp404_mount(folder: &Path) -> bool {
    let Ok(path) = CString::new(folder.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    // SAFETY: statfs fills the zeroed record for a valid C path.
    unsafe {
        let mut s: libc::statfs = std::mem::zeroed();
        if libc::statfs(path.as_ptr(), &mut s) != 0 {
            return false;
        }
        c_text(&s.f_fstypename) == FS_TYPE && Path::new(&c_text(&s.f_mntonname)) == folder
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(stderr: &str) -> Output {
        use std::os::unix::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(69 << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    /// The messages measured on macOS 26.6.
    #[test]
    fn mount_errors() {
        let busy = failed(
            "mount: Loading resource: The operation couldn\u{2019}t be completed. Resource busy\n\
             mount: Unable to invoke task\n",
        );
        assert!(mount_error(&busy, "/dev/p").contains("in use by another program"));
        let gone = failed(
            "mount: Loading resource: The operation couldn\u{2019}t be completed. \
             Operation not supported by device\nmount: Unable to invoke task\n",
        );
        assert!(mount_error(&gone, "/dev/p").contains("wasn't found on /dev/p"));
        // What an unknown file system type gives (`mount -F -t nosuchfs`).
        let not_started = failed("mount: Unable to invoke task\n");
        assert!(mount_error(&not_started, "/dev/p").contains("File System Extensions"));
        let other = failed("mount: /x: invalid file system.\n");
        assert_eq!(
            mount_error(&other, "/dev/p"),
            "mount failed: /x: invalid file system."
        );
    }

    #[test]
    fn url_round_trip() {
        let url = resource_url("/dev/cu.usbmodem1101", 5, true);
        assert_eq!(
            url,
            "sp404://unit?port=/dev/cu.usbmodem1101&cache-secs=5&read-only"
        );
        let m = Mounted {
            on: PathBuf::from("/x"),
            from: resource_url("/dev/cu.odd name&1", 5, false),
        };
        assert_eq!(
            m.from,
            "sp404://unit?port=/dev/cu.odd%20name%261&cache-secs=5"
        );
        assert_eq!(m.port().as_deref(), Some("/dev/cu.odd name&1"));
        let by_hand = Mounted {
            on: PathBuf::from("/x"),
            from: "sp404://unit".into(),
        };
        assert_eq!(by_hand.port(), None);
    }

    #[test]
    fn folder_paths() {
        let home = std::env::var("HOME").unwrap();
        let home = Path::new(&home).canonicalize().unwrap();
        assert_eq!(
            folder_path("~/SP-404MKII").unwrap(),
            home.join("SP-404MKII")
        );
        assert_eq!(
            folder_path("/tmp/sp404-x").unwrap(),
            Path::new("/private/tmp/sp404-x")
        );
    }

    #[test]
    fn a_lone_ds_store_leaves_the_folder_empty() {
        let dir = std::env::temp_dir().join(format!("sp404-cli-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(prepare_folder(&dir).unwrap(), "created");
        fs::write(dir.join(".DS_Store"), b"finder").unwrap();
        assert!(!prepare_folder(&dir).unwrap(), "empty but for .DS_Store");
        fs::write(dir.join("song.wav"), b"x").unwrap();
        assert!(prepare_folder(&dir).is_err());
        remove_folder(&dir);
        assert!(dir.join("song.wav").exists(), "a folder with files stays");
        fs::remove_file(dir.join("song.wav")).unwrap();
        remove_folder(&dir);
        assert!(!dir.exists());
    }

    #[test]
    fn reads_the_macos_version() {
        let version = sysctl_string(c"kern.osproductversion").unwrap();
        assert!(
            version.split('.').next().unwrap().parse::<u32>().is_ok(),
            "{version}"
        );
    }
}
