//! C interface to the drive's core, for the macOS FSKit extension (`macos/`). The header
//! is `include/sp404.h`; keep the two in step.
//!
//! The extension opens one [`SpDrive`] per mounted volume: the serial link to the unit,
//! the [`Drive`] over it, and the numbered [`Items`] FSKit works with. Every call returns
//! 0 or an errno value (reads and writes: a byte count, or a negative errno). A panic is
//! caught and reported as `EIO`, so it can't take the extension down mid-request.

use std::ffi::{CStr, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration, UNIX_EPOCH};

use sp404_drive::items::{self, Item, Items};
use sp404_drive::link::{Link, SerialPort, find_ports};
use sp404_drive::store::DeviceStore;
use sp404_drive::vfs::{Drive, FsError, Options};

pub struct SpDrive {
    items: Items,
    /// Set by [`sp_drive_set_read_only`]: FSKit passes the mount's options only after
    /// the drive is open.
    read_only: AtomicBool,
}

impl SpDrive {
    fn read_only(&self) -> bool {
        self.read_only.load(Ordering::Relaxed) || self.items.drive().read_only()
    }

    fn writable(&self) -> Result<(), i32> {
        if self.read_only() {
            Err(libc_errno::EROFS)
        } else {
            Ok(())
        }
    }
}

/// A file or folder: its item number, size and kind.
#[repr(C)]
pub struct SpAttr {
    pub id: u64,
    pub size: u64,
    pub is_dir: bool,
}

impl From<&Item> for SpAttr {
    fn from(item: &Item) -> Self {
        SpAttr {
            id: item.id,
            size: if item.attr.is_dir { 0 } else { item.attr.size },
            is_dir: item.attr.is_dir,
        }
    }
}

fn errno(e: &FsError) -> i32 {
    match e {
        FsError::NotFound => libc_errno::ENOENT,
        FsError::NotADirectory => libc_errno::ENOTDIR,
        FsError::IsADirectory => libc_errno::EISDIR,
        FsError::Exists => libc_errno::EEXIST,
        FsError::NotEmpty => libc_errno::ENOTEMPTY,
        FsError::AccessDenied => libc_errno::EACCES,
        FsError::Refused(what) if what == items::TOO_LARGE => libc_errno::EFBIG,
        // The unit says no without saying why (a full card, say): not a permission.
        FsError::Refused(_) => libc_errno::EIO,
        // Finder and `mv` copy and delete instead.
        FsError::CrossVolume => libc_errno::EXDEV,
        FsError::InvalidName => libc_errno::EINVAL,
        FsError::ReadOnly => libc_errno::EROFS,
        FsError::Disconnected => libc_errno::ENXIO,
        FsError::Io(_) => libc_errno::EIO,
    }
}

/// The errno values used here, as macOS numbers them.
mod libc_errno {
    pub const ENOENT: i32 = 2;
    pub const EIO: i32 = 5;
    pub const ENXIO: i32 = 6;
    pub const EACCES: i32 = 13;
    pub const EEXIST: i32 = 17;
    pub const EXDEV: i32 = 18;
    pub const ENOTDIR: i32 = 20;
    pub const EISDIR: i32 = 21;
    pub const EINVAL: i32 = 22;
    pub const EFBIG: i32 = 27;
    pub const EROFS: i32 = 30;
    pub const ENAMETOOLONG: i32 = 63;
    pub const ENOTEMPTY: i32 = 66;
}

/// Run `f`, turning a panic into `EIO`.
fn guard(f: impl FnOnce() -> Result<(), i32>) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => e,
        Err(_) => {
            log::error!("panic in a drive call");
            libc_errno::EIO
        }
    }
}

/// Run `f` for a byte count: `n >= 0`, or a negative errno.
fn guard_count(f: impl FnOnce() -> Result<usize, i32>) -> i64 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(n)) => n as i64,
        Ok(Err(e)) => -(e as i64),
        Err(_) => {
            log::error!("panic in a drive call");
            -(libc_errno::EIO as i64)
        }
    }
}

