//! The drive's filesystem, independent of any OS: a root holding two folders, `Internal`
//! (the device's own storage) and `SD-Card` (the card in its slot), with everything below
//! them served from the device.
//!
//! Platform front ends (WinFsp on Windows; FUSE on macOS) translate their callbacks into
//! the calls here. This layer owns the things every front end needs:
//!
//! - mapping drive paths to device paths;
//! - caching directory listings, because each listing costs one device request per file;
//! - pooling device read handles;
//! - buffering writes. The device only has "create or replace a whole file" as a safe
//!   write, so a file being written is held in memory and uploaded when it is flushed or
//!   closed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime};

use crate::store::{Store, StoreError};

pub const INTERNAL: &str = "Internal";
pub const SD_CARD: &str = "SD-Card";

/// Device read handles kept open at once. The device's limit is unknown; stay small.
const MAX_HANDLES: usize = 8;
/// How long the free-space figure is reused.
const FREE_SPACE_TTL: Duration = Duration::from_secs(10);
/// Reported when the device gives no free-space figure, so copies are not refused.
const UNKNOWN_FREE: u64 = 64 << 30;
/// The internal storage's published size. The device reports only its free space, so used
/// space is this minus that: files, file system structures and any reserved area.
const INTERNAL_TOTAL: u64 = 16_000_000_000;
/// Longest path the device's rename request can carry (a 200-byte slot with its NUL).
const MAX_REMOTE_PATH: usize = 199;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Volume {
    Internal,
    Card,
}

impl Volume {
    pub fn name(self) -> &'static str {
        match self {
            Volume::Internal => INTERNAL,
            Volume::Card => SD_CARD,
        }
    }

    fn device_root(self) -> &'static str {
        match self {
            Volume::Internal => "",
            Volume::Card => "SD:",
        }
    }

    /// Length of the prefix the device path gets on the wire (see SparkyMK2 `remote_path`).
    fn wire_prefix_len(self) -> usize {
        match self {
            Volume::Internal => "/SP404REMOTE///".len(),
            Volume::Card => "/".len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FsError {
    #[error("not found")]
    NotFound,
    #[error("not a directory")]
    NotADirectory,
    #[error("is a directory")]
    IsADirectory,
    #[error("already exists")]
    Exists,
    #[error("directory not empty")]
    NotEmpty,
    #[error("not allowed")]
    AccessDenied,
    #[error("cannot move between Internal and SD-Card")]
    CrossVolume,
    #[error("invalid or too long name")]
    InvalidName,
    #[error("drive is read-only")]
    ReadOnly,
    #[error("device refused: {0}")]
    Refused(String),
    #[error("device disconnected")]
    Disconnected,
    #[error("I/O error: {0}")]
    Io(String),
}

pub type FsResult<T> = Result<T, FsError>;

/// A path on the drive: the root, one of the volume folders, or something inside one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VPath {
    volume: Option<Volume>,
    parts: Vec<String>,
}

impl VPath {
    pub fn root() -> Self {
        VPath {
            volume: None,
            parts: Vec::new(),
        }
    }

    /// Parse a drive path such as `\Internal\ROLAND` or `/SD-Card/IMPORT`. Volume names
    /// match case-insensitively.
    pub fn parse(path: &str) -> FsResult<Self> {
        let mut comps = path.split(['/', '\\']).filter(|c| !c.is_empty());
        let Some(first) = comps.next() else {
            return Ok(Self::root());
        };
        let volume = if first.eq_ignore_ascii_case(INTERNAL) {
            Volume::Internal
        } else if first.eq_ignore_ascii_case(SD_CARD) {
            Volume::Card
        } else {
            return Err(FsError::NotFound);
        };
        let mut parts = Vec::new();
        for c in comps {
            if c == "." || c == ".." || c.contains(':') {
                return Err(FsError::InvalidName);
            }
            parts.push(c.to_string());
        }
        Ok(VPath {
            volume: Some(volume),
            parts,
        })
    }

    pub fn is_root(&self) -> bool {
        self.volume.is_none()
    }

    pub fn is_volume_root(&self) -> bool {
        self.volume.is_some() && self.parts.is_empty()
    }

    /// The root and the two volume folders, which cannot be changed.
    pub fn is_fixed(&self) -> bool {
        self.parts.is_empty()
    }

    pub fn volume(&self) -> Option<Volume> {
        self.volume
    }

    /// Last component; the volume name for a volume root, `""` for the root.
    pub fn name(&self) -> &str {
        match (self.parts.last(), self.volume) {
            (Some(p), _) => p,
            (None, Some(v)) => v.name(),
            (None, None) => "",
        }
    }

    pub fn parent(&self) -> Option<VPath> {
        if self.is_root() {
            return None;
        }
        if self.parts.is_empty() {
            return Some(Self::root());
        }
        let mut p = self.clone();
        p.parts.pop();
        Some(p)
    }

    pub fn child(&self, name: &str) -> VPath {
        let mut p = self.clone();
        p.parts.push(name.to_string());
        p
    }

    /// SparkyMK2 device path: `""`/`"SD:"` for the volume roots, `ROLAND/SP-404MKII`,
    /// `SD:IMPORT/pack`.
    pub fn device_path(&self) -> String {
        match self.volume {
            Some(v) => format!("{}{}", v.device_root(), self.parts.join("/")),
            None => String::new(),
        }
    }

    /// Whether `self` is `prefix` or lies inside it.
    pub fn starts_with(&self, prefix: &VPath) -> bool {
        self.volume == prefix.volume
            && self.parts.len() >= prefix.parts.len()
            && self
                .parts
                .iter()
                .zip(&prefix.parts)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
    }

    /// `self` with the leading `from` replaced by `to`. `self` must start with `from`.
    pub fn rebase(&self, from: &VPath, to: &VPath) -> VPath {
        let mut parts = to.parts.clone();
        parts.extend_from_slice(&self.parts[from.parts.len()..]);
        VPath {
            volume: to.volume,
            parts,
        }
    }

    fn fits_on_wire(&self) -> bool {
        match self.volume {
            Some(v) => v.wire_prefix_len() + self.parts.join("/").len() <= MAX_REMOTE_PATH,
            None => true,
        }
    }
}

impl std::fmt::Display for VPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.volume {
            None => write!(f, "\\"),
            Some(v) => {
                write!(f, "\\{}", v.name())?;
                for p in &self.parts {
                    write!(f, "\\{p}")?;
                }
                Ok(())
            }
        }
    }
}

