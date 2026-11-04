//! Windows front end: the drive as a WinFsp file system, mounted on a drive letter or an
//! empty folder.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::UNIX_EPOCH;

use anyhow::{Context, anyhow, bail};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HLOCAL, LocalFree, NTSTATUS,
    STATUS_ACCESS_DENIED, STATUS_BUFFER_OVERFLOW, STATUS_DEVICE_NOT_CONNECTED,
    STATUS_DIRECTORY_NOT_EMPTY, STATUS_END_OF_FILE, STATUS_FILE_IS_A_DIRECTORY,
    STATUS_IO_DEVICE_ERROR, STATUS_MEDIA_WRITE_PROTECTED, STATUS_NOT_A_DIRECTORY,
    STATUS_NOT_SAME_DEVICE, STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_INVALID,
    STATUS_OBJECT_NAME_NOT_FOUND,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::PSECURITY_DESCRIPTOR;
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_DIRECTORY};
use windows::Win32::System::LibraryLoader::LoadLibraryW;
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
};
use windows::core::{HSTRING, PCWSTR, w};
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::host::{FileSystemHost, FineGuard, VolumeParams};
use winfsp::{FspError, U16CStr};

use crate::vfs::{Attr, Drive, FsError, OpenFile, VPath};

const VOLUME_LABEL: &str = "SP-404MKII";
/// Allocation unit reported to Windows; only affects "size on disk".
const ALLOCATION_UNIT: u64 = 4096;
/// Everyone may do anything; the device has no notion of users. A read-only mount is
/// enforced by the volume flag, not by this.
const SDDL: PCWSTR = w!("O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)");
/// Cleanup flag: the file was marked for deletion.
const CLEANUP_DELETE: u32 = 0x01;
/// NtCreateFile options (from the WDK, which the `windows` crate does not expose here).
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;

fn status(e: FsError) -> FspError {
    let code: NTSTATUS = match e {
        FsError::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
        FsError::NotADirectory => STATUS_NOT_A_DIRECTORY,
        FsError::IsADirectory => STATUS_FILE_IS_A_DIRECTORY,
        FsError::Exists => STATUS_OBJECT_NAME_COLLISION,
        FsError::NotEmpty => STATUS_DIRECTORY_NOT_EMPTY,
        FsError::AccessDenied | FsError::Refused(_) => STATUS_ACCESS_DENIED,
        FsError::CrossVolume => STATUS_NOT_SAME_DEVICE,
        FsError::InvalidName => STATUS_OBJECT_NAME_INVALID,
        FsError::ReadOnly => STATUS_MEDIA_WRITE_PROTECTED,
        FsError::Disconnected => STATUS_DEVICE_NOT_CONNECTED,
        FsError::Io(_) => STATUS_IO_DEVICE_ERROR,
    };
    code.into()
}

fn parse(name: &U16CStr) -> winfsp::Result<VPath> {
    VPath::parse(&name.to_string_lossy()).map_err(status)
}

/// A FILETIME: 100 ns intervals since 1601.
fn filetime(t: std::time::SystemTime) -> u64 {
    const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
    let since_unix = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    (since_unix.as_secs() + EPOCH_DIFF_SECS) * 10_000_000
        + u64::from(since_unix.subsec_nanos() / 100)
}

pub struct WinFile {
    file: OpenFile,
    dir_buffer: DirBuffer,
}

pub struct WinFs {
    drive: Arc<Drive>,
    security: Vec<u8>,
    time: u64,
}

impl WinFs {
    fn new(drive: Arc<Drive>) -> anyhow::Result<Self> {
        let time = filetime(drive.mounted_at());
        Ok(WinFs {
            drive,
            security: security_descriptor()?,
            time,
        })
    }

    fn fill(&self, info: &mut FileInfo, attr: &Attr) {
        info.file_attributes = if attr.is_dir {
            FILE_ATTRIBUTE_DIRECTORY.0
        } else {
            FILE_ATTRIBUTE_ARCHIVE.0
        };
        info.file_size = if attr.is_dir { 0 } else { attr.size };
        info.allocation_size = info.file_size.div_ceil(ALLOCATION_UNIT) * ALLOCATION_UNIT;
        info.creation_time = self.time;
        info.last_access_time = self.time;
        info.last_write_time = self.time;
        info.change_time = self.time;
        info.reparse_tag = 0;
        info.index_number = 0;
        info.hard_links = 0;
        info.ea_size = 0;
    }

    fn wrap(&self, file: OpenFile, info: &mut OpenFileInfo) -> WinFile {
        self.fill(info.as_mut(), &file.attr());
        WinFile {
            file,
            dir_buffer: DirBuffer::new(),
        }
    }

    fn copy_security(&self, buf: Option<&mut [c_void]>) -> winfsp::Result<u64> {
        let len = self.security.len();
        if let Some(buf) = buf {
            if buf.len() < len {
                return Err(STATUS_BUFFER_OVERFLOW.into());
            }
            // SAFETY: `buf` has room for `len` bytes, checked above.
            unsafe {
                std::ptr::copy_nonoverlapping(self.security.as_ptr(), buf.as_mut_ptr().cast(), len);
            }
        }
        Ok(len as u64)
    }
}