/// # Safety
/// `s` is null or a NUL-terminated string.
unsafe fn str_arg<'a>(s: *const c_char) -> Result<&'a str, i32> {
    if s.is_null() {
        return Err(libc_errno::EINVAL);
    }
    // SAFETY: per this function's contract.
    unsafe { CStr::from_ptr(s) }
        .to_str()
        .map_err(|_| libc_errno::EINVAL)
}

/// Copy `s` into `out` (capacity `cap`, NUL included).
///
/// # Safety
/// `out` is null or has room for `cap` bytes.
unsafe fn str_out(s: &str, out: *mut c_char, cap: usize) -> Result<(), i32> {
    if out.is_null() || cap == 0 {
        return Ok(());
    }
    if s.len() + 1 > cap {
        return Err(libc_errno::ENAMETOOLONG);
    }
    // SAFETY: room for s.len() + 1 bytes, checked above.
    unsafe {
        std::ptr::copy_nonoverlapping(s.as_ptr(), out.cast::<u8>(), s.len());
        *out.add(s.len()) = 0;
    }
    Ok(())
}

/// # Safety
/// `d` is a live pointer from [`sp_drive_open`].
unsafe fn drive<'a>(d: *const SpDrive) -> Result<&'a SpDrive, i32> {
    // SAFETY: per this function's contract.
    unsafe { d.as_ref() }.ok_or(libc_errno::EINVAL)
}

// ---- logging ---------------------------------------------------------------------------

/// Receives the core's log lines: level 1 error, 2 warn, 3 info, 4 debug, 5 trace.
pub type SpLogFn = extern "C" fn(level: i32, message: *const c_char);

static LOG_FN: AtomicUsize = AtomicUsize::new(0);
static LOG_LEVEL: AtomicI32 = AtomicI32::new(3);

struct CallbackLogger;

impl log::Log for CallbackLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        (metadata.level() as i32) <= LOG_LEVEL.load(Ordering::Relaxed)
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let f = LOG_FN.load(Ordering::Relaxed);
        if f == 0 {
            return;
        }
        // SAFETY: only ever set from a valid SpLogFn in sp_set_logger.
        let f: SpLogFn = unsafe { std::mem::transmute::<usize, SpLogFn>(f) };
        let line = format!("{}: {}", record.target(), record.args()).replace('\0', " ");
        if let Ok(c) = std::ffi::CString::new(line) {
            f(record.level() as i32, c.as_ptr());
        }
    }

    fn flush(&self) {}
}

/// Send the core's log lines to `f`, up to `max_level` (see [`SpLogFn`]).
#[unsafe(no_mangle)]
pub extern "C" fn sp_set_logger(f: SpLogFn, max_level: i32) {
    static INSTALL: Once = Once::new();
    LOG_FN.store(f as usize, Ordering::Relaxed);
    LOG_LEVEL.store(max_level, Ordering::Relaxed);
    INSTALL.call_once(|| {
        if log::set_logger(&CallbackLogger).is_ok() {
            log::set_max_level(log::LevelFilter::Trace);
        }
    });
}

// ---- the drive -------------------------------------------------------------------------

/// The serial port to use: the first SP-404MKII the system lists by USB id (0582:02E7).
/// Never just any `/dev/cu.usbmodem*`: that may be another device (an Arduino, an audio
/// interface), which would be sent the unit's protocol. Measured: the lookup works inside
/// the extension's sandbox.
fn default_port() -> Result<String, String> {
    match find_ports() {
        Ok(ports) => match ports.first() {
            Some(port) => {
                log::info!("found the SP-404MKII by USB id on {}", port.name);
                Ok(port.name.clone())
            }
            None => Err("no SP-404MKII found (USB 0582:02E7)".to_string()),
        },
        Err(e) => Err(format!("no SP-404MKII found: listing ports by USB id: {e}")),
    }
}

/// Called once, on the link's reader thread, when the unit is unplugged or switched off.
pub type SpClosedFn = extern "C" fn(context: *mut c_void);