/// What the drive knows about a file or folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    /// The name as the device spells it.
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

impl Attr {
    fn dir(name: &str) -> Self {
        Attr {
            name: name.to_string(),
            is_dir: true,
            size: 0,
        }
    }
}

/// An open file or folder. Front ends keep one per OS handle.
pub struct OpenFile {
    inner: Arc<FileInner>,
}

/// One file or folder in use, shared by every handle open on it, so they all see the
/// same size and contents.
struct FileInner {
    id: u64,
    is_dir: bool,
    path: Mutex<VPath>,
    state: Mutex<FileState>,
}

struct FileState {
    size: u64,
    /// The whole file, while it is being written.
    buffer: Option<Vec<u8>>,
    /// The buffer differs from the device.
    dirty: bool,
}

impl OpenFile {
    pub fn is_dir(&self) -> bool {
        self.inner.is_dir
    }

    pub fn path(&self) -> VPath {
        self.inner.path.lock().unwrap().clone()
    }

    pub fn size(&self) -> u64 {
        self.inner.state.lock().unwrap().size
    }

    pub fn attr(&self) -> Attr {
        Attr {
            name: self.path().name().to_string(),
            is_dir: self.inner.is_dir,
            size: self.size(),
        }
    }
}

/// Key of a file in use. The drive path, not the device path: the root and `Internal`
/// share the device path `""`.
fn file_key(path: &VPath) -> String {
    path.to_string().to_lowercase()
}

struct CachedDir {
    at: Instant,
    entries: Vec<Attr>,
}

struct PooledHandle {
    file_id: u64,
    /// Lowercased device path of the file, to find handles that block a change to it.
    path: String,
    handle: i32,
    last_used: u64,
}

#[derive(Default)]
struct HandlePool {
    handles: Vec<PooledHandle>,
    tick: u64,
}

pub struct Options {
    pub read_only: bool,
    /// How long a directory listing is trusted before asking the device again.
    pub cache_ttl: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            read_only: false,
            cache_ttl: Duration::from_secs(5),
        }
    }
}

pub struct Drive {
    store: Arc<dyn Store>,
    opts: Options,
    mounted_at: SystemTime,
    /// Listings by lowercased device path (the device's filesystems ignore case).
    dirs: Mutex<HashMap<String, CachedDir>>,
    pool: Mutex<HandlePool>,
    /// Files in use, by [`file_key`].
    files: Mutex<HashMap<String, Weak<FileInner>>>,
    next_id: AtomicU64,
    free: Mutex<Option<(Instant, u64)>>,
    disconnected: AtomicBool,
}

impl Drive {
    pub fn new(store: Arc<dyn Store>, opts: Options) -> Self {
        Drive {
            store,
            opts,
            mounted_at: SystemTime::now(),
            dirs: Mutex::new(HashMap::new()),
            pool: Mutex::new(HandlePool::default()),
            files: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            free: Mutex::new(None),
            disconnected: AtomicBool::new(false),
        }
    }

    pub fn read_only(&self) -> bool {
        self.opts.read_only
    }

    /// The device reports no timestamps; everything carries the mount time.
    pub fn mounted_at(&self) -> SystemTime {
        self.mounted_at
    }

