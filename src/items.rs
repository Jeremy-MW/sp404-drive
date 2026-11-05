//! Numbered items over [`Drive`], for front ends that name files and folders by number
//! rather than by path: FSKit on macOS, through the C interface in `ffi/`.
//!
//! [`Items`] keeps a table from item numbers to drive paths. A path keeps its number for
//! as long as the front end holds the item (until [`Items::reclaim`]), and a rename moves
//! the item and everything below it. Each item has at most one [`OpenFile`], shared by all
//! the front end's opens: FSKit opens an item with modes (read, write) and closes it
//! "keeping" the modes still in use, so the file is uploaded when the last writer closes
//! it, as on Windows when the program writing it closes it.
//!
//! Platform-neutral, so it is tested here against the in-memory device.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::vfs::{Attr, Drive, FsError, FsResult, OpenFile, VPath};

/// The root's item number (FSKit's `FSItemIDRootDirectory`).
pub const ROOT: u64 = 2;
/// First number handed out for other items (FSKit reserves 0 to 2).
const FIRST: u64 = 3;

/// Names macOS creates for itself, which have no place on the unit: AppleDouble files
/// (`._name`), Finder's folder settings, the trash, and the indexing and event folders.
/// They can't be created through the drive; existing ones (from a card reader, say) can
/// still be seen and deleted.
pub fn is_mac_metadata(name: &str) -> bool {
    name.starts_with("._")
        || matches!(
            name,
            ".DS_Store"
                | ".Trashes"
                | ".fseventsd"
                | ".Spotlight-V100"
                | ".TemporaryItems"
                | ".apdisk"
                | ".VolumeIcon.icns"
        )
}

/// A file or folder and its number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: u64,
    pub attr: Attr,
}

struct Node {
    path: VPath,
    open: Option<Arc<OpenFile>>,
    /// The item is open for writing.
    writing: bool,
}

struct Table {
    nodes: HashMap<u64, Node>,
    /// Lowercased path -> number: names match without regard to case.
    by_key: HashMap<String, u64>,
    next: u64,
}

fn key(path: &VPath) -> String {
    path.to_string().to_lowercase()
}

impl Table {
    fn path(&self, id: u64) -> FsResult<VPath> {
        self.nodes
            .get(&id)
            .map(|n| n.path.clone())
            .ok_or(FsError::NotFound)
    }

    /// The number for `path` (in the device's spelling), numbering it if new.
    fn id(&mut self, path: &VPath) -> u64 {
        let k = key(path);
        if let Some(&id) = self.by_key.get(&k)
            && let Some(node) = self.nodes.get_mut(&id)
        {
            node.path = path.clone();
            return id;
        }
        let id = self.next;
        self.next += 1;
        self.nodes.insert(
            id,
            Node {
                path: path.clone(),
                open: None,
                writing: false,
            },
        );
        self.by_key.insert(k, id);
        id
    }

    /// `from` was renamed to `to`: move it and everything below it.
    fn moved(&mut self, from: &VPath, to: &VPath) {
        // Whatever `to` named before (a file replaced by the rename) is gone.
        if let Some(old) = self.by_key.remove(&key(to))
            && let Some(node) = self.nodes.get(&old)
            && !node.path.starts_with(from)
        {
            log::debug!("{to} was replaced by a rename");
        }
        let mut rekeyed = Vec::new();
        for (&id, node) in self.nodes.iter_mut() {
            if node.path.starts_with(from) {
                let old = key(&node.path);
                node.path = node.path.rebase(from, to);
                rekeyed.push((old, key(&node.path), id));
            }
        }
        for (old, _, id) in &rekeyed {
            if self.by_key.get(old) == Some(id) {
                self.by_key.remove(old);
            }
        }
        for (_, new, id) in rekeyed {
            self.by_key.insert(new, id);
        }
    }
}

pub struct Items {
    drive: Arc<Drive>,
    table: Mutex<Table>,
}

