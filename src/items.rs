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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use unicode_normalization::UnicodeNormalization;

use crate::vfs::{Attr, Drive, FsError, FsResult, OpenFile, VPath};

/// The root's item number (FSKit's `FSItemIDRootDirectory`).
pub const ROOT: u64 = 2;
/// First number handed out for other items (FSKit reserves 0 to 2).
const FIRST: u64 = 3;

/// Largest housekeeping file kept in memory (see [`is_mac_metadata`]).
const MAX_LOCAL: usize = 16 << 20;

/// The refusal for a housekeeping file over [`MAX_LOCAL`] (the C interface gives `EFBIG`).
pub const TOO_LARGE: &str = "housekeeping file too large";

/// Names macOS creates for itself, which have no place on the unit: AppleDouble files
/// (`._name`), Finder's folder settings, the trash, and the indexing and event folders.
///
/// The drive never creates them on the unit. A new file with such a name is kept in memory
/// instead, until it's deleted or the drive closes, so the program writing it carries on
/// (`cp -R` and `ditto` of a folder holding a `.DS_Store` would fail otherwise). It isn't
/// listed, since it isn't on the unit, but can be looked up, read, written, renamed and
/// deleted. Folders with these names are refused. Ones already on the unit (from a card
/// reader, say) are ordinary files.
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
    /// A housekeeping file kept in memory, never on the unit: its contents.
    local: Option<Vec<u8>>,
    /// Deleted, or replaced by a rename, while the front end still holds it. Its path is
    /// stale: it's never looked up or opened by path again (an open file stays usable, in
    /// memory only; see `vfs`).
    gone: bool,
}

struct Table {
    nodes: HashMap<u64, Node>,
    /// Lowercased path -> number: names match without regard to case.
    by_key: HashMap<String, u64>,
    /// Keys of the files this drive created on the unit since it opened (see
    /// [`Items::take_off_unit`]).
    made: HashSet<String>,
    next: u64,
}

/// A path's key: lowercased in ASCII only, as the drive matches names, so names that
/// differ in other letters' case are different items.
fn key(path: &VPath) -> String {
    path.to_string().to_ascii_lowercase()
}

/// A name for comparing spellings: precomposed (NFC), ASCII lowercased.
fn fold(name: &str) -> String {
    name.nfc().collect::<String>().to_ascii_lowercase()
}

impl Table {
    fn path(&self, id: u64) -> FsResult<VPath> {
        match self.nodes.get(&id) {
            Some(n) if !n.gone => Ok(n.path.clone()),
            _ => Err(FsError::NotFound),
        }
    }

    /// Item `id` was deleted or replaced: its path no longer leads to it.
    fn forget_path(&mut self, id: u64) {
        if let Some(node) = self.nodes.get_mut(&id) {
            node.gone = true;
            // Nothing left to save if it was open.
            node.writing = false;
            let k = key(&node.path);
            if self.by_key.get(&k) == Some(&id) {
                self.by_key.remove(&k);
            }
        }
    }