    /// The device has gone away; the front end should unmount.
    pub fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::Relaxed) || !self.store.is_connected()
    }

    fn err(&self, e: StoreError) -> FsError {
        match e {
            StoreError::Refused(what) => FsError::Refused(what),
            StoreError::Disconnected => {
                if !self.disconnected.swap(true, Ordering::Relaxed) {
                    log::warn!("the SP-404MKII stopped answering");
                }
                FsError::Disconnected
            }
            StoreError::Other(msg) => FsError::Io(msg),
        }
    }

    /// Fail at once, rather than wait on a dead link, once the device is gone.
    fn online(&self) -> FsResult<()> {
        if self.is_disconnected() {
            Err(FsError::Disconnected)
        } else {
            Ok(())
        }
    }

    fn writable(&self) -> FsResult<()> {
        if self.opts.read_only {
            Err(FsError::ReadOnly)
        } else {
            Ok(())
        }
    }

    // ---- directory listings ------------------------------------------------------------

    fn cached_listing(&self, dir: &VPath) -> FsResult<Vec<Attr>> {
        let key = dir.device_path().to_lowercase();
        if let Some(c) = self.dirs.lock().unwrap().get(&key)
            && c.at.elapsed() < self.opts.cache_ttl
        {
            return Ok(c.entries.clone());
        }
        self.online()?;
        let raw = self
            .store
            .list(&dir.device_path())
            .map_err(|e| self.err(e))?;
        let entries: Vec<Attr> = raw
            .into_iter()
            .map(|e| Attr {
                name: e.name,
                is_dir: e.is_dir,
                size: e.size,
            })
            .collect();
        log::debug!("listed {dir}: {} entries", entries.len());
        self.dirs.lock().unwrap().insert(
            key,
            CachedDir {
                at: Instant::now(),
                entries: entries.clone(),
            },
        );
        Ok(entries)
    }

    /// Update a cached listing in place after a change made through the drive.
    fn cache_put(&self, dir: &VPath, attr: Attr) {
        let key = dir.device_path().to_lowercase();
        if let Some(c) = self.dirs.lock().unwrap().get_mut(&key) {
            c.entries
                .retain(|e| !e.name.eq_ignore_ascii_case(&attr.name));
            c.entries.push(attr);
        }
    }

    fn cache_remove(&self, dir: &VPath, name: &str) {
        let key = dir.device_path().to_lowercase();
        if let Some(c) = self.dirs.lock().unwrap().get_mut(&key) {
            c.entries.retain(|e| !e.name.eq_ignore_ascii_case(name));
        }
    }

    /// Forget a directory and everything cached below it.
    fn cache_forget_tree(&self, dir: &VPath) {
        let key = dir.device_path().to_lowercase();
        let below = format!("{key}/");
        self.dirs
            .lock()
            .unwrap()
            .retain(|k, _| *k != key && !k.starts_with(&below));
    }

    /// Drop every cached listing, e.g. when the user asks for a refresh.
    pub fn forget_cache(&self) {
        self.dirs.lock().unwrap().clear();
    }

    /// A path as the device spells it, and its attributes. Windows matches names without
    /// regard to case; whether the device does is unknown, so every component is matched
    /// against its parent's listing and the device's spelling is used from then on.
    fn resolve(&self, path: &VPath) -> FsResult<(VPath, Attr)> {
        if path.is_root() {
            return Ok((path.clone(), Attr::dir("")));
        }
        if path.is_volume_root() {
            return Ok((path.clone(), Attr::dir(path.name())));
        }
        let parent = self.resolve_dir(&path.parent().expect("not the root"))?;
        let name = path.name();
        let attr = self
            .cached_listing(&parent)?
            .into_iter()
            .find(|e| e.name.eq_ignore_ascii_case(name))
            .ok_or(FsError::NotFound)?;
        Ok((parent.child(&attr.name), attr))
    }

    /// A folder's path as the device spells it.
    fn resolve_dir(&self, path: &VPath) -> FsResult<VPath> {
        let (path, attr) = self.resolve(path)?;
        if !attr.is_dir {
            return Err(FsError::NotADirectory);
        }
        Ok(path)
    }

    /// Attributes of a path.
    pub fn lookup(&self, path: &VPath) -> FsResult<Attr> {
        self.resolve(path).map(|(_, attr)| attr)
    }

    /// Contents of a folder.
    pub fn list(&self, path: &VPath) -> FsResult<Vec<Attr>> {
        if path.is_root() {
            return Ok(vec![Attr::dir(INTERNAL), Attr::dir(SD_CARD)]);
        }
        let dir = self.resolve_dir(path)?;
        self.cached_listing(&dir)
    }

    // ---- opening and creating ----------------------------------------------------------

    /// A handle on `path`, sharing the file's state with any other handle already open.
    fn new_file(&self, path: VPath, attr: &Attr) -> OpenFile {
        let key = file_key(&path);
        let mut files = self.files.lock().unwrap();
        if let Some(inner) = files.get(&key).and_then(Weak::upgrade)
            && inner.is_dir == attr.is_dir
        {
            return OpenFile { inner };
        }
        let inner = Arc::new(FileInner {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            is_dir: attr.is_dir,
            path: Mutex::new(path),
            state: Mutex::new(FileState {
                size: attr.size,
                buffer: None,
                dirty: false,
            }),
        });
        files.insert(key, Arc::downgrade(&inner));
        OpenFile { inner }
    }

    pub fn open(&self, path: &VPath) -> FsResult<OpenFile> {
        let (path, attr) = self.resolve(path)?;
        Ok(self.new_file(path, &attr))
    }

    /// Create a file or folder. A new file is created empty on the device at once, so it
    /// exists for anyone who looks before the first flush.
    pub fn create(&self, path: &VPath, is_dir: bool) -> FsResult<OpenFile> {
        self.writable()?;
        self.online()?;
        if path.is_fixed() {
            return Err(FsError::AccessDenied);
        }
        if !path.fits_on_wire() {
            return Err(FsError::InvalidName);
        }
        let parent = self.resolve_dir(&path.parent().expect("not the root"))?;
        let path = parent.child(path.name());
        match self.lookup(&path) {
            Ok(_) => return Err(FsError::Exists),
            Err(FsError::NotFound) => {}
            Err(e) => return Err(e),
        }
        let dev = path.device_path();
        let result = if is_dir {
            self.store.mkdir(&dev)
        } else {
            self.store.write_file(&dev, &[])
        };
        result.map_err(|e| self.err(e))?;
        log::info!("created {}{path}", if is_dir { "folder " } else { "" });
        let attr = Attr {
            name: path.name().to_string(),
            is_dir,
            size: 0,
        };
        self.cache_put(&parent, attr.clone());
        Ok(self.new_file(path, &attr))
    }

    // ---- reading -----------------------------------------------------------------------

    /// Run `f` with a device read handle for `file`, opening one if needed.
    ///
    /// The device closes every open handle after a couple of seconds without requests,
    /// without saying so, and hands the numbers out again (issue #1). So a pooled handle
    /// may be dead: a refusal through one means reopen and try once more, and a number the
    /// device hands out again retires whatever pooled entry still held it.
    fn with_handle<T>(
        &self,
        file: &OpenFile,
        f: impl Fn(i32) -> Result<T, StoreError>,
    ) -> FsResult<T> {
        self.online()?;
        // The pool stays locked for the read, so a handle is never evicted mid-read. The
        // device serves one request at a time anyway.
        let mut pool = self.pool.lock().unwrap();
        pool.tick += 1;
        let tick = pool.tick;
        let id = file.inner.id;
        let path = file.path().device_path();
        if let Some(h) = pool.handles.iter_mut().find(|h| h.file_id == id) {
            h.last_used = tick;
            let handle = h.handle;
            match f(handle) {
                Err(StoreError::Refused(what)) => {
                    log::debug!("{path}: handle {handle} refused {what}, reopening");
                    pool.handles.retain(|h| h.file_id != id);
                    self.store.close(handle);
                }
                result => return self.handle_result(&mut pool, id, handle, result),
            }
        }
        if pool.handles.len() >= MAX_HANDLES
            && let Some(oldest) = pool
                .handles
                .iter()
                .enumerate()
                .min_by_key(|(_, h)| h.last_used)
                .map(|(i, _)| i)
        {
            let old = pool.handles.swap_remove(oldest);
            self.store.close(old.handle);
        }
        let handle = self.store.open(&path).map_err(|e| self.err(e))?;
        // The device only reuses a number it has closed, so an entry holding it is dead.
        // Don't close it: the number now belongs to this file.
        pool.handles.retain(|h| {
            if h.handle == handle {
                log::debug!("handle {handle} for {} was closed by the device", h.path);
            }
            h.handle != handle
        });
        pool.handles.push(PooledHandle {
            file_id: id,
            path: path.to_lowercase(),
            handle,
            last_used: tick,
        });
        let result = f(handle);
        self.handle_result(&mut pool, id, handle, result)
    }

    fn handle_result<T>(
        &self,
        pool: &mut HandlePool,
        id: u64,
        handle: i32,
        result: Result<T, StoreError>,
    ) -> FsResult<T> {
        if result.is_err() {
            // Don't reuse a handle that just failed.
            pool.handles.retain(|h| h.file_id != id);
            self.store.close(handle);
        }
        result.map_err(|e| self.err(e))
    }

    fn drop_handle(&self, file_id: u64) {
        let mut pool = self.pool.lock().unwrap();
        if let Some(i) = pool.handles.iter().position(|h| h.file_id == file_id) {
            let h = pool.handles.swap_remove(i);
            self.store.close(h.handle);
        }
    }

    /// Close every read handle on `path` or below it. The device refuses to replace,
    /// delete or rename a file it has open; readers simply reopen on their next read.
    fn release_handles(&self, path: &VPath) {
        let key = path.device_path().to_lowercase();
        let below = format!("{key}/");
        let mut pool = self.pool.lock().unwrap();
        let (blocking, keep): (Vec<_>, Vec<_>) = pool
            .handles
            .drain(..)
            .partition(|h| h.path == key || h.path.starts_with(&below));
        pool.handles = keep;
        for h in blocking {
            self.store.close(h.handle);
        }
    }

    /// Read into `buf` at `offset`; returns the byte count, 0 at end of file.
    pub fn read(&self, file: &OpenFile, offset: u64, buf: &mut [u8]) -> FsResult<usize> {
        if file.inner.is_dir {
            return Err(FsError::IsADirectory);
        }
        let size = {
            let state = file.inner.state.lock().unwrap();
            if let Some(data) = &state.buffer {
                let start = (offset as usize).min(data.len());
                let n = buf.len().min(data.len() - start);
                buf[..n].copy_from_slice(&data[start..start + n]);
                return Ok(n);
            }
            state.size
        };
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let len = buf.len().min((size - offset) as usize);
        let data = self.with_handle(file, |h| self.store.read_at(h, offset, len))?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }

    // ---- writing -----------------------------------------------------------------------

    /// Make sure the whole file is in memory, ready to change.
    fn load(&self, file: &OpenFile, state: &mut FileState) -> FsResult<()> {
        if state.buffer.is_some() {
            return Ok(());
        }
        let mut data = Vec::new();
        if state.size > 0 {
            let size = state.size as usize;
            data = self.with_handle(file, |h| self.store.read_at(h, 0, size))?;
            if data.len() != size {
                log::warn!(
                    "{}: expected {size} bytes, read {}",
                    file.path(),
                    data.len()
                );
            }
        }
        state.size = data.len() as u64;
        state.buffer = Some(data);
        Ok(())
    }

    /// Write `data` at `offset` (`None`: at the end). With `constrained`, the file does
    /// not grow. Returns the byte count written.
    pub fn write(
        &self,
        file: &OpenFile,
        offset: Option<u64>,
        data: &[u8],
        constrained: bool,
    ) -> FsResult<usize> {
        self.writable()?;
        if file.inner.is_dir {
            return Err(FsError::IsADirectory);
        }
        let mut state = file.inner.state.lock().unwrap();
        self.load(file, &mut state)?;
        let size = state.size as usize;
        let offset = offset.map_or(size, |o| o as usize);
        let data = if constrained {
            if offset >= size {
                return Ok(0);
            }
            &data[..data.len().min(size - offset)]
        } else {
            data
        };
        let buf = state.buffer.as_mut().expect("loaded");
        if buf.len() < offset + data.len() {
            buf.resize(offset + data.len(), 0);
        }
        buf[offset..offset + data.len()].copy_from_slice(data);
        state.size = buf.len() as u64;
        state.dirty = true;
        Ok(data.len())
    }

    /// Change a file's length, padding with zeros.
    pub fn set_size(&self, file: &OpenFile, size: u64) -> FsResult<()> {
        self.writable()?;
        if file.inner.is_dir {
            return Err(FsError::IsADirectory);
        }
        let mut state = file.inner.state.lock().unwrap();
        if state.size == size && state.buffer.is_some() {
            return Ok(());
        }
        if size == 0 {
            state.buffer = Some(Vec::new());
        } else {
            self.load(file, &mut state)?;
            state
                .buffer
                .as_mut()
                .expect("loaded")
                .resize(size as usize, 0);
        }
        state.size = size;
        state.dirty = true;
        Ok(())
    }

    /// Upload a file's pending changes.
    pub fn flush(&self, file: &OpenFile) -> FsResult<()> {
        let mut state = file.inner.state.lock().unwrap();
        if !state.dirty {
            return Ok(());
        }
        self.online()?;
        let path = file.path();
        let data = state.buffer.as_deref().unwrap_or_default();
        self.release_handles(&path);
        let started = Instant::now();
        self.store
            .write_file(&path.device_path(), data)
            .map_err(|e| self.err(e))?;
        log::info!(
            "wrote {path} ({} bytes, {} ms)",
            data.len(),
            started.elapsed().as_millis()
        );
        state.dirty = false;
        // Keep memory down; later reads go back to the device.
        state.buffer = None;
        let size = state.size;
        drop(state);
        if let Some(parent) = path.parent() {
            self.cache_put(
                &parent,
                Attr {
                    name: path.name().to_string(),
                    is_dir: false,
                    size,
                },
            );
        }
        Ok(())
    }

    // ---- deleting and renaming ---------------------------------------------------------

    /// Whether `file` may be deleted: not one of the fixed folders, and empty if a folder.
    pub fn check_delete(&self, file: &OpenFile) -> FsResult<()> {
        self.writable()?;
        let path = file.path();
        if path.is_fixed() {
            return Err(FsError::AccessDenied);
        }
        if file.inner.is_dir {
            // Ask the device afresh: deleting on stale information is worse than a request.
            self.cache_forget_tree(&path);
            if !self.cached_listing(&path)?.is_empty() {
                return Err(FsError::NotEmpty);
            }
        }
        Ok(())
    }

    pub fn delete(&self, file: &OpenFile) -> FsResult<()> {
        self.writable()?;
        self.online()?;
        let path = file.path();
        if path.is_fixed() {
            return Err(FsError::AccessDenied);
        }
        self.release_handles(&path);
        let dev = path.device_path();
        let result = if file.inner.is_dir {
            self.store.rmdir(&dev)
        } else {
            self.store.unlink(&dev)
        };
        result.map_err(|e| self.err(e))?;
        log::info!("deleted {path}");
        self.files.lock().unwrap().remove(&file_key(&path));
        {
            // Nothing left to upload.
            let mut state = file.inner.state.lock().unwrap();
            state.dirty = false;
            state.buffer = None;
        }
        if let Some(parent) = path.parent() {
            self.cache_remove(&parent, path.name());
        }
        if file.inner.is_dir {
            self.cache_forget_tree(&path);
        }
        Ok(())
    }

    /// Rename or move `file` to `to`, within the same volume.
    pub fn rename(&self, file: &OpenFile, to: &VPath, replace: bool) -> FsResult<()> {
        self.writable()?;
        self.online()?;
        let from = file.path();
        if from.is_fixed() || to.is_fixed() {
            return Err(FsError::AccessDenied);
        }
        if from.volume() != to.volume() {
            return Err(FsError::CrossVolume);
        }
        if !to.fits_on_wire() || !from.fits_on_wire() {
            return Err(FsError::InvalidName);
        }
        let to_parent = self.resolve_dir(&to.parent().expect("not fixed"))?;
        let to = &to_parent.child(to.name());
        let case_only = from.device_path().eq_ignore_ascii_case(&to.device_path());
        if !case_only {
            match self.lookup(to) {
                Ok(existing) => {
                    if !replace || existing.is_dir || file.inner.is_dir {
                        return Err(FsError::Exists);
                    }
                    self.release_handles(to);
                    self.store
                        .unlink(&to.device_path())
                        .map_err(|e| self.err(e))?;
                    self.cache_remove(&to_parent, to.name());
                }
                Err(FsError::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        self.release_handles(&from);
        self.store
            .rename(&from.device_path(), &to.device_path())
            .map_err(|e| self.err(e))?;
        log::info!("renamed {from} to {to}");
        let attr = Attr {
            name: to.name().to_string(),
            is_dir: file.inner.is_dir,
            size: file.size(),
        };
        self.cache_remove(&from.parent().expect("not fixed"), from.name());
        if file.inner.is_dir {
            self.cache_forget_tree(&from);
        }
        self.cache_put(&to_parent, attr);
        self.move_files(&from, to);
        Ok(())
    }

    /// After a rename, point the file (and, for a folder, everything open inside it) at
    /// its new path.
    fn move_files(&self, from: &VPath, to: &VPath) {
        let mut files = self.files.lock().unwrap();
        let moved: Vec<Arc<FileInner>> = files
            .values()
            .filter_map(Weak::upgrade)
            .filter(|f| f.path.lock().unwrap().starts_with(from))
            .collect();
        files.retain(|_, w| w.strong_count() > 0);
        for f in moved {
            let mut path = f.path.lock().unwrap();
            let old_key = file_key(&path);
            *path = path.rebase(from, to);
            files.remove(&old_key);
            files.insert(file_key(&path), Arc::downgrade(&f));
        }
    }

    // ---- closing -----------------------------------------------------------------------

    /// The OS is done with `file`: upload anything pending and let its handle go.
    pub fn release(&self, file: OpenFile) {
        if let Err(e) = self.flush(&file) {
            log::error!("could not save {}: {e}", file.path());
        }
        let mut files = self.files.lock().unwrap();
        // The last handle on the file: let its device handle go too.
        if Arc::strong_count(&file.inner) == 1 {
            self.drop_handle(file.inner.id);
            let key = file_key(&file.path());
            if files
                .get(&key)
                .is_some_and(|w| std::ptr::eq(w.as_ptr(), Arc::as_ptr(&file.inner)))
            {
                files.remove(&key);
            }
        }
    }

    /// Unmounting: save every open file with changes, then release the device.
    pub fn shutdown(&self) {
        let open: Vec<Arc<FileInner>> = self
            .files
            .lock()
            .unwrap()
            .drain()
            .filter_map(|(_, w)| w.upgrade())
            .collect();
        for inner in open {
            let file = OpenFile { inner };
            if let Err(e) = self.flush(&file) {
                log::error!("could not save {}: {e}", file.path());
            }
        }
        // Pooled read handles are dropped, not closed: the device closes them itself once
        // the session lapses, and each close would only extend it.
        self.pool.lock().unwrap().handles.clear();
        if !self.is_disconnected() {
            self.store.goodbye();
        }
    }

    // ---- the volume --------------------------------------------------------------------

    /// (total, free) bytes of the internal storage: its published size, and the free space
    /// the device reports. The card reports neither.
    pub fn space(&self) -> (u64, u64) {
        let mut cached = self.free.lock().unwrap();
        let free = match *cached {
            Some((at, free)) if at.elapsed() < FREE_SPACE_TTL => free,
            _ => {
                let free = self.store.free_bytes().unwrap_or(UNKNOWN_FREE);
                *cached = Some((Instant::now(), free));
                free
            }
        };
        (INTERNAL_TOTAL.max(free), free)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::mem::MemStore;

    fn drive(store: &Arc<MemStore>) -> Drive {
        Drive::new(store.clone(), Options::default())
    }

    fn sample() -> Arc<MemStore> {
        Arc::new(MemStore::new(&[
            ("ROLAND", None),
            ("ROLAND/SP-404MKII", None),
            ("QSPI.bin", Some(b"0123456789")),
            ("SD:IMPORT", None),
            ("SD:IMPORT/kick.wav", Some(b"RIFF....")),
        ]))
    }

    fn p(s: &str) -> VPath {
        VPath::parse(s).unwrap()
    }

    #[test]
    fn maps_paths_to_device_paths() {
        assert!(p("\\").is_root());
        assert_eq!(p("\\Internal").device_path(), "");
        assert_eq!(p("/sd-card").device_path(), "SD:");
        assert_eq!(
            p("\\Internal\\ROLAND\\SP-404MKII").device_path(),
            "ROLAND/SP-404MKII"
        );
        assert_eq!(
            p("\\SD-Card\\IMPORT\\a b.wav").device_path(),
            "SD:IMPORT/a b.wav"
        );
        assert_eq!(VPath::parse("\\Elsewhere"), Err(FsError::NotFound));
        assert_eq!(VPath::parse("\\Internal\\a:b"), Err(FsError::InvalidName));
        assert_eq!(p("\\SD-Card\\IMPORT").to_string(), "\\SD-Card\\IMPORT");
        assert_eq!(p("\\SD-Card\\IMPORT").parent(), Some(p("\\SD-Card")));
        assert_eq!(p("\\SD-Card").parent(), Some(VPath::root()));
    }

    #[test]
    fn lists_root_volumes_and_contents() {
        let s = sample();
        let d = drive(&s);
        let names: Vec<String> = d
            .list(&VPath::root())
            .unwrap()
            .into_iter()
            .map(|a| a.name)
            .collect();
        assert_eq!(names, ["Internal", "SD-Card"]);
        let internal = d.list(&p("\\Internal")).unwrap();
        assert!(internal.contains(&Attr {
            name: "QSPI.bin".into(),
            is_dir: false,
            size: 10
        }));
        assert!(internal.iter().any(|a| a.name == "ROLAND" && a.is_dir));
        assert_eq!(
            d.lookup(&p("\\SD-Card\\import\\KICK.WAV")).unwrap().name,
            "kick.wav"
        );
        assert_eq!(
            d.lookup(&p("\\SD-Card\\desktop.ini")),
            Err(FsError::NotFound)
        );
        assert_eq!(
            d.list(&p("\\Internal\\QSPI.bin")),
            Err(FsError::NotADirectory)
        );
    }

    #[test]
    fn listings_are_cached() {
        let s = sample();
        let d = drive(&s);
        for _ in 0..3 {
            d.lookup(&p("\\Internal\\QSPI.bin")).unwrap();
            d.lookup(&p("\\Internal\\desktop.ini")).unwrap_err();
        }
        let lists = s
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.starts_with("list"))
            .count();
        assert_eq!(lists, 1);
    }

    #[test]
    fn reads_at_offsets() {
        let s = sample();
        let d = drive(&s);
        let f = d.open(&p("\\Internal\\QSPI.bin")).unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(d.read(&f, 3, &mut buf).unwrap(), 4);
        assert_eq!(&buf, b"3456");
        assert_eq!(d.read(&f, 8, &mut buf).unwrap(), 2);
        assert_eq!(d.read(&f, 10, &mut buf).unwrap(), 0);
        d.release(f);
    }

    #[test]
    fn survives_the_device_closing_idle_handles() {
        // The device closes every open handle after a couple of seconds without requests,
        // then hands the same numbers to the next files opened (issue #1).
        let s = Arc::new(MemStore::new(&[
            ("SD:A.BIN", Some(b"aaaa")),
            ("SD:B.BIN", Some(b"bbbb")),
            ("SD:C.BIN", Some(b"cccc")),
        ]));
        let d = drive(&s);
        let (a, b) = (
            d.open(&p("/SD-Card/A.BIN")).unwrap(),
            d.open(&p("/SD-Card/B.BIN")).unwrap(),
        );
        let mut buf = [0u8; 4];
        d.read(&a, 0, &mut buf).unwrap();
        d.read(&b, 0, &mut buf).unwrap();
        s.close_idle_handles();
        // C gets A's old number.
        let c = d.open(&p("/SD-Card/C.BIN")).unwrap();
        d.read(&c, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"cccc");
        // B's number is still free: the device refuses it, and the drive reopens.
        d.read(&b, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"bbbb");
        // A's number now belongs to C.
        d.read(&a, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"aaaa");
    }

    #[test]
    fn creates_writes_and_uploads_on_release() {
        let s = sample();
        let d = drive(&s);
        let f = d.create(&p("\\SD-Card\\IMPORT\\new.wav"), false).unwrap();
        assert_eq!(s.get("SD:IMPORT/new.wav"), Some(Some(vec![])));
        d.write(&f, Some(0), b"hello", false).unwrap();
        d.write(&f, None, b" world", false).unwrap();
        // Nothing uploaded until the file is flushed or closed.
        assert_eq!(s.get("SD:IMPORT/new.wav"), Some(Some(vec![])));
        let mut buf = [0u8; 11];
        assert_eq!(d.read(&f, 0, &mut buf).unwrap(), 11);
        assert_eq!(&buf, b"hello world");
        d.release(f);
        assert_eq!(
            s.get("SD:IMPORT/new.wav"),
            Some(Some(b"hello world".to_vec()))
        );
        assert_eq!(d.lookup(&p("\\SD-Card\\IMPORT\\new.wav")).unwrap().size, 11);
    }

    #[test]
    fn edits_existing_files_in_place() {
        let s = sample();
        let d = drive(&s);
        let f = d.open(&p("\\Internal\\QSPI.bin")).unwrap();
        d.write(&f, Some(2), b"ab", false).unwrap();
        assert_eq!(d.write(&f, Some(9), b"xyz", true).unwrap(), 1);
        d.set_size(&f, 12).unwrap();
        d.release(f);
        assert_eq!(s.get("QSPI.bin"), Some(Some(b"01ab45678x\0\0".to_vec())));
    }

    #[test]
    fn changes_files_that_are_open_for_reading() {
        // The device refuses to replace, delete or rename a file it has open.
        let s = sample();
        let d = drive(&s);
        let reader = d.open(&p("/Internal/QSPI.bin")).unwrap();
        let mut buf = [0u8; 2];
        d.read(&reader, 0, &mut buf).unwrap();
        // Append, as `Add-Content` does: the whole file is read, then rewritten.
        let writer = d.open(&p("/Internal/QSPI.bin")).unwrap();
        d.write(&writer, None, b"!", false).unwrap();
        d.release(writer);
        assert_eq!(s.get("QSPI.bin"), Some(Some(b"0123456789!".to_vec())));
        // The reader carries on, reopening as needed.
        d.read(&reader, 0, &mut buf).unwrap();
        d.rename(&reader, &p("/Internal/Q2.bin"), false).unwrap();
        d.read(&reader, 9, &mut buf).unwrap();
        assert_eq!(&buf, b"9!");
        d.delete(&reader).unwrap();
        assert_eq!(s.get("Q2.bin"), None);
    }

    #[test]
    fn renaming_a_folder_moves_files_open_inside_it() {
        let s = sample();
        let d = drive(&s);
        let inside = d.open(&p("/SD-Card/IMPORT/kick.wav")).unwrap();
        let dir = d.open(&p("/SD-Card/IMPORT")).unwrap();
        d.rename(&dir, &p("/SD-Card/IN"), false).unwrap();
        assert_eq!(inside.path(), p("/SD-Card/IN/kick.wav"));
        d.write(&inside, None, b"!", false).unwrap();
        d.release(inside);
        assert_eq!(s.get("SD:IN/kick.wav"), Some(Some(b"RIFF....!".to_vec())));
    }

    #[test]
    fn root_and_internal_stay_apart() {
        // Both have the device path "", but are different folders.
        let s = sample();
        let d = drive(&s);
        let root = d.open(&VPath::root()).unwrap();
        let internal = d.open(&p("/Internal")).unwrap();
        assert_eq!(root.path(), VPath::root());
        assert_eq!(internal.path(), p("/Internal"));
        assert!(
            d.list(&internal.path())
                .unwrap()
                .iter()
                .any(|a| a.name == "QSPI.bin")
        );
    }

    #[test]
    fn makes_and_removes_folders() {
        let s = sample();
        let d = drive(&s);
        let dir = d.create(&p("\\SD-Card\\IMPORT\\pack"), true).unwrap();
        assert!(dir.is_dir());
        let f = d
            .create(&p("\\SD-Card\\IMPORT\\pack\\a.wav"), false)
            .unwrap();
        assert_eq!(d.check_delete(&dir), Err(FsError::NotEmpty));
        d.check_delete(&f).unwrap();
        d.delete(&f).unwrap();
        d.release(f);
        d.check_delete(&dir).unwrap();
        d.delete(&dir).unwrap();
        assert_eq!(s.get("SD:IMPORT/pack"), None);
        assert_eq!(
            d.lookup(&p("\\SD-Card\\IMPORT\\pack")),
            Err(FsError::NotFound)
        );
    }

    #[test]
    fn renames_within_a_volume_only() {
        let s = sample();
        let d = drive(&s);
        let f = d.open(&p("\\SD-Card\\IMPORT\\kick.wav")).unwrap();
        assert_eq!(
            d.rename(&f, &p("\\Internal\\kick.wav"), false),
            Err(FsError::CrossVolume)
        );
        d.rename(&f, &p("\\SD-Card\\kick2.wav"), false).unwrap();
        assert_eq!(f.path(), p("\\SD-Card\\kick2.wav"));
        assert_eq!(s.get("SD:kick2.wav"), Some(Some(b"RIFF....".to_vec())));
        assert_eq!(
            d.lookup(&p("\\SD-Card\\IMPORT\\kick.wav")),
            Err(FsError::NotFound)
        );
        assert!(d.lookup(&p("\\SD-Card\\kick2.wav")).is_ok());

        let g = d.create(&p("\\SD-Card\\other.wav"), false).unwrap();
        assert_eq!(
            d.rename(&g, &p("\\SD-Card\\kick2.wav"), false),
            Err(FsError::Exists)
        );
        d.rename(&g, &p("\\SD-Card\\kick2.wav"), true).unwrap();
        assert_eq!(s.get("SD:kick2.wav"), Some(Some(vec![])));
    }

    #[test]
    fn fixed_folders_cannot_change() {
        let s = sample();
        let d = drive(&s);
        assert_eq!(
            d.create(&p("\\SD-Card"), true).err(),
            Some(FsError::AccessDenied)
        );
        let vol = d.open(&p("\\Internal")).unwrap();
        assert_eq!(d.check_delete(&vol), Err(FsError::AccessDenied));
        assert_eq!(
            d.rename(&vol, &p("\\Internal\\x"), false),
            Err(FsError::AccessDenied)
        );
    }

    #[test]
    fn read_only_refuses_changes() {
        let s = sample();
        let d = Drive::new(
            s.clone(),
            Options {
                read_only: true,
                ..Options::default()
            },
        );
        assert_eq!(
            d.create(&p("\\SD-Card\\x"), false).err(),
            Some(FsError::ReadOnly)
        );
        let f = d.open(&p("\\Internal\\QSPI.bin")).unwrap();
        assert_eq!(d.write(&f, Some(0), b"x", false), Err(FsError::ReadOnly));
    }

    #[test]
    fn shutdown_saves_open_files() {
        let s = sample();
        let d = drive(&s);
        let f = d.create(&p("\\SD-Card\\pending.txt"), false).unwrap();
        d.write(&f, Some(0), b"data", false).unwrap();
        d.shutdown();
        assert_eq!(s.get("SD:pending.txt"), Some(Some(b"data".to_vec())));
        drop(f);
    }

    #[test]
    fn shutdown_leaves_read_handles_to_the_device() {
        let s = sample();
        let d = drive(&s);
        let f = d.open(&p("/Internal/QSPI.bin")).unwrap();
        let mut buf = [0u8; 4];
        d.read(&f, 0, &mut buf).unwrap();
        assert_eq!(s.open_handles(), 1);
        d.shutdown();
        assert_eq!(s.open_handles(), 1);
    }

    #[test]
    fn reports_the_internal_storage_size() {
        // MemStore reports 1 GiB free.
        let d = drive(&sample());
        assert_eq!(d.space(), (16_000_000_000, 1 << 30));
    }

    #[test]
    fn rejects_paths_too_long_for_the_device() {
        let s = sample();
        let d = drive(&s);
        let long = format!("\\SD-Card\\{}", "x".repeat(250));
        assert_eq!(d.create(&p(&long), false).err(), Some(FsError::InvalidName));
    }
}