impl Items {
    pub fn new(drive: Arc<Drive>) -> Self {
        let mut table = Table {
            nodes: HashMap::new(),
            by_key: HashMap::new(),
            next: FIRST,
        };
        table.nodes.insert(
            ROOT,
            Node {
                path: VPath::root(),
                open: None,
                writing: false,
            },
        );
        table.by_key.insert(key(&VPath::root()), ROOT);
        Items {
            drive,
            table: Mutex::new(table),
        }
    }

    pub fn drive(&self) -> &Arc<Drive> {
        &self.drive
    }

    fn path(&self, id: u64) -> FsResult<VPath> {
        self.table.lock().unwrap().path(id)
    }

    /// The path of `name` in folder `dir`. In the root, only the two volume folders.
    fn child(&self, dir: u64, name: &str) -> FsResult<VPath> {
        // `:` separates the card's prefix in device paths; `/` and `\` separate drive paths.
        if name.is_empty() || name == "." || name == ".." || name.contains([':', '/', '\\']) {
            return Err(FsError::InvalidName);
        }
        let dir = self.path(dir)?;
        if dir.is_root() {
            VPath::parse(name)
        } else {
            Ok(dir.child(name))
        }
    }

    /// `path` with its last component as the device spells it.
    fn spelled(path: &VPath, attr: &Attr) -> VPath {
        match path.parent() {
            Some(parent) if !parent.is_root() => parent.child(&attr.name),
            _ => path.clone(),
        }
    }

    /// Look up `name` in folder `dir`, numbering it if new.
    pub fn lookup(&self, dir: u64, name: &str) -> FsResult<Item> {
        let path = self.child(dir, name).map_err(|e| match e {
            FsError::InvalidName => FsError::NotFound,
            e => e,
        })?;
        let attr = self.drive.lookup(&path)?;
        let id = self.table.lock().unwrap().id(&Self::spelled(&path, &attr));
        let attr = self.open_attr(id).unwrap_or(attr);
        Ok(Item { id, attr })
    }

    /// Attributes of an open item, which may hold unsaved writes.
    fn open_attr(&self, id: u64) -> Option<Attr> {
        let table = self.table.lock().unwrap();
        table.nodes.get(&id)?.open.as_ref().map(|f| f.attr())
    }

    /// Current attributes of an item.
    pub fn attr(&self, id: u64) -> FsResult<Attr> {
        if let Some(attr) = self.open_attr(id) {
            return Ok(attr);
        }
        self.drive.lookup(&self.path(id)?)
    }

    /// The contents of folder `dir`, each numbered.
    pub fn list(&self, dir: u64) -> FsResult<Vec<Item>> {
        let path = self.path(dir)?;
        let entries = self.drive.list(&path)?;
        let mut table = self.table.lock().unwrap();
        let mut items = Vec::with_capacity(entries.len());
        for attr in entries {
            let child = if path.is_root() {
                match VPath::parse(&attr.name) {
                    Ok(p) => p,
                    Err(_) => continue,
                }
            } else {
                path.child(&attr.name)
            };
            let id = table.id(&child);
            let attr = table
                .nodes
                .get(&id)
                .and_then(|n| n.open.as_ref())
                .map(|f| f.attr())
                .unwrap_or(attr);
            items.push(Item { id, attr });
        }
        Ok(items)
    }

    /// The front end no longer holds item `id`: forget its number, unless it's open.
    pub fn reclaim(&self, id: u64) {
        if id == ROOT {
            return;
        }
        let mut table = self.table.lock().unwrap();
        let Some(node) = table.nodes.get(&id) else {
            return;
        };
        if node.open.is_some() {
            return;
        }
        let k = key(&node.path);
        table.nodes.remove(&id);
        if table.by_key.get(&k) == Some(&id) {
            table.by_key.remove(&k);
        }
    }