    /// Forget that `path`, and everything below it, was made by this drive.
    fn unmade(&mut self, path: &VPath) {
        let k = key(path);
        // `VPath` shows as `\SD-Card\dir\name`.
        let below = format!("{k}\\");
        self.made.retain(|m| *m != k && !m.starts_with(&below));
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
                local: None,
                gone: false,
            },
        );
        self.by_key.insert(k, id);
        id
    }

    /// The number of the in-memory file at `path`, if there is one.
    fn local(&self, path: &VPath) -> Option<u64> {
        let id = *self.by_key.get(&key(path))?;
        self.nodes.get(&id)?.local.as_ref().map(|_| id)
    }

    /// Forget an in-memory file.
    fn drop_local(&mut self, id: u64) {
        if let Some(node) = self.nodes.remove(&id) {
            let k = key(&node.path);
            if self.by_key.get(&k) == Some(&id) {
                self.by_key.remove(&k);
            }
        }
    }

    /// A folder was deleted: forget the in-memory files below it.
    fn drop_local_below(&mut self, dir: &VPath) {
        let below: Vec<u64> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.local.is_some() && n.path.starts_with(dir))
            .map(|(&id, _)| id)
            .collect();
        for id in below {
            self.drop_local(id);
        }
    }

    /// Point item `id` at `path`, taking the number over from whatever had it.
    fn repoint(&mut self, id: u64, path: &VPath) {
        let Some(node) = self.nodes.get_mut(&id) else {
            return;
        };
        let old = key(&node.path);
        node.path = path.clone();
        if self.by_key.get(&old) == Some(&id) {
            self.by_key.remove(&old);
        }
        self.by_key.insert(key(path), id);
    }

    /// `from` was renamed to `to`: move it and everything below it.
    fn moved(&mut self, from: &VPath, to: &VPath) {
        // Whatever `to` named before (a file replaced by the rename) is gone. (Not when
        // it's the item being renamed: a change of case only.)
        if let Some(&old) = self.by_key.get(&key(to))
            && let Some(node) = self.nodes.get(&old)
            && !node.path.starts_with(from)
        {
            log::debug!("{to} was replaced by a rename");
            self.forget_path(old);
        }
        self.by_key.remove(&key(to));
        // What this drive made moves along; what `to` was is no more.
        let (from_key, to_key) = (key(from), key(to));
        let below = format!("{from_key}\\");
        let moving: Vec<String> = self
            .made
            .iter()
            .filter(|m| **m == from_key || m.starts_with(&below))
            .cloned()
            .collect();
        self.made.remove(&to_key);
        for m in moving {
            self.made.remove(&m);
            self.made
                .insert(format!("{to_key}{}", &m[from_key.len()..]));
        }
        let mut rekeyed = Vec::new();
        for (&id, node) in self.nodes.iter_mut() {
            if !node.gone && node.path.starts_with(from) {
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
            made: HashSet::new(),
            next: FIRST,
        };
        table.nodes.insert(
            ROOT,
            Node {
                path: VPath::root(),
                open: None,
                writing: false,
                local: None,
                gone: false,
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
            Ok(dir.child(&self.respell(&dir, name)))
        }
    }

    /// `name` as spelt in folder `dir` on the unit, if it's there under another Unicode
    /// form: the front end sends names precomposed (NFC), and a file put on the card by a
    /// computer may have a decomposed name (NFD). The drive itself compares names byte for
    /// byte, apart from ASCII case.
    fn respell(&self, dir: &VPath, name: &str) -> String {
        if name.is_ascii() {
            return name.to_string();
        }
        let Ok(entries) = self.drive.list(dir) else {
            return name.to_string();
        };
        if entries.iter().any(|e| e.name.eq_ignore_ascii_case(name)) {
            return name.to_string();
        }
        let wanted = fold(name);
        entries
            .into_iter()
            .find(|e| fold(&e.name) == wanted)
            .map_or_else(|| name.to_string(), |e| e.name)
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
        {
            let table = self.table.lock().unwrap();
            if let Some(id) = table.local(&path) {
                let attr = Self::local_attr(&table.nodes[&id]);
                return Ok(Item { id, attr });
            }
        }
        let attr = self.drive.lookup(&path)?;
        let id = self.table.lock().unwrap().id(&Self::spelled(&path, &attr));
        let attr = self.open_attr(id).unwrap_or(attr);
        Ok(Item { id, attr })
    }

    /// Attributes of an open item, which may hold unsaved writes, or of an in-memory file.
    fn open_attr(&self, id: u64) -> Option<Attr> {
        let table = self.table.lock().unwrap();
        let node = table.nodes.get(&id)?;
        if node.local.is_some() {
            return Some(Self::local_attr(node));
        }
        node.open.as_ref().map(|f| f.attr())
    }

    fn local_attr(node: &Node) -> Attr {
        Attr {
            name: node.path.name().to_string(),
            is_dir: false,
            size: node.local.as_ref().map_or(0, |d| d.len() as u64),
        }
    }

    /// Run `f` on the contents of item `id` if it's an in-memory file.
    fn with_local<T>(&self, id: u64, f: impl FnOnce(&mut Vec<u8>) -> T) -> Option<T> {
        let mut table = self.table.lock().unwrap();
        table.nodes.get_mut(&id)?.local.as_mut().map(f)
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
        // An in-memory file lasts until it's deleted.
        if node.local.is_some() && table.by_key.get(&k) == Some(&id) {
            return;
        }
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
            let mut table = self.table.lock().unwrap();
            let node = table.nodes.get_mut(&id).ok_or(FsError::NotFound)?;
            if node.local.is_some() {
                return Ok(());
            }
            if node.open.is_some() {
                // Under the same lock as the check, so a last close can't come between.
                node.writing |= write;
                return Ok(());
            }
            if node.gone {
                return Err(FsError::NotFound);
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
            self.let_go(file);
        }
        saved
    }

    /// Drop a hold on an open file that the table no longer has; whoever drops the last
    /// one lets it go (saving anything left unsaved, unless the file has gone).
    fn let_go(&self, file: Arc<OpenFile>) {
        // Exactly one of several racing callers gets the file.
        if let Some(file) = Arc::into_inner(file) {
            self.drive.release(file);
        }
    }

    /// Run `f` with item `id`'s open file, or with one opened just for the call.
    fn with_file<T>(&self, id: u64, f: impl FnOnce(&OpenFile) -> FsResult<T>) -> FsResult<T> {
        let open = {
            let table = self.table.lock().unwrap();
            let node = table.nodes.get(&id).ok_or(FsError::NotFound)?;
            match &node.open {
                Some(file) => Ok(file.clone()),
                None if node.gone => return Err(FsError::NotFound),
                None => Err(node.path.clone()),
            }
        };
        match open {
            Ok(file) => {
                let result = f(&file);
                // Closed meanwhile: then this call lets it go.
                let held = {
                    let table = self.table.lock().unwrap();
                    table
                        .nodes
                        .get(&id)
                        .and_then(|n| n.open.as_ref())
                        .is_some_and(|o| Arc::ptr_eq(o, &file))
                };
                if !held {
                    self.let_go(file);
                }
                result
            }
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
        let local = self.with_local(id, |data| {
            let start = offset.min(data.len() as u64) as usize;
            let n = buf.len().min(data.len() - start);
            buf[..n].copy_from_slice(&data[start..start + n]);
            n
        });
        if let Some(n) = local {
            return Ok(n);
        }
        self.with_file(id, |file| self.drive.read(file, offset, buf))
    }

    pub fn write(&self, id: u64, offset: u64, data: &[u8]) -> FsResult<usize> {
        let local = self.with_local(id, |buf| {
            let end = offset.saturating_add(data.len() as u64);
            if self.drive.read_only() {
                return Err(FsError::ReadOnly);
            }
            if end > MAX_LOCAL as u64 {
                return Err(FsError::Refused(TOO_LARGE.into()));
            }
            let (offset, end) = (offset as usize, end as usize);
            if buf.len() < end {
                buf.resize(end, 0);
            }
            buf[offset..end].copy_from_slice(data);
            Ok(data.len())
        });
        if let Some(result) = local {
            return result;
        }
        self.with_file(id, |file| self.drive.write(file, Some(offset), data, false))
    }

    /// Change a file's length. Saved with the file's other changes when it's closed, or at
    /// once if it isn't open.
    pub fn set_size(&self, id: u64, size: u64) -> FsResult<()> {
        let local = self.with_local(id, |buf| {
            if self.drive.read_only() {
                return Err(FsError::ReadOnly);
            }
            if size > MAX_LOCAL as u64 {
                return Err(FsError::Refused(TOO_LARGE.into()));
            }
            buf.resize(size as usize, 0);
            Ok(())
        });
        if let Some(result) = local {
            return result;
        }
        self.with_file(id, |file| self.drive.set_size(file, size))
    }

    /// Create a file or folder named `name` in folder `dir`.
    pub fn create(&self, dir: u64, name: &str, is_dir: bool) -> FsResult<Item> {
        if is_mac_metadata(name) {
            if is_dir {
                return Err(FsError::AccessDenied);
            }
            return self.create_local(dir, name);
        }
        let path = self.child(dir, name)?;
        let file = self.drive.create(&path, is_dir)?;
        let attr = file.attr();
        let path = file.path();
        let id = {
            let mut table = self.table.lock().unwrap();
            if !is_dir {
                table.made.insert(key(&path));
            }
            table.id(&path)
        };
        self.drive.release(file);
        Ok(Item { id, attr })
    }

    /// A new housekeeping file named `name` in folder `dir`, kept in memory only (see
    /// [`is_mac_metadata`]).
    fn create_local(&self, dir: u64, name: &str) -> FsResult<Item> {
        if self.drive.read_only() {
            return Err(FsError::ReadOnly);
        }
        let path = self.local_path(dir, name)?;
        if self.table.lock().unwrap().local(&path).is_some() {
            return Err(FsError::Exists);
        }
        match self.drive.lookup(&path) {
            Ok(_) => return Err(FsError::Exists),
            Err(FsError::NotFound) => {}
            Err(e) => return Err(e),
        }
        let mut table = self.table.lock().unwrap();
        // A fresh number, not one an earlier file at this path still holds.
        table.by_key.remove(&key(&path));
        let id = table.id(&path);
        let node = table.nodes.get_mut(&id).expect("just numbered");
        node.local = Some(Vec::new());
        log::info!("keeping {path} in memory, not on the unit");
        Ok(Item {
            id,
            attr: Self::local_attr(node),
        })
    }

    /// Where an in-memory file named `name` goes in folder `dir`: inside a volume, in a
    /// folder that exists.
    fn local_path(&self, dir: u64, name: &str) -> FsResult<VPath> {
        let dir = self.path(dir)?;
        if dir.is_root() {
            return Err(FsError::AccessDenied);
        }
        if !self.drive.lookup(&dir)?.is_dir {
            return Err(FsError::NotADirectory);
        }
        Ok(dir.child(name))
    }

    /// Delete `name` from folder `dir`: a file, or an empty folder.
    pub fn remove(&self, dir: u64, name: &str) -> FsResult<()> {
        let path = self.child(dir, name)?;
        {
            let mut table = self.table.lock().unwrap();
            if let Some(id) = table.local(&path) {
                if self.drive.read_only() {
                    return Err(FsError::ReadOnly);
                }
                table.drop_local(id);
                return Ok(());
            }
        }
        self.delete_file(&path)
    }

    /// Delete a file, or an empty folder, from the unit.
    fn delete_file(&self, path: &VPath) -> FsResult<()> {
        let file = self.drive.open(path)?;
        let is_dir = file.is_dir();
        let result = self
            .drive
            .check_delete(&file)
            .and_then(|_| self.drive.delete(&file));
        let path = file.path();
        self.drive.release(file);
        result?;
        let mut table = self.table.lock().unwrap();
        if let Some(&id) = table.by_key.get(&key(&path)) {
            table.forget_path(id);
        }
        table.unmade(&path);
        if is_dir {
            table.drop_local_below(&path);
        }
        Ok(())
    }

    /// The whole contents of `file`, for an in-memory copy.
    fn read_all(&self, file: &OpenFile) -> FsResult<Vec<u8>> {
        if file.is_dir() {
            return Err(FsError::AccessDenied);
        }
        let size = file.size() as usize;
        if size > MAX_LOCAL {
            return Err(FsError::Refused(TOO_LARGE.into()));
        }
        let mut data = vec![0; size];
        let mut done = 0;
        while done < size {
            let n = self.drive.read(file, done as u64, &mut data[done..])?;
            if n == 0 {
                break;
            }
            done += n;
        }
        data.truncate(done);
        Ok(data)
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
        let from = self.child(from_dir, from_name)?;
        let to = self.child(to_dir, to_name)?;
        let local = self.table.lock().unwrap().local(&from);
        if let Some(id) = local {
            if self.drive.read_only() {
                return Err(FsError::ReadOnly);
            }
            if from.volume() != to.volume() {
                return Err(FsError::CrossVolume);
            }
            return if is_mac_metadata(to_name) {
                self.rename_local(id, to_dir, to_name, replace)
            } else {
                self.store_local(id, &to, replace)
            };
        }
        // Folders can't take such names, as they can't be made with them.
        if is_mac_metadata(to_name) && self.drive.lookup(&from)?.is_dir {
            return Err(FsError::AccessDenied);
        }
        if is_mac_metadata(to_name) && self.is_temporary(&from) {
            match self.drive.lookup(&to) {
                // Already on the unit: an ordinary rename over it.
                Ok(_) => {}
                Err(FsError::NotFound) => {
                    return self.take_off_unit(&from, to_dir, to_name, replace);
                }
                Err(e) => return Err(e),
            }
        }
        let file = self.drive.open(&from)?;
        let from = file.path();
        let result = self.drive.rename(&file, &to, replace);
        let moved_to = file.path();
        self.drive.release(file);
        result?;
        self.table.lock().unwrap().moved(&from, &moved_to);
        Ok(moved_to.name().to_string())
    }

    /// Rename in-memory file `id` to another housekeeping name.
    fn rename_local(&self, id: u64, to_dir: u64, to_name: &str, replace: bool) -> FsResult<String> {
        let to = self.local_path(to_dir, to_name)?;
        let other = self.table.lock().unwrap().local(&to);
        if other.is_some_and(|o| o != id) && !replace {
            return Err(FsError::Exists);
        }
        match self.drive.lookup(&to) {
            Ok(attr) if replace && !attr.is_dir => self.remove(to_dir, to_name)?,
            Ok(_) => return Err(FsError::Exists),
            Err(FsError::NotFound) => {}
            Err(e) => return Err(e),
        }
        let mut table = self.table.lock().unwrap();
        if let Some(other) = table.local(&to)
            && other != id
        {
            table.drop_local(other);
        }
        table.repoint(id, &to);
        Ok(to.name().to_string())
    }

    /// Rename in-memory file `id` to an ordinary name: it goes to the unit.
    fn store_local(&self, id: u64, to: &VPath, replace: bool) -> FsResult<String> {
        let data = self
            .with_local(id, |d| d.clone())
            .ok_or(FsError::NotFound)?;
        match self.drive.lookup(to) {
            // Deleted first, so a handle still open on it can't save it over this one.
            Ok(attr) if replace && !attr.is_dir => self.delete_file(to)?,
            Ok(_) => return Err(FsError::Exists),
            Err(FsError::NotFound) => {}
            Err(e) => return Err(e),
        }
        let file = self.drive.create(to, false)?;
        let saved = self
            .drive
            .write(&file, Some(0), &data, false)
            .and_then(|_| self.drive.flush(&file));
        let path = file.path();
        self.drive.release(file);
        saved?;
        let mut table = self.table.lock().unwrap();
        if let Some(node) = table.nodes.get_mut(&id) {
            node.local = None;
        }
        table.made.insert(key(&path));
        table.repoint(id, &path);
        Ok(path.name().to_string())
    }

    /// Whether `from` is a hidden file this drive made since it opened: a copying
    /// program's temporary file (rsync writes `.DS_Store` as `..DS_Store.XXXXXX`, then
    /// renames it). Any other file renamed to a housekeeping name is renamed on the unit
    /// as asked, so its contents are never lost.
    fn is_temporary(&self, from: &VPath) -> bool {
        // `child` spells the name as the request did; the device's spelling may differ in
        // case only, which `key` ignores.
        from.name().starts_with('.') && self.table.lock().unwrap().made.contains(&key(from))
    }

    /// Rename file `from`, on the unit, to a housekeeping name: it's taken off the unit and
    /// kept in memory, as rsync does with the `.DS_Store` it writes under a temporary name.
    /// Only for files this drive made (see [`Items::is_temporary`]).
    fn take_off_unit(
        &self,
        from: &VPath,
        to_dir: u64,
        to_name: &str,
        replace: bool,
    ) -> FsResult<String> {
        if self.drive.read_only() {
            return Err(FsError::ReadOnly);
        }
        let to = self.local_path(to_dir, to_name)?;
        if from.volume() != to.volume() {
            return Err(FsError::CrossVolume);
        }
        if self.table.lock().unwrap().local(&to).is_some() && !replace {
            return Err(FsError::Exists);
        }
        let file = self.drive.open(from)?;
        let deleted = self.read_all(&file).and_then(|data| {
            self.drive.check_delete(&file)?;
            self.drive.delete(&file)?;
            Ok(data)
        });
        let from = file.path();
        self.drive.release(file);
        let data = deleted?;
        let mut table = self.table.lock().unwrap();
        if let Some(other) = table.local(&to) {
            table.drop_local(other);
        }
        table.unmade(&from);
        let id = match table.by_key.get(&key(&from)) {
            Some(&id) => id,
            None => table.id(&from),
        };
        let open = table.nodes.get_mut(&id).and_then(|node| {
            node.writing = false;
            node.local = Some(data);
            node.open.take()
        });
        table.repoint(id, &to);
        drop(table);
        if let Some(file) = open {
            self.let_go(file);
        }
        log::info!("renamed {from} to {to}: keeping it in memory, not on the unit");
        Ok(to.name().to_string())
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
            // One still in use is saved by the drive's shutdown.
            self.let_go(file);
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
    fn keeps_new_mac_housekeeping_files_in_memory() {
        let s = sample();
        let it = items(&s);
        let import = walk(&it, &["SD-Card", "IMPORT"]);
        for name in [".DS_Store", "._Kick.wav"] {
            let f = it.create(import.id, name, false).unwrap();
            it.open(f.id, true).unwrap();
            assert_eq!(it.write(f.id, 0, b"finder").unwrap(), 6);
            assert_eq!(it.write(f.id, 6, b" info").unwrap(), 5);
            it.close(f.id, false, false).unwrap();
            // Never on the unit, and not listed; but there for whoever asks by name.
            assert_eq!(s.get(&format!("SD:IMPORT/{name}")), None, "{name}");
            assert!(
                !it.list(import.id)
                    .unwrap()
                    .iter()
                    .any(|i| i.attr.name == name)
            );
            let found = it.lookup(import.id, &name.to_uppercase()).unwrap();
            assert_eq!(found.id, f.id);
            assert_eq!(found.attr.size, 11);
            let mut buf = [0u8; 32];
            assert_eq!(it.read(f.id, 0, &mut buf).unwrap(), 11);
            assert_eq!(&buf[..11], b"finder info");
            assert_eq!(it.create(import.id, name, false), Err(FsError::Exists));
            it.set_size(f.id, 3).unwrap();
            assert_eq!(it.attr(f.id).unwrap().size, 3);
            // Kept when FSKit lets the item go, until deleted.
            it.reclaim(f.id);
            assert_eq!(it.lookup(import.id, name).unwrap().id, f.id);
            it.remove(import.id, name).unwrap();
            assert_eq!(it.lookup(import.id, name), Err(FsError::NotFound));
        }
        assert_eq!(s.open_handles(), 0);
        // Folders, and the root, still refuse them.
        assert_eq!(
            it.create(import.id, ".Trashes", true),
            Err(FsError::AccessDenied)
        );
        assert_eq!(
            it.create(ROOT, ".DS_Store", false),
            Err(FsError::AccessDenied)
        );
    }

    #[test]
    fn existing_housekeeping_files_on_the_unit_are_ordinary() {
        let s = Arc::new(MemStore::new(&[
            ("SD:IMPORT", None),
            ("SD:IMPORT/.DS_Store", Some(b"old")),
        ]));
        let it = items(&s);
        let import = walk(&it, &["SD-Card", "IMPORT"]);
        assert_eq!(
            it.create(import.id, ".DS_Store", false),
            Err(FsError::Exists)
        );
        let ds = it.lookup(import.id, ".DS_Store").unwrap();
        assert_eq!(ds.attr.size, 3);
        assert!(it.list(import.id).unwrap().iter().any(|i| i.id == ds.id));
        it.remove(import.id, ".DS_Store").unwrap();
        assert_eq!(s.get("SD:IMPORT/.DS_Store"), None);
    }

    #[test]
    fn renaming_to_and_from_housekeeping_names() {
        let s = sample();
        let it = items(&s);
        let import = walk(&it, &["SD-Card", "IMPORT"]);
        // rsync writes `..DS_Store.XXXX`, then renames it into place.
        let tmp = it.create(import.id, "..DS_Store.tmp", false).unwrap();
        it.open(tmp.id, true).unwrap();
        it.write(tmp.id, 0, b"bud1").unwrap();
        it.close(tmp.id, false, false).unwrap();
        assert_eq!(
            s.get("SD:IMPORT/..DS_Store.tmp"),
            Some(Some(b"bud1".to_vec()))
        );
        let name = it
            .rename(import.id, "..DS_Store.tmp", import.id, ".DS_Store", false)
            .unwrap();
        assert_eq!(name, ".DS_Store");
        assert_eq!(s.get("SD:IMPORT/..DS_Store.tmp"), None);
        assert_eq!(s.get("SD:IMPORT/.DS_Store"), None);
        let ds = it.lookup(import.id, ".DS_Store").unwrap();
        assert_eq!(ds.id, tmp.id, "the renamed item keeps its number");
        assert_eq!(ds.attr.size, 4);
        // Between housekeeping names, and over another one only when replacing.
        let other = it.create(import.id, "._x", false).unwrap();
        assert_eq!(
            it.rename(import.id, ".DS_Store", import.id, "._x", false),
            Err(FsError::Exists)
        );
        it.rename(import.id, ".DS_Store", import.id, "._x", true)
            .unwrap();
        assert_eq!(it.lookup(import.id, "._x").unwrap().id, tmp.id);
        assert_ne!(it.lookup(import.id, "._x").unwrap().id, other.id);
        assert_eq!(it.lookup(import.id, ".DS_Store"), Err(FsError::NotFound));
        // To an ordinary name: it goes to the unit.
        let name = it
            .rename(import.id, "._x", import.id, "kept.bin", false)
            .unwrap();
        assert_eq!(name, "kept.bin");
        assert_eq!(s.get("SD:IMPORT/kept.bin"), Some(Some(b"bud1".to_vec())));
        assert_eq!(it.lookup(import.id, "kept.bin").unwrap().id, tmp.id);
        // A folder can't take such a name, and nothing moves to Internal.
        assert_eq!(
            it.rename(import.id, "pack", import.id, ".Trashes", false),
            Err(FsError::AccessDenied)
        );
        let internal = it.lookup(ROOT, "Internal").unwrap();
        assert_eq!(
            it.rename(import.id, "kept.bin", internal.id, ".DS_Store", false),
            Err(FsError::CrossVolume)
        );
        assert_eq!(s.get("SD:IMPORT/kept.bin"), Some(Some(b"bud1".to_vec())));
        assert_eq!(s.open_handles(), 0);
    }

    #[test]
    fn deleting_a_folder_forgets_housekeeping_files_in_it() {
        let s = sample();
        let it = items(&s);
        let card = it.lookup(ROOT, "SD-Card").unwrap();
        let dir = it.create(card.id, "new", true).unwrap();
        let ds = it.create(dir.id, ".DS_Store", false).unwrap();
        it.write(ds.id, 0, b"x").unwrap();
        // Empty on the unit, so it can go.
        it.remove(card.id, "new").unwrap();
        assert_eq!(s.get("SD:new"), None);
        let dir = it.create(card.id, "new", true).unwrap();
        assert_eq!(it.lookup(dir.id, ".DS_Store"), Err(FsError::NotFound));
        // A renamed folder takes them along.
        it.create(dir.id, "._a", false).unwrap();
        it.rename(card.id, "new", card.id, "newer", false).unwrap();
        let newer = it.lookup(card.id, "newer").unwrap();
        assert!(it.lookup(newer.id, "._a").is_ok());
    }

    #[test]
    fn read_only_refuses_housekeeping_files_too() {
        let s = sample();
        let it = Items::new(Arc::new(Drive::new(
            s.clone(),
            Options {
                read_only: true,
                ..Options::default()
            },
        )));
        let import = walk(&it, &["SD-Card", "IMPORT"]);
        assert_eq!(
            it.create(import.id, ".DS_Store", false),
            Err(FsError::ReadOnly)
        );
    }

    #[test]
    fn a_file_replaced_by_a_rename_is_not_saved_over_its_replacement() {
        let s = sample();
        let it = items(&s);
        let import = walk(&it, &["SD-Card", "IMPORT"]);
        let a = it.create(import.id, "A.wav", false).unwrap();
        it.open(a.id, true).unwrap();
        it.write(a.id, 0, b"aaaa").unwrap();
        it.close(a.id, false, false).unwrap();
        // B.wav is open for writing, with unsaved writes, and another reader.
        let b = it.create(import.id, "B.wav", false).unwrap();
        it.open(b.id, true).unwrap();
        it.write(b.id, 0, b"bbbbbbbb").unwrap();
        // `mv A.wav B.wav`
        it.rename(import.id, "A.wav", import.id, "B.wav", true)
            .unwrap();
        assert_eq!(s.get("SD:IMPORT/B.wav"), Some(Some(b"aaaa".to_vec())));
        // The old B's handle still works, in memory only.
        let mut buf = [0u8; 16];
        assert_eq!(it.read(b.id, 0, &mut buf).unwrap(), 8);
        assert_eq!(&buf[..8], b"bbbbbbbb");
        assert_eq!(it.write(b.id, 8, b"!").unwrap(), 1);
        it.close(b.id, false, false).unwrap();
        assert_eq!(s.get("SD:IMPORT/B.wav"), Some(Some(b"aaaa".to_vec())));
        // The name leads to the renamed file, under A's number.
        let found = it.lookup(import.id, "B.wav").unwrap();
        assert_eq!(found.id, a.id);
        assert_eq!(found.attr.size, 4);
        // The old B is gone for good.
        assert_eq!(it.attr(b.id), Err(FsError::NotFound));
        assert_eq!(it.read(b.id, 0, &mut buf), Err(FsError::NotFound));
        assert_eq!(s.open_handles(), 0);
    }

    #[test]
    fn a_reader_of_a_replaced_file_never_reads_its_replacement() {
        let s = sample();
        let it = items(&s);
        let import = walk(&it, &["SD-Card", "IMPORT"]);
        let kick = it.lookup(import.id, "Kick.wav").unwrap();
        it.open(kick.id, false).unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(it.read(kick.id, 0, &mut buf).unwrap(), 8);
        let pack = it.lookup(import.id, "pack").unwrap();
        it.rename(pack.id, "snare.wav", import.id, "Kick.wav", true)
            .unwrap();
        assert_eq!(s.get("SD:IMPORT/Kick.wav"), Some(Some(b"snare".to_vec())));
        // Its contents weren't in memory, and the unit no longer has them.
        assert!(matches!(it.read(kick.id, 0, &mut buf), Err(FsError::Io(_))));
        assert!(matches!(it.write(kick.id, 0, b"x"), Err(FsError::Io(_))));
        it.close(kick.id, false, false).unwrap();
        assert_eq!(s.get("SD:IMPORT/Kick.wav"), Some(Some(b"snare".to_vec())));
        assert_eq!(s.open_handles(), 0);
    }

    #[test]
    fn a_handle_on_a_deleted_file_leaves_a_new_file_there_alone() {
        let s = sample();
        let it = items(&s);
        let import = walk(&it, &["SD-Card", "IMPORT"]);
        let old = it.create(import.id, "B.wav", false).unwrap();
        it.open(old.id, true).unwrap();
        it.write(old.id, 0, b"OLD!").unwrap();
        it.close(old.id, false, false).unwrap();
        it.open(old.id, true).unwrap();
        // Deleted while open, then made again and written by someone else.
        it.remove(import.id, "B.wav").unwrap();
        let new = it.create(import.id, "B.wav", false).unwrap();
        assert_ne!(new.id, old.id);
        it.open(new.id, true).unwrap();
        it.write(new.id, 0, b"NEWNEW").unwrap();
        it.close(new.id, false, false).unwrap();
        // The old handle writes and closes.
        assert!(it.write(old.id, 0, b"x").is_err());
        it.close(old.id, false, false).unwrap();
        assert_eq!(s.get("SD:IMPORT/B.wav"), Some(Some(b"NEWNEW".to_vec())));
        assert_eq!(it.lookup(import.id, "B.wav").unwrap().id, new.id);
        assert_eq!(s.open_handles(), 0);
    }

    #[test]
    fn a_file_deleted_while_open_stays_deleted() {
        let s = sample();
        let it = items(&s);
        let import = walk(&it, &["SD-Card", "IMPORT"]);
        // mkstemp, unlink, then use it as scratch space.
        let tmp = it.create(import.id, "tmp.bin", false).unwrap();
        it.open(tmp.id, true).unwrap();
        it.remove(import.id, "tmp.bin").unwrap();
        assert_eq!(s.get("SD:IMPORT/tmp.bin"), None);
        assert_eq!(it.write(tmp.id, 0, b"scratch").unwrap(), 7);
        it.set_size(tmp.id, 5).unwrap();
        assert_eq!(it.attr(tmp.id).unwrap().size, 5);
        let mut buf = [0u8; 16];
        assert_eq!(it.read(tmp.id, 0, &mut buf).unwrap(), 5);
        assert_eq!(&buf[..5], b"scrat");
        it.close(tmp.id, false, false).unwrap();
        assert_eq!(s.get("SD:IMPORT/tmp.bin"), None);
        assert_eq!(it.lookup(import.id, "tmp.bin"), Err(FsError::NotFound));
        assert_eq!(it.attr(tmp.id), Err(FsError::NotFound));
        assert_eq!(s.open_handles(), 0);
    }

    #[test]
    fn writes_buffered_before_a_delete_stay_readable() {
        let s = sample();
        let it = items(&s);
        let import = walk(&it, &["SD-Card", "IMPORT"]);
        let kick = it.lookup(import.id, "Kick.wav").unwrap();
        it.open(kick.id, true).unwrap();
        it.write(kick.id, 0, b"KICK").unwrap();
        it.remove(import.id, "Kick.wav").unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(it.read(kick.id, 0, &mut buf).unwrap(), 8);
        assert_eq!(&buf[..8], b"KICK....");
        it.close(kick.id, false, false).unwrap();
        assert_eq!(s.get("SD:IMPORT/Kick.wav"), None);
    }

    #[test]
    fn existing_files_renamed_to_housekeeping_names_stay_on_the_unit() {
        let s = sample();
        let it = items(&s);
        let import = walk(&it, &["SD-Card", "IMPORT"]);
        // A file that was there before the drive opened.
        assert_eq!(
            it.rename(import.id, "Kick.wav", import.id, "._Kick.wav", false)
                .unwrap(),
            "._Kick.wav"
        );
        assert_eq!(
            s.get("SD:IMPORT/._Kick.wav"),
            Some(Some(b"RIFF....".to_vec()))
        );
        // Made here, but not hidden: not a copying program's temporary file either.
        let song = it.create(import.id, "song.wav", false).unwrap();
        it.write(song.id, 0, b"song").unwrap();
        it.rename(import.id, "song.wav", import.id, ".apdisk", false)
            .unwrap();
        assert_eq!(s.get("SD:IMPORT/.apdisk"), Some(Some(b"song".to_vec())));
        // Made here and hidden, then moved: still taken off the unit when renamed.
        let tmp = it.create(import.id, ".x.tmp", false).unwrap();
        it.write(tmp.id, 0, b"ds").unwrap();
        let pack = it.lookup(import.id, "pack").unwrap();
        it.rename(import.id, ".x.tmp", pack.id, ".y.tmp", false)
            .unwrap();
        it.rename(pack.id, ".y.tmp", pack.id, ".DS_Store", false)
            .unwrap();
        assert_eq!(s.get("SD:IMPORT/pack/.DS_Store"), None);
        assert_eq!(s.get("SD:IMPORT/pack/.y.tmp"), None);
        assert_eq!(it.lookup(pack.id, ".DS_Store").unwrap().attr.size, 2);
        // Made here inside a folder that is then renamed: still ours.
        let card = it.lookup(ROOT, "SD-Card").unwrap();
        let dir = it.create(card.id, "copying", true).unwrap();
        let tmp = it.create(dir.id, "..DS_Store.abc", false).unwrap();
        it.write(tmp.id, 0, b"bud1").unwrap();
        it.rename(card.id, "copying", card.id, "copied", false)
            .unwrap();
        let dir = it.lookup(card.id, "copied").unwrap();
        it.rename(dir.id, "..DS_Store.abc", dir.id, ".DS_Store", false)
            .unwrap();
        assert_eq!(s.get("SD:copied/.DS_Store"), None);
        assert_eq!(s.get("SD:copied/..DS_Store.abc"), None);
        // Deleted and made again by someone else (over the link, say): not ours any more.
        let other = it.create(dir.id, ".other", false).unwrap();
        it.remove(dir.id, ".other").unwrap();
        s.nodes
            .lock()
            .unwrap()
            .insert("SD:copied/.other".into(), Some(b"theirs".to_vec()));
        it.drive().forget_cache();
        assert_ne!(it.lookup(dir.id, ".other").unwrap().id, other.id);
        it.rename(dir.id, ".other", dir.id, "._other", false)
            .unwrap();
        assert_eq!(s.get("SD:copied/._other"), Some(Some(b"theirs".to_vec())));
        assert_eq!(s.open_handles(), 0);
    }

    #[test]
    fn replacing_an_open_file_with_a_housekeeping_file_leaves_no_stale_save() {
        let s = sample();
        let it = items(&s);
        let import = walk(&it, &["SD-Card", "IMPORT"]);
        let kick = it.lookup(import.id, "Kick.wav").unwrap();
        it.open(kick.id, true).unwrap();
        it.write(kick.id, 0, b"STALE").unwrap();
        let ds = it.create(import.id, "._x", false).unwrap();
        it.write(ds.id, 0, b"fresh").unwrap();
        it.rename(import.id, "._x", import.id, "Kick.wav", true)
            .unwrap();
        assert_eq!(s.get("SD:IMPORT/Kick.wav"), Some(Some(b"fresh".to_vec())));
        it.close(kick.id, false, false).unwrap();
        assert_eq!(s.get("SD:IMPORT/Kick.wav"), Some(Some(b"fresh".to_vec())));
        assert_eq!(it.lookup(import.id, "kick.wav").unwrap().id, ds.id);
    }

    #[test]
    fn finds_decomposed_names_from_precomposed_ones() {
        // "Café.wav" with a decomposed é, as a Mac may write it to a card.
        let nfd = "Cafe\u{301}.wav";
        let s = Arc::new(MemStore::new(&[
            ("SD:IMPORT", None),
            (&format!("SD:IMPORT/{nfd}"), Some(b"cafe")),
            ("SD:IMPORT/\u{c9}t\u{e9}.wav", Some(b"upper")),
            ("SD:IMPORT/\u{e9}t\u{e9}.wav", Some(b"lower")),
        ]));
        let it = items(&s);
        let import = walk(&it, &["SD-Card", "IMPORT"]);
        let found = it.lookup(import.id, "Caf\u{e9}.WAV").unwrap();
        assert_eq!(found.attr.name, nfd);
        assert_eq!(it.lookup(import.id, nfd).unwrap().id, found.id);
        assert!(it.list(import.id).unwrap().iter().any(|i| i.id == found.id));
        let mut buf = [0u8; 8];
        assert_eq!(it.read(found.id, 0, &mut buf).unwrap(), 4);
        // Only ASCII letters match without regard to case, as on the drive.
        let upper = it.lookup(import.id, "\u{c9}t\u{e9}.wav").unwrap();
        let lower = it.lookup(import.id, "\u{e9}t\u{e9}.wav").unwrap();
        assert_ne!(upper.id, lower.id);
        assert_eq!(upper.attr.size, 5);
        assert_eq!(lower.attr.size, 5);
        assert_eq!(it.read(lower.id, 0, &mut buf).unwrap(), 5);
        assert_eq!(&buf[..5], b"lower");
        // Deleting by the precomposed name deletes the decomposed file.
        it.remove(import.id, "Caf\u{e9}.wav").unwrap();
        assert_eq!(s.get(&format!("SD:IMPORT/{nfd}")), None);
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
