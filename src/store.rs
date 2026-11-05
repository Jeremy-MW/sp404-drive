//! What the drive needs from the device, as a trait, so the filesystem logic can be tested
//! without hardware and shared by every platform front end.
//!
//! Paths are SparkyMK2 device paths: `ROLAND/SP-404MKII` on the internal storage,
//! `SD:IMPORT/pack` on the card, `""` and `"SD:"` for the two roots.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::link::{self, Link};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The device answered −1; the path is missing, taken, not empty, or not allowed.
    #[error("device refused {0}")]
    Refused(String),
    /// The serial link is gone (unplugged, or the port closed).
    #[error("device disconnected")]
    Disconnected,
    #[error("{0}")]
    Other(String),
}

pub type StoreResult<T> = Result<T, StoreError>;

impl From<link::Error> for StoreError {
    fn from(e: link::Error) -> Self {
        match e {
            link::Error::Refused(what) => StoreError::Refused(what),
            link::Error::Closed => StoreError::Disconnected,
            other => StoreError::Other(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

pub trait Store: Send + Sync {
    /// Directory contents with file sizes, without `.` and `..`.
    fn list(&self, dir: &str) -> StoreResult<Vec<RawEntry>>;
    /// Open a file for reading; returns a device handle.
    fn open(&self, path: &str) -> StoreResult<i32>;
    /// Read up to `len` bytes at `offset`. Fewer bytes means end of file.
    fn read_at(&self, handle: i32, offset: u64, len: usize) -> StoreResult<Vec<u8>>;
    fn close(&self, handle: i32);
    /// Create or replace a whole file.
    fn write_file(&self, path: &str, data: &[u8]) -> StoreResult<()>;
    fn mkdir(&self, path: &str) -> StoreResult<()>;
    fn rmdir(&self, path: &str) -> StoreResult<()>;
    fn unlink(&self, path: &str) -> StoreResult<()>;
    fn rename(&self, from: &str, to: &str) -> StoreResult<()>;
    /// Free bytes on the internal storage, if the device says.
    fn free_bytes(&self) -> Option<u64>;
    /// False once the device has gone away.
    fn is_connected(&self) -> bool {
        true
    }
    /// Let go of the device: the last call before the link closes.
    fn goodbye(&self) {}
}

/// Join a device directory path and an entry name.
pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir.ends_with(':') || dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The real device, over its serial file API.
pub struct DeviceStore {
    dev: Link,
    /// Where each open handle's file position is, to skip redundant seeks.
    positions: Mutex<HashMap<i32, u64>>,
}

impl DeviceStore {
    pub fn new(dev: Link) -> Self {
        DeviceStore {
            dev,
            positions: Mutex::new(HashMap::new()),
        }
    }

    pub fn port_name(&self) -> &str {
        self.dev.port_name()
    }
}

/// Largest read requested at once; the size the official app uses.
const READ_REQUEST: usize = 0x20000;

impl Store for DeviceStore {
    fn list(&self, dir: &str) -> StoreResult<Vec<RawEntry>> {
        self.dev.exclusive(|d| {
            let entries = d.list_dir(dir)?;
            let mut out = Vec::with_capacity(entries.len());
            for e in entries {
                // readdir gives names and types only; sizes take a stat each.
                let size = if e.is_dir() {
                    0
                } else {
                    match d.stat(&join(dir, &e.name)) {
                        Ok(Some(st)) => u64::from(st.size),
                        Ok(None) => 0,
                        Err(link::Error::Closed) => return Err(StoreError::Disconnected),
                        Err(err) => {
                            log::debug!("stat {}: {err}", join(dir, &e.name));
                            0
                        }
                    }
                };
                out.push(RawEntry {
                    is_dir: e.is_dir(),
                    name: e.name,
                    size,
                });
            }
            Ok(out)
        })
    }

    fn open(&self, path: &str) -> StoreResult<i32> {
        let handle = self
            .dev
            .exclusive(|d| d.open_file(path, link::OPEN_READ_ONLY))?;
        self.positions.lock().unwrap().insert(handle, 0);
        Ok(handle)
    }

    fn read_at(&self, handle: i32, offset: u64, len: usize) -> StoreResult<Vec<u8>> {
        let offset32 =
            u32::try_from(offset).map_err(|_| StoreError::Other("offset past 4 GB".into()))?;
        self.dev.exclusive(|d| {
            let mut positions = self.positions.lock().unwrap();
            let pos = positions.entry(handle).or_insert(u64::MAX);
            if *pos != offset {
                d.seek(handle, offset32)?;
                *pos = offset;
            }
            let mut out = Vec::with_capacity(len);
            while out.len() < len {
                let want = (len - out.len()).min(READ_REQUEST) as u32;
                let chunk = d.read(handle, want)?;
                *pos += chunk.len() as u64;
                let short = chunk.len() < want as usize;
                out.extend_from_slice(&chunk);
                if chunk.is_empty() || short {
                    break;
                }
            }
            Ok(out)
        })
    }

    fn close(&self, handle: i32) {
        self.positions.lock().unwrap().remove(&handle);
        if let Err(e) = self.dev.exclusive(|d| d.close_file(handle)) {
            log::debug!("close handle {handle}: {e}");
        }
    }

    fn write_file(&self, path: &str, data: &[u8]) -> StoreResult<()> {
        Ok(self.dev.exclusive(|d| d.write_file(path, data))?)
    }

    fn mkdir(&self, path: &str) -> StoreResult<()> {
        Ok(self.dev.exclusive(|d| d.mkdir(path))?)
    }

    fn rmdir(&self, path: &str) -> StoreResult<()> {
        Ok(self.dev.exclusive(|d| d.rmdir(path))?)
    }

    fn unlink(&self, path: &str) -> StoreResult<()> {
        Ok(self.dev.exclusive(|d| d.unlink(path))?)
    }

    fn rename(&self, from: &str, to: &str) -> StoreResult<()> {
        Ok(self.dev.exclusive(|d| d.rename(from, to))?)
    }

    fn free_bytes(&self) -> Option<u64> {
        self.dev
            .exclusive(|d| d.free_kb())
            .ok()
            .map(|kb| u64::from(kb) * 1024)
    }

    fn is_connected(&self) -> bool {
        !self.dev.is_closed()
    }

    /// Closing the port while the device's session is alive can freeze it, so wait the
    /// session out first, sending nothing.
    fn goodbye(&self) {
        self.dev.exclusive(|d| d.let_session_lapse());
    }
}

/// An in-memory device for tests: a flat map of paths, directories marked by `None`.
#[cfg(test)]
pub mod mem {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Default)]
    pub struct MemStore {
        pub nodes: Mutex<BTreeMap<String, Option<Vec<u8>>>>,
        handles: Mutex<HashMap<i32, String>>,
        pub requests: Mutex<Vec<String>>,
    }

    fn parent(path: &str) -> &str {
        match path.rfind('/') {
            Some(i) => &path[..i],
            None if path.starts_with("SD:") => "SD:",
            None => "",
        }
    }

    impl MemStore {
        pub fn new(paths: &[(&str, Option<&[u8]>)]) -> Self {
            let s = MemStore::default();
            {
                let mut nodes = s.nodes.lock().unwrap();
                for (p, data) in paths {
                    nodes.insert(p.to_string(), data.map(|d| d.to_vec()));
                }
            }
            s
        }

        fn log(&self, what: String) {
            self.requests.lock().unwrap().push(what);
        }

        /// Device handles currently open.
        pub fn open_handles(&self) -> usize {
            self.handles.lock().unwrap().len()
        }

        /// Like the device after a couple of seconds without requests: close every open
        /// handle, without telling anyone.
        pub fn close_idle_handles(&self) {
            self.handles.lock().unwrap().clear();
        }

        pub fn get(&self, path: &str) -> Option<Option<Vec<u8>>> {
            self.nodes.lock().unwrap().get(path).cloned()
        }

        fn dir_exists(&self, nodes: &BTreeMap<String, Option<Vec<u8>>>, dir: &str) -> bool {
            dir.is_empty() || dir == "SD:" || matches!(nodes.get(dir), Some(None))
        }

        /// Like the device, refuse to change a file that is open.
        fn is_open(&self, path: &str) -> bool {
            let below = format!("{path}/");
            self.handles
                .lock()
                .unwrap()
                .values()
                .any(|p| p == path || p.starts_with(&below))
        }
    }

    impl Store for MemStore {
        fn list(&self, dir: &str) -> StoreResult<Vec<RawEntry>> {
            self.log(format!("list {dir}"));
            let nodes = self.nodes.lock().unwrap();
            Ok(nodes
                .iter()
                .filter(|(p, _)| parent(p) == dir && p.as_str() != dir)
                .map(|(p, d)| RawEntry {
                    name: p[p.rfind(['/', ':']).map_or(0, |i| i + 1)..].to_string(),
                    is_dir: d.is_none(),
                    size: d.as_ref().map_or(0, |d| d.len() as u64),
                })
                .collect())
        }

        fn open(&self, path: &str) -> StoreResult<i32> {
            self.log(format!("open {path}"));
            match self.nodes.lock().unwrap().get(path) {
                Some(Some(_)) => {}
                _ => return Err(StoreError::Refused(format!("open {path}"))),
            }
            // Like the device, hand out the lowest free number.
            let mut handles = self.handles.lock().unwrap();
            let handle = (1..).find(|h| !handles.contains_key(h)).unwrap();
            handles.insert(handle, path.to_string());
            Ok(handle)
        }

        fn read_at(&self, handle: i32, offset: u64, len: usize) -> StoreResult<Vec<u8>> {
            let Some(path) = self.handles.lock().unwrap().get(&handle).cloned() else {
                return Err(StoreError::Refused("read".into()));
            };
            let nodes = self.nodes.lock().unwrap();
            let data = nodes.get(&path).cloned().flatten().unwrap_or_default();
            let start = (offset as usize).min(data.len());
            let end = (start + len).min(data.len());
            Ok(data[start..end].to_vec())
        }

        fn close(&self, handle: i32) {
            self.handles.lock().unwrap().remove(&handle);
        }

        fn write_file(&self, path: &str, data: &[u8]) -> StoreResult<()> {
            self.log(format!("write {path} {}", data.len()));
            if self.is_open(path) {
                return Err(StoreError::Refused(format!("open {path}")));
            }
            let mut nodes = self.nodes.lock().unwrap();
            if !self.dir_exists(&nodes, parent(path)) || matches!(nodes.get(path), Some(None)) {
                return Err(StoreError::Refused(format!("open {path}")));
            }
            nodes.insert(path.to_string(), Some(data.to_vec()));
            Ok(())
        }

        fn mkdir(&self, path: &str) -> StoreResult<()> {
            self.log(format!("mkdir {path}"));
            let mut nodes = self.nodes.lock().unwrap();
            if nodes.contains_key(path) || !self.dir_exists(&nodes, parent(path)) {
                return Err(StoreError::Refused(format!("mkdir {path}")));
            }
            nodes.insert(path.to_string(), None);
            Ok(())
        }

        fn rmdir(&self, path: &str) -> StoreResult<()> {
            self.log(format!("rmdir {path}"));
            let mut nodes = self.nodes.lock().unwrap();
            let has_children = nodes.keys().any(|p| parent(p) == path && p != path);
            if !matches!(nodes.get(path), Some(None)) || has_children {
                return Err(StoreError::Refused(format!("rmdir {path}")));
            }
            nodes.remove(path);
            Ok(())
        }

        fn unlink(&self, path: &str) -> StoreResult<()> {
            self.log(format!("unlink {path}"));
            if self.is_open(path) {
                return Err(StoreError::Refused(format!("unlink {path}")));
            }
            let mut nodes = self.nodes.lock().unwrap();
            match nodes.get(path) {
                Some(Some(_)) => {
                    nodes.remove(path);
                    Ok(())
                }
                _ => Err(StoreError::Refused(format!("unlink {path}"))),
            }
        }

        fn rename(&self, from: &str, to: &str) -> StoreResult<()> {
            self.log(format!("rename {from} {to}"));
            if self.is_open(from) {
                return Err(StoreError::Refused(format!("rename {from}")));
            }
            let mut nodes = self.nodes.lock().unwrap();
            if !nodes.contains_key(from) || nodes.contains_key(to) {
                return Err(StoreError::Refused(format!("rename {from}")));
            }
            let moved: Vec<String> = nodes
                .keys()
                .filter(|p| p.as_str() == from || p.starts_with(&format!("{from}/")))
                .cloned()
                .collect();
            for p in moved {
                let v = nodes.remove(&p).unwrap();
                nodes.insert(format!("{to}{}", &p[from.len()..]), v);
            }
            Ok(())
        }

        fn free_bytes(&self) -> Option<u64> {
            Some(1 << 30)
        }
    }
}