    /// Open item `id`, for writing if `write`. Opening an open item adds the mode.
    pub fn open(&self, id: u64, write: bool) -> FsResult<()> {
        if write && self.drive.read_only() {
            return Err(FsError::ReadOnly);
        }
        let path = {
            let table = self.table.lock().unwrap();
            let node = table.nodes.get(&id).ok_or(FsError::NotFound)?;
            if node.open.is_some() {
                drop(table);
                if write {
                    self.table
                        .lock()
                        .unwrap()
                        .nodes
                        .get_mut(&id)
                        .ok_or(FsError::NotFound)?
                        .writing = true;
                }
                return Ok(());
            }
            node.path.clone()
        };
        let file = self.drive.open(&path)?;
        let mut table = self.table.lock().unwrap();
        match table.nodes.get_mut(&id) {
            Some(node) if node.open.is_none() => {
                node.open = Some(Arc::new(file));
                node.writing = write;
            }
            Some(node) => {
                // Opened meanwhile by another call.
                node.writing |= write;
                drop(table);
                self.drive.release(file);
            }
            None => {
                drop(table);
                self.drive.release(file);
                return Err(FsError::NotFound);
            }
        }
        Ok(())
    }

    /// Close item `id`, keeping the modes still in use. When the last writer closes, the
    /// file is uploaded; an upload failure is returned. When nothing is kept, the item's
    /// file is let go.
    pub fn close(&self, id: u64, keep_read: bool, keep_write: bool) -> FsResult<()> {
        let (file, was_writing) = {
            let mut table = self.table.lock().unwrap();
            let Some(node) = table.nodes.get_mut(&id) else {
                return Ok(());
            };
            let Some(file) = node.open.clone() else {
                return Ok(());
            };
            let was_writing = node.writing;
            node.writing = keep_write;
            if !keep_read && !keep_write {
                node.open = None;
            }
            (file, was_writing)
        };
        let saved = if was_writing && !keep_write {
            self.drive.flush(&file)
        } else {
            Ok(())
        };
        if !keep_read && !keep_write {
            match Arc::try_unwrap(file) {
                Ok(file) => self.drive.release(file),
                // A read or write still running; it lets the file go when it ends.
                Err(_) => log::debug!("item {id} closed while in use"),
            }
        }
        saved
    }

    /// Run `f` with item `id`'s open file, or with one opened just for the call.
    fn with_file<T>(&self, id: u64, f: impl FnOnce(&OpenFile) -> FsResult<T>) -> FsResult<T> {
        let open = {
            let table = self.table.lock().unwrap();
            let node = table.nodes.get(&id).ok_or(FsError::NotFound)?;
            match &node.open {
                Some(file) => Ok(file.clone()),
                None => Err(node.path.clone()),
            }
        };
        match open {
            Ok(file) => f(&file),
            Err(path) => {
                let file = self.drive.open(&path)?;
                let result = f(&file);
                // Saves anything written, for a write without an open.
                self.drive.release(file);
                result
            }
        }
    }

    pub fn read(&self, id: u64, offset: u64, buf: &mut [u8]) -> FsResult<usize> {
        self.with_file(id, |file| self.drive.read(file, offset, buf))
    }

    pub fn write(&self, id: u64, offset: u64, data: &[u8]) -> FsResult<usize> {
        self.with_file(id, |file| self.drive.write(file, Some(offset), data, false))
    }

    /// Change a file's length. Saved with the file's other changes when it's closed, or at
    /// once if it isn't open.
    pub fn set_size(&self, id: u64, size: u64) -> FsResult<()> {
        self.with_file(id, |file| self.drive.set_size(file, size))
    }

    /// Create a file or folder named `name` in folder `dir`.
    pub fn create(&self, dir: u64, name: &str, is_dir: bool) -> FsResult<Item> {
        if is_mac_metadata(name) {
            return Err(FsError::AccessDenied);
        }
        let path = self.child(dir, name)?;
        let file = self.drive.create(&path, is_dir)?;
        let attr = file.attr();
        let id = self.table.lock().unwrap().id(&file.path());
        self.drive.release(file);
        Ok(Item { id, attr })
    }

    /// Delete `name` from folder `dir`: a file, or an empty folder.
    pub fn remove(&self, dir: u64, name: &str) -> FsResult<()> {
        let path = self.child(dir, name)?;
        let file = self.drive.open(&path)?;
        let result = self
            .drive
            .check_delete(&file)
            .and_then(|_| self.drive.delete(&file));
        let path = file.path();
        self.drive.release(file);
        result?;
        let mut table = self.table.lock().unwrap();
        if let Some(id) = table.by_key.remove(&key(&path))
            && let Some(node) = table.nodes.get_mut(&id)
        {
            // Nothing left to save if it was open.
            node.writing = false;
        }
        Ok(())
    }