/// Connect to the unit and open the drive. `port` may be NULL (find it). `on_closed` is
/// called once if the unit goes away. On failure, returns NULL with the reason in `err`.
///
/// # Safety
/// `port` is NULL or a NUL-terminated string; `err` is NULL or has room for `err_len`
/// bytes; `context` is passed back to `on_closed` as is, from another thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sp_drive_open(
    port: *const c_char,
    read_only: bool,
    cache_secs: u32,
    on_closed: Option<SpClosedFn>,
    context: *mut c_void,
    err: *mut c_char,
    err_len: usize,
) -> *mut SpDrive {
    let port = if port.is_null() {
        None
    } else {
        // SAFETY: per this function's contract.
        unsafe { str_arg(port) }.ok().map(str::to_string)
    };
    let context = context as usize;
    let opened = catch_unwind(AssertUnwindSafe(|| -> Result<SpDrive, String> {
        let name = match port {
            Some(name) => name,
            None => default_port()?,
        };
        let serial = SerialPort::open(&name).map_err(|e| {
            format!("opening {name}: {e} (is Roland's SP-404MKII app or sp404drive using it?)")
        })?;
        let link = Link::new(&name, Arc::new(serial), move || {
            if let Some(f) = on_closed {
                f(context as *mut c_void);
            }
        })
        .map_err(|e| format!("starting the link on {name}: {e}"))?;
        log::info!("connected to the SP-404MKII on {name}");
        let drive = Drive::new(
            Arc::new(DeviceStore::new(link)),
            Options {
                read_only,
                cache_ttl: Duration::from_secs(cache_secs as u64),
            },
        );
        Ok(SpDrive {
            items: Items::new(Arc::new(drive)),
            read_only: AtomicBool::new(false),
        })
    }));
    match opened {
        Ok(Ok(d)) => Box::into_raw(Box::new(d)),
        Ok(Err(reason)) => {
            log::error!("{reason}");
            // Cut the message to fit, on a character boundary.
            let mut cut = reason.as_str();
            while cut.len() + 1 > err_len && !cut.is_empty() {
                let mut end = cut.len() - 1;
                while !cut.is_char_boundary(end) {
                    end -= 1;
                }
                cut = &cut[..end];
            }
            // SAFETY: per this function's contract.
            let _ = unsafe { str_out(cut, err, err_len) };
            std::ptr::null_mut()
        }
        Err(_) => {
            // SAFETY: per this function's contract.
            let _ = unsafe { str_out("internal error opening the drive", err, err_len) };
            std::ptr::null_mut()
        }
    }
}

/// Save everything, wait out the unit's session, close the port, and free the drive.
///
/// # Safety
/// `d` is NULL or a pointer from [`sp_drive_open`], not used again afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sp_drive_close(d: *mut SpDrive) {
    if d.is_null() {
        return;
    }
    // SAFETY: per this function's contract.
    let d = unsafe { Box::from_raw(d) };
    if catch_unwind(AssertUnwindSafe(|| d.items.shutdown())).is_err() {
        log::error!("panic while closing the drive");
        // Still wait out the unit's session before the port closes: closing it during a
        // session can freeze the unit.
        let _ = catch_unwind(AssertUnwindSafe(|| d.items.drive().goodbye()));
    }
    drop(d);
    log::info!("drive closed");
}

/// Seconds since 1970 when the drive was opened: the time every file shows.
///
/// # Safety
/// `d` is a pointer from [`sp_drive_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sp_drive_time(d: *const SpDrive) -> i64 {
    // SAFETY: per this function's contract.
    let Ok(d) = (unsafe { drive(d) }) else {
        return 0;
    };
    catch_unwind(AssertUnwindSafe(|| {
        d.items
            .drive()
            .mounted_at()
            .duration_since(UNIX_EPOCH)
            .map(|t| t.as_secs() as i64)
            .unwrap_or(0)
    }))
    .unwrap_or(0)
}

/// # Safety
/// `d` is a pointer from [`sp_drive_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sp_drive_read_only(d: *const SpDrive) -> bool {
    // SAFETY: per this function's contract.
    unsafe { drive(d) }.is_ok_and(|d| d.read_only())
}

/// Refuse every change from now on (`read_only` true), or stop refusing them (false). A
/// drive opened read-only stays read-only.
///
/// # Safety
/// `d` is a pointer from [`sp_drive_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sp_drive_set_read_only(d: *const SpDrive, read_only: bool) {
    // SAFETY: per this function's contract.
    if let Ok(d) = unsafe { drive(d) } {
        d.read_only.store(read_only, Ordering::Relaxed);
    }
}