/// The self-relative security descriptor every file reports.
fn security_descriptor() -> anyhow::Result<Vec<u8>> {
    let mut psd = PSECURITY_DESCRIPTOR::default();
    let mut len = 0u32;
    // SAFETY: valid SDDL string and out pointers; the result is freed with LocalFree.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            SDDL,
            SDDL_REVISION_1,
            &mut psd,
            Some(&mut len),
        )?;
        let bytes = std::slice::from_raw_parts(psd.0 as *const u8, len as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(psd.0)));
        Ok(bytes)
    }
}

impl FileSystemContext for WinFs {
    type FileContext = WinFile;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let attr = self.drive.lookup(&parse(file_name)?).map_err(status)?;
        let mut info = FileInfo::default();
        self.fill(&mut info, &attr);
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: self.copy_security(security_descriptor)?,
            attributes: info.file_attributes,
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<WinFile> {
        let file = self.drive.open(&parse(file_name)?).map_err(status)?;
        if create_options & FILE_DIRECTORY_FILE != 0 && !file.is_dir() {
            self.drive.release(file);
            return Err(status(FsError::NotADirectory));
        }
        if create_options & FILE_NON_DIRECTORY_FILE != 0 && file.is_dir() {
            self.drive.release(file);
            return Err(status(FsError::IsADirectory));
        }
        Ok(self.wrap(file, file_info))
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        _file_attributes: u32,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<WinFile> {
        let is_dir = create_options & FILE_DIRECTORY_FILE != 0;
        let file = self
            .drive
            .create(&parse(file_name)?, is_dir)
            .map_err(status)?;
        Ok(self.wrap(file, file_info))
    }

    fn cleanup(&self, context: &WinFile, _file_name: Option<&U16CStr>, flags: u32) {
        if flags & CLEANUP_DELETE != 0 {
            if let Err(e) = self.drive.delete(&context.file) {
                log::error!("could not delete {}: {e}", context.file.path());
            }
        } else if let Err(e) = self.drive.flush(&context.file) {
            log::error!("could not save {}: {e}", context.file.path());
        }
    }

    fn close(&self, context: WinFile) {
        self.drive.release(context.file);
    }

    fn flush(&self, context: Option<&WinFile>, file_info: &mut FileInfo) -> winfsp::Result<()> {
        if let Some(context) = context {
            self.drive.flush(&context.file).map_err(status)?;
            self.fill(file_info, &context.file.attr());
        }
        Ok(())
    }

    fn get_file_info(&self, context: &WinFile, file_info: &mut FileInfo) -> winfsp::Result<()> {
        self.fill(file_info, &context.file.attr());
        Ok(())
    }

    fn get_security(
        &self,
        _context: &WinFile,
        security_descriptor: Option<&mut [c_void]>,
    ) -> winfsp::Result<u64> {
        self.copy_security(security_descriptor)
    }

    fn overwrite(
        &self,
        context: &WinFile,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        self.drive.set_size(&context.file, 0).map_err(status)?;
        self.fill(file_info, &context.file.attr());
        Ok(())
    }

    fn read_directory(
        &self,
        context: &WinFile,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        let path = context.file.path();
        if let Ok(lock) = context.dir_buffer.acquire(marker.is_none(), None) {
            let mut entries = self.drive.list(&path).map_err(status)?;
            if !path.is_root() {
                entries.push(Attr {
                    name: ".".into(),
                    is_dir: true,
                    size: 0,
                });
                entries.push(Attr {
                    name: "..".into(),
                    is_dir: true,
                    size: 0,
                });
            }
            let mut info: DirInfo = DirInfo::new();
            for e in entries {
                info.reset();
                if info.set_name(&e.name).is_err() {
                    log::warn!("{path}: name too long to show: {}", e.name);
                    continue;
                }
                self.fill(info.file_info_mut(), &e);
                lock.write(&mut info)?;
            }
        }
        Ok(context.dir_buffer.read(marker, buffer))
    }

    fn rename(
        &self,
        context: &WinFile,
        _file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        let to = parse(new_file_name)?;
        self.drive
            .rename(&context.file, &to, replace_if_exists)
            .map_err(status)
    }

    fn set_basic_info(
        &self,
        context: &WinFile,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        // The device keeps no attributes or times; accept and ignore them, as copies set them.
        self.fill(file_info, &context.file.attr());
        Ok(())
    }

    fn set_delete(
        &self,
        context: &WinFile,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> winfsp::Result<()> {
        if delete_file {
            self.drive.check_delete(&context.file).map_err(status)?;
        }
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &WinFile,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        // Allocation size only matters when it cuts the file short.
        if !set_allocation_size || new_size < context.file.size() {
            self.drive
                .set_size(&context.file, new_size)
                .map_err(status)?;
        }
        self.fill(file_info, &context.file.attr());
        Ok(())
    }

    fn read(&self, context: &WinFile, buffer: &mut [u8], offset: u64) -> winfsp::Result<u32> {
        if offset >= context.file.size() {
            return Err(STATUS_END_OF_FILE.into());
        }
        let n = self
            .drive
            .read(&context.file, offset, buffer)
            .map_err(status)?;
        Ok(n as u32)
    }

    fn write(
        &self,
        context: &WinFile,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        let offset = if write_to_eof { None } else { Some(offset) };
        let n = self
            .drive
            .write(&context.file, offset, buffer, constrained_io)
            .map_err(status)?;
        self.fill(file_info, &context.file.attr());
        Ok(n as u32)
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        let (total, free) = self.drive.space();
        out_volume_info.total_size = total;
        out_volume_info.free_size = free;
        out_volume_info.set_volume_label(VOLUME_LABEL);
        Ok(())
    }
}

/// Load the installed WinFsp DLL so its delay-loaded imports resolve.
fn load_winfsp() -> anyhow::Result<()> {
    let mut dir = [0u16; 260];
    let mut size = (dir.len() * 2) as u32;
    // SAFETY: the buffer and its size in bytes are passed together.
    let found = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\WOW6432Node\\WinFsp"),
            w!("InstallDir"),
            RRF_RT_REG_SZ,
            None,
            Some(dir.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if found.is_err() {
        bail!(
            "WinFsp is not installed (get it from https://winfsp.dev or `winget install WinFsp.WinFsp`)"
        );
    }
    let dir = String::from_utf16_lossy(&dir[..(size as usize / 2).saturating_sub(1)]);
    let dll = format!(
        "{}\\bin\\{}",
        dir.trim_end_matches('\\'),
        if cfg!(target_arch = "aarch64") {
            "winfsp-a64.dll"
        } else {
            "winfsp-x64.dll"
        }
    );
    // SAFETY: loading a library by full path.
    unsafe { LoadLibraryW(&HSTRING::from(dll.as_str())) }
        .with_context(|| format!("loading {dll}"))?;
    winfsp::winfsp_init().map_err(|e| anyhow!("initialising WinFsp: {e:?}"))?;
    Ok(())
}

/// Mount `drive` at `mount_point` and serve it until `stop` fires.
pub fn run(drive: Arc<Drive>, mount_point: &str, stop: Receiver<String>) -> anyhow::Result<()> {
    load_winfsp()?;
    let mut params = VolumeParams::new();
    params
        .filesystem_name("SP404")
        .sector_size(512)
        .sectors_per_allocation_unit((ALLOCATION_UNIT / 512) as u16)
        .max_component_length(255)
        .volume_creation_time(filetime(drive.mounted_at()))
        .volume_serial_number(0x0404_2021)
        // Kernel caches file information for a second; data is never cached, so reads
        // always reflect the device.
        .file_info_timeout(1000)
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .persistent_acls(true)
        .post_cleanup_when_modified_only(true)
        .read_only_volume(drive.read_only());

    let fs = WinFs::new(drive.clone())?;
    // Fine-grained locking: operations run concurrently; the drive and the device link
    // serialise what must be.
    let mut host: FileSystemHost<WinFs, FineGuard> =
        FileSystemHost::new(params, fs).map_err(|e| anyhow!("creating the file system: {e}"))?;
    host.mount(mount_point)
        .map_err(|e| anyhow!("mounting at {mount_point} (is it already in use?): {e}"))?;
    host.start()
        .map_err(|e| anyhow!("starting the file system: {e}"))?;
    log::info!(
        "SP-404MKII mounted at {mount_point}{}; `sp404drive unmount {mount_point}` or Ctrl+C to unmount",
        if drive.read_only() {
            " (read-only)"
        } else {
            ""
        }
    );

    let reason = stop.recv().unwrap_or_else(|_| "stopping".into());
    log::info!("{reason}: unmounting {mount_point}");
    host.unmount();
    host.stop();
    drive.shutdown();
    drop(host);
    log::info!("unmounted");
    Ok(())
}

fn event_name(mount_point: &str) -> HSTRING {
    let tag: String = mount_point
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    HSTRING::from(format!("Local\\sp404drive-unmount-{tag}"))
}

/// Wait in the background for `sp404drive unmount` aimed at `mount_point`, then call
/// `on_signal`.
pub fn listen_for_unmount(
    mount_point: &str,
    on_signal: impl FnOnce() + Send + 'static,
) -> anyhow::Result<()> {
    let name = event_name(mount_point);
    // SAFETY: plain named-event creation; the handle is closed after the wait.
    let event = unsafe { CreateEventW(None, true, false, &name)? };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(event)? };
        bail!("another sp404drive already serves {mount_point}");
    }
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

/// Ask the `sp404drive mount` process serving `mount_point` to unmount.
pub fn signal_unmount(mount_point: &str) -> anyhow::Result<()> {
    let name = event_name(mount_point);
    // SAFETY: open, signal and close a named event.
    unsafe {
        let event = OpenEventW(EVENT_MODIFY_STATE, false, &name)
            .map_err(|_| anyhow!("nothing is mounted at {mount_point} by sp404drive"))?;
        let result = SetEvent(event);
        let _ = CloseHandle(event);
        result?;
    }
    Ok(())
}