    /// Rename `from_name` in `from_dir` to `to_name` in `to_dir`, replacing a file there
    /// if `replace`. Returns the new name as stored.
    pub fn rename(
        &self,
        from_dir: u64,
        from_name: &str,
        to_dir: u64,
        to_name: &str,
        replace: bool,
    ) -> FsResult<String> {
        if is_mac_metadata(to_name) {
            return Err(FsError::AccessDenied);
        }
        let from = self.child(from_dir, from_name)?;
        let to = self.child(to_dir, to_name)?;
        let file = self.drive.open(&from)?;
        let from = file.path();
        let result = self.drive.rename(&file, &to, replace);
        let moved_to = file.path();
        self.drive.release(file);
        result?;
        self.table.lock().unwrap().moved(&from, &moved_to);
        Ok(moved_to.name().to_string())
    }

    /// Upload every open file with unsaved changes.
    pub fn sync(&self) -> FsResult<()> {
        let open: Vec<Arc<OpenFile>> = self
            .table
            .lock()
            .unwrap()
            .nodes
            .values()
            .filter_map(|n| n.open.clone())
            .collect();
        let mut first_error = Ok(());
        for file in open {
            if let Err(e) = self.drive.flush(&file)
                && first_error.is_ok()
            {
                first_error = Err(e);
            }
        }
        first_error
    }