/// Total and free bytes of the internal storage.
///
/// # Safety
/// `d` is a pointer from [`sp_drive_open`]; `total` and `free` are valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sp_space(d: *const SpDrive, total: *mut u64, free: *mut u64) {
    // SAFETY: per this function's contract.
    if let Ok(d) = unsafe { drive(d) } {
        let Ok((t, f)) = catch_unwind(AssertUnwindSafe(|| d.items.drive().space())) else {
            log::error!("panic in a drive call");
            return;
        };
        // SAFETY: per this function's contract.
        unsafe {
            *total = t;
            *free = f;
        }
    }
}

// ---- items -----------------------------------------------------------------------------

/// Look up `name` in folder `dir`. Fills `out`, and `name_out` with the name as the unit
/// spells it.
///
/// # Safety
/// `d` is from [`sp_drive_open`]; `name` is a NUL-terminated string; `out` is valid;
/// `name_out` is NULL or has room for `name_cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sp_lookup(
    d: *const SpDrive,
    dir: u64,
    name: *const c_char,
    out: *mut SpAttr,
    name_out: *mut c_char,
    name_cap: usize,
) -> i32 {
    guard(|| {
        // SAFETY: per this function's contract.
        let (d, name) = unsafe { (drive(d)?, str_arg(name)?) };
        let item = d.items.lookup(dir, name).map_err(|e| errno(&e))?;
        // SAFETY: per this function's contract.
        unsafe {
            str_out(&item.attr.name, name_out, name_cap)?;
            *out = SpAttr::from(&item);
        }
        Ok(())
    })
}

/// # Safety
/// `d` is from [`sp_drive_open`]; `out` is valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sp_getattr(d: *const SpDrive, id: u64, out: *mut SpAttr) -> i32 {
    guard(|| {
        // SAFETY: per this function's contract.
        let d = unsafe { drive(d)? };
        let attr = d.items.attr(id).map_err(|e| errno(&e))?;
        // SAFETY: per this function's contract.
        unsafe { *out = SpAttr::from(&Item { id, attr }) };
        Ok(())
    })
}

/// Called for each entry of a listing; return true to stop.
pub type SpListFn =
    extern "C" fn(context: *mut c_void, name: *const c_char, attr: *const SpAttr) -> bool;

/// List folder `dir`, calling `f` for each entry.
///
/// # Safety
/// `d` is from [`sp_drive_open`]; `context` is passed to `f` as is.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sp_list(
    d: *const SpDrive,
    dir: u64,
    f: SpListFn,
    context: *mut c_void,
) -> i32 {
    guard(|| {
        // SAFETY: per this function's contract.
        let d = unsafe { drive(d)? };
        for item in d.items.list(dir).map_err(|e| errno(&e))? {
            let Ok(name) = std::ffi::CString::new(item.attr.name.as_str()) else {
                continue;
            };
            let attr = SpAttr::from(&item);
            if f(context, name.as_ptr(), &attr) {
                break;
            }
        }
        Ok(())
    })
}

/// FSKit no longer holds item `id`.
///
/// # Safety
/// `d` is from [`sp_drive_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sp_reclaim(d: *const SpDrive, id: u64) {
    // SAFETY: per this function's contract.
    if let Ok(d) = unsafe { drive(d) } {
        let _ = catch_unwind(AssertUnwindSafe(|| d.items.reclaim(id)));
    }
}

/// # Safety
/// `d` is from [`sp_drive_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sp_open(d: *const SpDrive, id: u64, write: bool) -> i32 {
    guard(|| {
        // SAFETY: per this function's contract.
        let d = unsafe { drive(d)? };
        if write {
            d.writable()?;
        }
        d.items.open(id, write).map_err(|e| errno(&e))
    })
}

/// Close item `id`, keeping the modes still in use. Uploads a written file when the last
/// writer closes; an upload failure is returned.
///
/// # Safety
/// `d` is from [`sp_drive_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sp_close(
    d: *const SpDrive,
    id: u64,
    keep_read: bool,
    keep_write: bool,
) -> i32 {
    guard(|| {
        // SAFETY: per this function's contract.
        let d = unsafe { drive(d)? };
        d.items
            .close(id, keep_read, keep_write)
            .map_err(|e| errno(&e))
    })
}