    /// Unmounting: let every open file go (saving it), then release the device.
    pub fn shutdown(&self) {
        let open: Vec<Arc<OpenFile>> = self
            .table
            .lock()
            .unwrap()
            .nodes
            .values_mut()
            .filter_map(|n| {
                n.writing = false;
                n.open.take()
            })
            .collect();
        for file in open {
            if let Ok(file) = Arc::try_unwrap(file) {
                self.drive.release(file);
            }
        }
        self.drive.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::mem::MemStore;
    use crate::vfs::Options;

    fn items(store: &Arc<MemStore>) -> Items {
        Items::new(Arc::new(Drive::new(store.clone(), Options::default())))
    }

    fn sample() -> Arc<MemStore> {
        Arc::new(MemStore::new(&[
            ("ROLAND", None),
            ("QSPI.bin", Some(b"0123456789")),
            ("SD:IMPORT", None),
            ("SD:IMPORT/Kick.wav", Some(b"RIFF....")),
            ("SD:IMPORT/pack", None),
            ("SD:IMPORT/pack/snare.wav", Some(b"snare")),
        ]))
    }

    fn walk(items: &Items, path: &[&str]) -> Item {
        let mut id = ROOT;
        let mut item = None;
        for name in path {
            let found = items.lookup(id, name).unwrap();
            id = found.id;
            item = Some(found);
        }
        item.unwrap()
    }

    #[test]
    fn numbers_paths_and_keeps_numbers() {
        let s = sample();
        let it = items(&s);
        let card = it.lookup(ROOT, "sd-card").unwrap();
        assert_eq!(card.attr.name, "SD-Card");
        assert!(card.id >= FIRST);
        let import = it.lookup(card.id, "import").unwrap();
        // The device's spelling, the same number however it's spelt.
        let kick = it.lookup(import.id, "KICK.WAV").unwrap();
        assert_eq!(kick.attr.name, "Kick.wav");
        assert_eq!(kick.attr.size, 8);
        assert_eq!(it.lookup(import.id, "kick.wav").unwrap().id, kick.id);
        let listed = it.list(import.id).unwrap();
        assert!(listed.iter().any(|i| i.id == kick.id));
        assert_eq!(it.lookup(ROOT, "Elsewhere"), Err(FsError::NotFound));
        assert_eq!(it.lookup(import.id, "a:b"), Err(FsError::NotFound));
    }

    #[test]
    fn uploads_when_the_last_writer_closes() {
        let s = sample();
        let it = items(&s);
        let import = walk(&it, &["SD-Card", "IMPORT"]);
        let new = it.create(import.id, "new.wav", false).unwrap();
        it.open(new.id, false).unwrap();
        it.open(new.id, true).unwrap();
        assert_eq!(it.write(new.id, 0, b"hello").unwrap(), 5);
        assert_eq!(it.attr(new.id).unwrap().size, 5);
        // Created empty, not yet uploaded.
        assert_eq!(s.get("SD:IMPORT/new.wav"), Some(Some(Vec::new())));
        // The writer closes; a reader stays.
        it.close(new.id, true, false).unwrap();
        assert_eq!(s.get("SD:IMPORT/new.wav"), Some(Some(b"hello".to_vec())));
        let mut buf = [0u8; 8];
        assert_eq!(it.read(new.id, 0, &mut buf).unwrap(), 5);
        it.close(new.id, false, false).unwrap();
        assert_eq!(s.open_handles(), 0);
    }

    #[test]
    fn reads_without_an_open() {
        let s = sample();
        let it = items(&s);
        let qspi = walk(&it, &["Internal", "QSPI.bin"]);
        let mut buf = [0u8; 4];
        assert_eq!(it.read(qspi.id, 2, &mut buf).unwrap(), 4);
        assert_eq!(&buf, b"2345");
    }

    #[test]
    fn rename_moves_the_item_and_what_is_below_it() {
        let s = sample();
        let it = items(&s);
        let import = walk(&it, &["SD-Card", "IMPORT"]);
        let pack = it.lookup(import.id, "pack").unwrap();
        let snare = it.lookup(pack.id, "snare.wav").unwrap();
        let card = it.lookup(ROOT, "SD-Card").unwrap();
        let name = it
            .rename(import.id, "pack", card.id, "Drums", false)
            .unwrap();
        assert_eq!(name, "Drums");
        assert_eq!(s.get("SD:Drums/snare.wav"), Some(Some(b"snare".to_vec())));
        // Same numbers, new paths.
        assert_eq!(it.lookup(card.id, "drums").unwrap().id, pack.id);
        let mut buf = [0u8; 5];
        assert_eq!(it.read(snare.id, 0, &mut buf).unwrap(), 5);
        assert_eq!(it.lookup(import.id, "pack"), Err(FsError::NotFound));
    }

    #[test]
    fn removes_files_and_empty_folders() {
        let s = sample();
        let it = items(&s);
        let import = walk(&it, &["SD-Card", "IMPORT"]);
        assert_eq!(it.remove(import.id, "pack"), Err(FsError::NotEmpty));
        let pack = it.lookup(import.id, "pack").unwrap();
        it.remove(pack.id, "snare.wav").unwrap();
        it.remove(import.id, "pack").unwrap();
        assert_eq!(s.get("SD:IMPORT/pack"), None);
        assert_eq!(it.lookup(import.id, "pack"), Err(FsError::NotFound));
        // The fixed folders stay.
        assert_eq!(it.remove(ROOT, "SD-Card"), Err(FsError::AccessDenied));
    }

    #[test]
    fn refuses_mac_metadata_names() {
        let s = sample();
        let it = items(&s);
        let import = walk(&it, &["SD-Card", "IMPORT"]);
        for name in ["._Kick.wav", ".DS_Store", ".Trashes"] {
            assert_eq!(
                it.create(import.id, name, false),
                Err(FsError::AccessDenied),
                "{name}"
            );
        }
        assert_eq!(
            it.rename(import.id, "Kick.wav", import.id, "._x", false),
            Err(FsError::AccessDenied)
        );
    }

    #[test]
    fn reclaim_forgets_closed_items_only() {
        let s = sample();
        let it = items(&s);
        let kick = walk(&it, &["SD-Card", "IMPORT", "Kick.wav"]);
        it.open(kick.id, false).unwrap();
        it.reclaim(kick.id);
        assert!(it.attr(kick.id).is_ok());
        it.close(kick.id, false, false).unwrap();
        it.reclaim(kick.id);
        assert_eq!(it.attr(kick.id), Err(FsError::NotFound));
    }
}