/// # Safety
/// `d` is from [`sp_drive_open`]; `buf` has room for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sp_read(
    d: *const SpDrive,
    id: u64,
    offset: u64,
    buf: *mut u8,
    len: usize,
) -> i64 {
    guard_count(|| {
        // SAFETY: per this function's contract.
        let d = unsafe { drive(d)? };
        if len == 0 {
            return Ok(0);
        }
        // SAFETY: per this function's contract.
        let buf = unsafe { std::slice::from_raw_parts_mut(buf, len) };
        d.items.read(id, offset, buf).map_err(|e| errno(&e))
    })
}

/// # Safety
/// `d` is from [`sp_drive_open`]; `data` holds `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sp_write(
    d: *const SpDrive,
    id: u64,
    offset: u64,
    data: *const u8,
    len: usize,
) -> i64 {
    guard_count(|| {
        // SAFETY: per this function's contract.
        let d = unsafe { drive(d)? };
        d.writable()?;
        if len == 0 {
            return Ok(0);
        }
        // SAFETY: per this function's contract.
        let data = unsafe { std::slice::from_raw_parts(data, len) };
        d.items.write(id, offset, data).map_err(|e| errno(&e))
    })
}

/// # Safety
/// `d` is from [`sp_drive_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sp_set_size(d: *const SpDrive, id: u64, size: u64) -> i32 {
    guard(|| {
        // SAFETY: per this function's contract.
        let d = unsafe { drive(d)? };
        d.writable()?;
        d.items.set_size(id, size).map_err(|e| errno(&e))
    })
}

/// Create a file or folder `name` in folder `dir`; fills `out`.
///
/// # Safety
/// `d` is from [`sp_drive_open`]; `name` is a NUL-terminated string; `out` is valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sp_create(
    d: *const SpDrive,
    dir: u64,
    name: *const c_char,
    is_dir: bool,
    out: *mut SpAttr,
) -> i32 {
    guard(|| {
        // SAFETY: per this function's contract.
        let (d, name) = unsafe { (drive(d)?, str_arg(name)?) };
        d.writable()?;
        let item = d.items.create(dir, name, is_dir).map_err(|e| errno(&e))?;
        // SAFETY: per this function's contract.
        unsafe { *out = SpAttr::from(&item) };
        Ok(())
    })
}

/// Delete `name` from folder `dir`: a file or an empty folder.
///
/// # Safety
/// `d` is from [`sp_drive_open`]; `name` is a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sp_remove(d: *const SpDrive, dir: u64, name: *const c_char) -> i32 {
    guard(|| {
        // SAFETY: per this function's contract.
        let (d, name) = unsafe { (drive(d)?, str_arg(name)?) };
        d.writable()?;
        d.items.remove(dir, name).map_err(|e| errno(&e))
    })
}

/// Rename `from_name` in `from_dir` to `to_name` in `to_dir`, replacing a file there if
/// `replace`. Fills `name_out` with the new name as stored.
///
/// # Safety
/// `d` is from [`sp_drive_open`]; the names are NUL-terminated strings; `name_out` is
/// NULL or has room for `name_cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sp_rename(
    d: *const SpDrive,
    from_dir: u64,
    from_name: *const c_char,
    to_dir: u64,
    to_name: *const c_char,
    replace: bool,
    name_out: *mut c_char,
    name_cap: usize,
) -> i32 {
    guard(|| {
        // SAFETY: per this function's contract.
        let (d, from_name, to_name) =
            unsafe { (drive(d)?, str_arg(from_name)?, str_arg(to_name)?) };
        d.writable()?;
        let name = d
            .items
            .rename(from_dir, from_name, to_dir, to_name, replace)
            .map_err(|e| errno(&e))?;
        // SAFETY: per this function's contract.
        unsafe { str_out(&name, name_out, name_cap) }
    })
}

/// Upload every open file with unsaved changes.
///
/// # Safety
/// `d` is from [`sp_drive_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sp_sync(d: *const SpDrive) -> i32 {
    guard(|| {
        // SAFETY: per this function's contract.
        let d = unsafe { drive(d)? };
        d.items.sync().map_err(|e| errno(&e))
    })
}
