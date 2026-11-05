import Foundation
import FSKit
import SP404Core

func posix(_ code: Int32) -> any Error { fs_errorForPOSIXError(code) }

/// Names go to the core precomposed (NFC), as Windows passes them; some macOS programs
/// pass decomposed names. The core finds a name stored decomposed on the card from its
/// precomposed form (tested on the unit).
func coreName(_ name: FSFileName) -> String? {
    name.string?.precomposedStringWithCanonicalMapping
}

/// The string in a NUL-terminated buffer the core filled.
func string(_ buffer: [CChar]) -> String {
    String(decoding: buffer.prefix(while: { $0 != 0 }).map { UInt8(bitPattern: $0) }, as: UTF8.self)
}

/// A listing entry, collected from the core's callback.
private struct Entry {
    let name: String
    let attr: SpAttr
}

private final class Listing {
    var entries: [Entry] = []
}

private let collect: SpListFn = { context, name, attr in
    guard let context, let name, let attr else { return false }
    let listing = Unmanaged<Listing>.fromOpaque(context).takeUnretainedValue()
    listing.entries.append(Entry(name: String(cString: name), attr: attr.pointee))
    return false
}

/// Directory cookies: the position of the next entry, and the low 32 bits of the number
/// of the entry before it. If the folder changed between two calls, the number finds the
/// place again; if that entry has gone, the position is used as is.
private func cookie(position: Int, after id: UInt64) -> FSDirectoryCookie {
    FSDirectoryCookie(rawValue: UInt64(position) << 32 | (id & 0xFFFF_FFFF))
}

/// The SP-404MKII's volume: the root holds `Internal` and `SD-Card`. Every call goes to the
/// core (sp404.h), which numbers items, caches listings, shares open files and uploads a
/// written file when its last writer closes it.
final class Volume: FSVolume, FSVolume.Operations, FSVolume.PathConfOperations,
    FSVolume.OpenCloseOperations, FSVolume.ReadWriteOperations, FSVolume.XattrOperations,
    @unchecked Sendable
{
    static let uuid = UUID(uuidString: "5B4F0404-2021-4E55-8D2B-53503430344D")!

    /// Fixed when the volume is activated (where the mount's options arrive), before any
    /// other call; kept under `stateLock`, since every call reads them.
    private let stateLock = NSLock()
    private var _readOnly: Bool
    private var _time = timespec()
    var readOnly: Bool { stateLock.withLock { _readOnly } }
    var time: timespec { stateLock.withLock { _time } }
    let root: Item
    private weak var fileSystem: SP404FileSystem?
    /// From the resource's URL (see `MountSettings`).
    let settings: MountSettings

    /// The core's drive; nil until opened and once closed. Calls hold `driveLock` for
    /// reading while they use it, so closing (which holds it for writing) waits for them.
    private var drive: OpaquePointer?
    private let driveLock: UnsafeMutablePointer<pthread_rwlock_t>
    private var watch: Unmanaged<UnitWatch>?

    /// One `Item` per item number, while FSKit holds it. Lookups, listings, creation and
    /// reclaiming hold `itemsLock` across their core call, so a number reclaimed in the
    /// core is never handed out again through a stale object.
    private let itemsLock = NSLock()
    private var items: [UInt64: Item] = [:]

    var requestedMountOptions: FSVolume.MountOptions = []

    private var _activated = false
    /// Whether FSKit has activated the volume since loadResource (under `stateLock`).
    var activated: Bool { stateLock.withLock { _activated } }

    init(fileSystem: SP404FileSystem, settings: MountSettings, readOnly: Bool) {
        self.fileSystem = fileSystem
        self.settings = settings
        _readOnly = readOnly
        driveLock = .allocate(capacity: 1)
        pthread_rwlock_init(driveLock, nil)
        root = Item(id: UInt64(SP404_ROOT), isDir: true, parent: FSItem.Identifier.parentOfRoot.rawValue)
        super.init(
            volumeID: FSVolume.Identifier(uuid: Volume.uuid),
            volumeName: FSFileName(string: "SP-404MKII"))
        items[root.id] = root
    }

    deinit {
        closeDrive()
        pthread_rwlock_destroy(driveLock)
        driveLock.deallocate()
    }

    // MARK: The drive

    /// Run `body` with the drive, or return ENXIO if it's closed.
    private func withDrive(_ body: (OpaquePointer) -> Int32) -> Int32 {
        pthread_rwlock_rdlock(driveLock)
        defer { pthread_rwlock_unlock(driveLock) }
        guard let d = drive else { return ENXIO }
        return body(d)
    }

    /// Connect to the unit (the port in the settings, or the first SP-404MKII found) and
    /// open the drive.
    func openDrive(readOnly: Bool) -> (any Error)? {
        let watch = UnitWatch()
        watch.fileSystem = fileSystem
        let context = Unmanaged.passRetained(watch)
        let errLen = 512
        var err = [CChar](repeating: 0, count: errLen)
        let opened = withOptionalCString(settings.port) { port in
            sp_drive_open(port, readOnly, settings.cacheSecs, unitGone, context.toOpaque(), &err, errLen)
        }
        guard let d = opened else {
            context.release()
            let reason = string(err)
            log.error("could not open the drive: \(reason, privacy: .public)")
            return posix(openErrorCode(reason))
        }
        // A new drive numbers its items afresh, so items from an earlier one (if FSKit
        // activates the volume again after a deactivate) must not be handed out for them.
        itemsLock.lock()
        pthread_rwlock_wrlock(driveLock)
        let old = drive
        drive = d
        let oldWatch = self.watch
        self.watch = context
        stateLock.withLock {
            _readOnly = readOnly
            _time = timespec(tv_sec: Int(sp_drive_time(d)), tv_nsec: 0)
        }
        items = [root.id: root]
        pthread_rwlock_unlock(driveLock)
        itemsLock.unlock()
        if let old {
            log.error("opened twice; closing the first drive")
            sp_drive_close(old)
            oldWatch?.release()
        }
        return nil
    }

    /// Apply the mount's read-only option: the extension and the core refuse changes,
    /// and FSKit is asked to mount read-only, so the kernel refuses them first.
    private func setReadOnly(_ readOnly: Bool) {
        pthread_rwlock_wrlock(driveLock)
        stateLock.withLock { _readOnly = readOnly }
        if let d = drive {
            sp_drive_set_read_only(d, readOnly)
        }
        requestedMountOptions = readOnly ? [.readOnly] : []
        pthread_rwlock_unlock(driveLock)
    }

    var isOpen: Bool {
        pthread_rwlock_rdlock(driveLock)
        defer { pthread_rwlock_unlock(driveLock) }
        return drive != nil
    }

    /// Save everything, wait out the unit's session (up to 3 s) and let the port go.
    func closeDrive() {
        pthread_rwlock_wrlock(driveLock)
        let d = drive
        drive = nil
        let w = watch
        watch = nil
        pthread_rwlock_unlock(driveLock)
        guard let d else { return }
        let start = Date()
        sp_drive_close(d)
        // The core can't call back any more.
        w?.release()
        log.notice("drive closed in \(Date().timeIntervalSince(start), format: .fixed(precision: 2)) s")
    }

    // MARK: Items and attributes

    /// The `Item` for number `attr.id`, found in folder `parent`. Call with `itemsLock`.
    private func obtain(_ attr: SpAttr, parent: UInt64) -> Item {
        if let item = items[attr.id] {
            item.parent = parent
            return item
        }
        let item = Item(id: attr.id, isDir: attr.is_dir, parent: parent)
        items[attr.id] = item
        return item
    }

    private func attributes(_ attr: SpAttr, parent: UInt64) -> FSItem.Attributes {
        let a = FSItem.Attributes()
        let write: UInt32 = readOnly ? 0 : 0o200
        if attr.is_dir {
            a.type = .directory
            a.mode = UInt32(S_IFDIR) | 0o555 | write
            a.linkCount = 2
            a.size = 0
            a.allocSize = 0
        } else {
            a.type = .file
            a.mode = UInt32(S_IFREG) | 0o444 | write
            a.linkCount = 1
            a.size = attr.size
            a.allocSize = (attr.size + 4095) / 4096 * 4096
        }
        a.uid = getuid()
        a.gid = getgid()
        a.flags = 0
        a.fileID = FSItem.Identifier(rawValue: attr.id)!
        a.parentID = FSItem.Identifier(rawValue: parent)!
        a.modifyTime = time
        a.changeTime = time
        a.accessTime = time
        a.birthTime = time
        return a
    }

    private func currentAttributes(_ item: Item) -> (FSItem.Attributes?, Int32) {
        var attr = SpAttr()
        let e = withDrive { sp_getattr($0, item.id, &attr) }
        if e != 0 { return (nil, e) }
        return (attributes(attr, parent: item.parent), 0)
    }

    // MARK: Path configuration and capabilities

    var maximumLinkCount: Int { 1 }
    var maximumNameLength: Int { 255 }
    var restrictsOwnershipChanges: Bool { true }
    var truncatesLongNames: Bool { false }
    var maximumXattrSize: Int { 4 << 20 }
    var maximumFileSize: UInt64 { UInt64(UInt32.max) }

    var supportedVolumeCapabilities: FSVolume.SupportedCapabilities {
        let c = FSVolume.SupportedCapabilities()
        c.supports64BitObjectIDs = true
        c.supportsHiddenFiles = false
        c.supportsSymbolicLinks = false
        c.supportsHardLinks = false
        c.supportsSparseFiles = false
        c.doesNotSupportRootTimes = true
        c.doesNotSupportImmutableFiles = true
        c.doesNotSupportSettingFilePermissions = true
        c.caseFormat = .insensitiveCasePreserving
        return c
    }

    /// The internal storage's size and free space; the card's aren't available over USB.
    var volumeStatistics: FSStatFSResult {
        let s = FSStatFSResult(fileSystemTypeName: "sp404")
        var total: UInt64 = 0
        var free: UInt64 = 0
        _ = withDrive { d in
            sp_space(d, &total, &free)
            return 0
        }
        s.blockSize = 4096
        s.ioSize = 256 * 1024
        s.totalBytes = total
        s.freeBytes = free
        s.availableBytes = free
        s.usedBytes = total - min(free, total)
        return s
    }

    // MARK: Lifecycle

    func activate(options: FSTaskOptions, replyHandler: @escaping (FSItem?, (any Error)?) -> Void) {
        let args = options.taskOptions
        let readOnly = self.readOnly || wantsReadOnly(args)
        log.notice("activate: options \(args, privacy: .public), read-only \(readOnly)")
        // Opened by loadResource; again here only if FSKit activates the volume a second
        // time after a deactivate.
        if !isOpen, let error = openDrive(readOnly: readOnly) {
            return replyHandler(nil, error)
        }
        setReadOnly(readOnly)
        stateLock.withLock { _activated = true }
        replyHandler(root, nil)
    }

    func deactivate(options: FSDeactivateOptions = [], replyHandler: @escaping ((any Error)?) -> Void) {
        log.notice("deactivate")
        // Normally closed already, on unmount.
        closeDrive()
        replyHandler(nil)
    }

    func mount(options: FSTaskOptions, replyHandler: @escaping ((any Error)?) -> Void) {
        log.notice("mount: options \(options.taskOptions, privacy: .public)")
        replyHandler(nil)
    }

    func unmount(replyHandler: @escaping () -> Void) {
        log.notice("unmount")
        closeDrive()
        replyHandler()
    }

    func synchronize(flags: FSSyncFlags, replyHandler: @escaping ((any Error)?) -> Void) {
        log.debug("synchronize: flags \(flags.rawValue)")
        if readOnly { return replyHandler(nil) }
        let e = withDrive { sp_sync($0) }
        replyHandler(e == 0 || e == ENXIO ? nil : posix(e))
    }

    // MARK: Items

    func getAttributes(
        _ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem,
        replyHandler: @escaping (FSItem.Attributes?, (any Error)?) -> Void
    ) {
        guard let item = item as? Item else { return replyHandler(nil, posix(EINVAL)) }
        let (attrs, e) = currentAttributes(item)
        replyHandler(attrs, e == 0 ? nil : posix(e))
    }

    /// Only the size is stored. Times, permissions, owner and flags are accepted and
    /// ignored: the unit keeps none of them.
    func setAttributes(
        _ newAttributes: FSItem.SetAttributesRequest, on item: FSItem,
        replyHandler: @escaping (FSItem.Attributes?, (any Error)?) -> Void
    ) {
        guard let item = item as? Item else { return replyHandler(nil, posix(EINVAL)) }
        if readOnly { return replyHandler(nil, posix(EROFS)) }
        var consumed: FSItem.Attribute = []
        if newAttributes.isValid(.size) {
            if !item.isDir {
                log.debug("set size: item \(item.id) size \(newAttributes.size)")
                let e = withDrive { sp_set_size($0, item.id, newAttributes.size) }
                if e != 0 { return replyHandler(nil, posix(e)) }
            }
            consumed.insert(.size)
        }
        let ignored: [FSItem.Attribute] = [
            .mode, .uid, .gid, .flags, .accessTime, .modifyTime, .changeTime, .birthTime,
            .backupTime, .addedTime,
        ]
        for a in ignored where newAttributes.isValid(a) {
            consumed.insert(a)
        }
        newAttributes.consumedAttributes = consumed
        let (attrs, e) = currentAttributes(item)
        replyHandler(attrs, e == 0 ? nil : posix(e))
    }

    func lookupItem(
        named name: FSFileName, inDirectory directory: FSItem,
        replyHandler: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void
    ) {
        guard let dir = directory as? Item else { return replyHandler(nil, nil, posix(EINVAL)) }
        guard let name = coreName(name) else { return replyHandler(nil, nil, posix(ENOENT)) }
        var attr = SpAttr()
        var spelled = [CChar](repeating: 0, count: 1024)
        itemsLock.lock()
        let e = withDrive { sp_lookup($0, dir.id, name, &attr, &spelled, spelled.count) }
        let found = e == 0 ? obtain(attr, parent: dir.id) : nil
        itemsLock.unlock()
        guard let found else { return replyHandler(nil, nil, posix(e)) }
        replyHandler(found, FSFileName(string: string(spelled)), nil)
    }

    func reclaimItem(_ item: FSItem, replyHandler: @escaping ((any Error)?) -> Void) {
        guard let item = item as? Item else { return replyHandler(nil) }
        if item.id != root.id {
            itemsLock.withLock {
                // Only the current object for its number: one left from an earlier drive
                // (see openDrive) would reclaim an unrelated item.
                guard items[item.id] === item else { return }
                items[item.id] = nil
                _ = withDrive { d in
                    sp_reclaim(d, item.id)
                    return 0
                }
            }
        }
        replyHandler(nil)
    }

    func readSymbolicLink(_ item: FSItem, replyHandler: @escaping (FSFileName?, (any Error)?) -> Void) {
        replyHandler(nil, posix(EINVAL))
    }

    func createItem(
        named name: FSFileName, type: FSItem.ItemType, inDirectory directory: FSItem,
        attributes newAttributes: FSItem.SetAttributesRequest,
        replyHandler: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void
    ) {
        guard let dir = directory as? Item else { return replyHandler(nil, nil, posix(EINVAL)) }
        if readOnly { return replyHandler(nil, nil, posix(EROFS)) }
        let isDir: Bool
        switch type {
        case .file: isDir = false
        case .directory: isDir = true
        default: return replyHandler(nil, nil, posix(ENOTSUP))
        }
        guard let name = coreName(name) else { return replyHandler(nil, nil, posix(EINVAL)) }
        var attr = SpAttr()
        itemsLock.lock()
        let e = withDrive { sp_create($0, dir.id, name, isDir, &attr) }
        let created = e == 0 ? obtain(attr, parent: dir.id) : nil
        itemsLock.unlock()
        guard let created else { return replyHandler(nil, nil, posix(e)) }
        newAttributes.consumedAttributes = []
        replyHandler(created, FSFileName(string: name), nil)
    }

    func createSymbolicLink(
        named name: FSFileName, inDirectory directory: FSItem,
        attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName,
        replyHandler: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void
    ) {
        replyHandler(nil, nil, posix(readOnly ? EROFS : ENOTSUP))
    }

    func createLink(
        to item: FSItem, named name: FSFileName, inDirectory directory: FSItem,
        replyHandler: @escaping (FSFileName?, (any Error)?) -> Void
    ) {
        replyHandler(nil, posix(readOnly ? EROFS : ENOTSUP))
    }

    func removeItem(
        _ item: FSItem, named name: FSFileName, fromDirectory directory: FSItem,
        replyHandler: @escaping ((any Error)?) -> Void
    ) {
        guard let dir = directory as? Item else { return replyHandler(posix(EINVAL)) }
        if readOnly { return replyHandler(posix(EROFS)) }
        guard let name = coreName(name) else { return replyHandler(posix(ENOENT)) }
        let e = withDrive { sp_remove($0, dir.id, name) }
        replyHandler(e == 0 ? nil : posix(e))
    }

    func renameItem(
        _ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName,
        to destinationName: FSFileName, inDirectory destinationDirectory: FSItem,
        overItem: FSItem?, replyHandler: @escaping (FSFileName?, (any Error)?) -> Void
    ) {
        guard let item = item as? Item, let from = sourceDirectory as? Item,
            let to = destinationDirectory as? Item
        else { return replyHandler(nil, posix(EINVAL)) }
        if readOnly { return replyHandler(nil, posix(EROFS)) }
        guard let fromName = coreName(sourceName), let toName = coreName(destinationName) else {
            return replyHandler(nil, posix(EINVAL))
        }
        var stored = [CChar](repeating: 0, count: 1024)
        let e = withDrive {
            sp_rename($0, from.id, fromName, to.id, toName, overItem != nil, &stored, stored.count)
        }
        if e != 0 { return replyHandler(nil, posix(e)) }
        item.parent = to.id
        replyHandler(FSFileName(string: string(stored)), nil)
    }

    /// Entries in the core's order. Without attributes, "." and ".." come first.
    func enumerateDirectory(
        _ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier,
        attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker,
        replyHandler: @escaping (FSDirectoryVerifier, (any Error)?) -> Void
    ) {
        guard let dir = directory as? Item else { return replyHandler(verifier, posix(EINVAL)) }
        guard dir.isDir else { return replyHandler(verifier, posix(ENOTDIR)) }
        let listing = Listing()
        let context = Unmanaged.passUnretained(listing).toOpaque()
        itemsLock.lock()
        let e = withDrive { sp_list($0, dir.id, collect, context) }
        itemsLock.unlock()
        if e != 0 { return replyHandler(verifier, posix(e)) }

        var entries: [Entry] = []
        if attributes == nil {
            let parent = dir.id == root.id ? root.id : dir.parent
            entries.append(Entry(name: ".", attr: SpAttr(id: dir.id, size: 0, is_dir: true)))
            entries.append(Entry(name: "..", attr: SpAttr(id: parent, size: 0, is_dir: true)))
        }
        entries += listing.entries

        // Find the place to continue from.
        var start = 0
        if cookie != .initial {
            let position = Int(cookie.rawValue >> 32)
            let last = cookie.rawValue & 0xFFFF_FFFF
            if position >= 1, position <= entries.count, entries[position - 1].attr.id & 0xFFFF_FFFF == last {
                start = position
            } else if let i = entries.firstIndex(where: { $0.attr.id & 0xFFFF_FFFF == last }) {
                start = i + 1
            } else {
                start = min(position, entries.count)
            }
        }

        var index = start
        while index < entries.count {
            let entry = entries[index]
            let packed = packer.packEntry(
                name: FSFileName(string: entry.name),
                itemType: entry.attr.is_dir ? .directory : .file,
                itemID: FSItem.Identifier(rawValue: entry.attr.id)!,
                nextCookie: Self.cookieAfter(index, entry.attr.id),
                attributes: attributes == nil ? nil : self.attributes(entry.attr, parent: dir.id))
            if !packed { break }
            index += 1
        }
        replyHandler(FSDirectoryVerifier(rawValue: 1), nil)
    }

    private static func cookieAfter(_ index: Int, _ id: UInt64) -> FSDirectoryCookie {
        cookie(position: index + 1, after: id)
    }

    // MARK: Open and close

    /// FSKit opens an item with the modes wanted and closes it keeping the modes still in
    /// use; the core uploads a written file when the last writer closes it. Measured on
    /// macOS 26: FSKit passes each write on before the close, the program's close(2) waits
    /// for closeItem, and an error given here is what close(2) returns (`cp` reports it).
    /// fsync, F_FULLFSYNC and sync(8) never reach the volume, so closing is what saves.
    func openItem(_ item: FSItem, modes: FSVolume.OpenModes, replyHandler: @escaping ((any Error)?) -> Void) {
        guard let item = item as? Item else { return replyHandler(posix(EINVAL)) }
        if item.isDir { return replyHandler(nil) }
        let write = modes.contains(.write)
        log.debug("open: item \(item.id) modes \(modes.rawValue)")
        if write && readOnly { return replyHandler(posix(EROFS)) }
        let e = withDrive { sp_open($0, item.id, write) }
        replyHandler(e == 0 ? nil : posix(e))
    }

    func closeItem(_ item: FSItem, modes: FSVolume.OpenModes, replyHandler: @escaping ((any Error)?) -> Void) {
        guard let item = item as? Item else { return replyHandler(posix(EINVAL)) }
        if item.isDir { return replyHandler(nil) }
        log.debug("close: item \(item.id) keeping modes \(modes.rawValue)")
        let e = withDrive { sp_close($0, item.id, modes.contains(.read), modes.contains(.write)) }
        if e != 0 && e != ENXIO {
            log.error("closing item \(item.id): saving it failed, errno \(e)")
        }
        replyHandler(e == 0 ? nil : posix(e))
    }

    // MARK: Read and write

    func read(
        from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer,
        replyHandler: @escaping (Int, (any Error)?) -> Void
    ) {
        guard let item = item as? Item else { return replyHandler(0, posix(EINVAL)) }
        if item.isDir { return replyHandler(0, posix(EISDIR)) }
        if offset < 0 { return replyHandler(0, posix(EINVAL)) }
        let want = min(length, buffer.length)
        var total = 0
        var e: Int32 = 0
        buffer.withUnsafeMutableBytes { raw in
            guard let base = raw.baseAddress?.assumingMemoryBound(to: UInt8.self) else { return }
            e = withDrive { d in
                while total < want {
                    let n = sp_read(d, item.id, UInt64(offset) + UInt64(total), base + total, want - total)
                    if n < 0 { return Int32(-n) }
                    if n == 0 { break }
                    total += Int(n)
                }
                return 0
            }
        }
        replyHandler(total, total == 0 && e != 0 ? posix(e) : nil)
    }

    func write(
        contents: Data, to item: FSItem, at offset: off_t,
        replyHandler: @escaping (Int, (any Error)?) -> Void
    ) {
        guard let item = item as? Item else { return replyHandler(0, posix(EINVAL)) }
        if item.isDir { return replyHandler(0, posix(EISDIR)) }
        if readOnly { return replyHandler(0, posix(EROFS)) }
        if offset < 0 { return replyHandler(0, posix(EINVAL)) }
        log.debug("write: item \(item.id) offset \(offset) length \(contents.count)")
        var total = 0
        var e: Int32 = 0
        contents.withUnsafeBytes { raw in
            guard let base = raw.baseAddress?.assumingMemoryBound(to: UInt8.self) else { return }
            e = withDrive { d in
                while total < raw.count {
                    let n = sp_write(d, item.id, UInt64(offset) + UInt64(total), base + total, raw.count - total)
                    if n < 0 { return Int32(-n) }
                    if n == 0 { return EIO }
                    total += Int(n)
                }
                return 0
            }
        }
        replyHandler(total, total == 0 && e != 0 ? posix(e) : nil)
    }

    // MARK: Extended attributes (in memory; see Item)

    func getXattr(named name: FSFileName, of item: FSItem, replyHandler: @escaping (Data?, (any Error)?) -> Void) {
        guard let item = item as? Item, let name = name.string else { return replyHandler(nil, posix(EINVAL)) }
        if let value = item.xattr(name) {
            replyHandler(value, nil)
        } else {
            replyHandler(nil, posix(ENOATTR))
        }
    }

    func setXattr(
        named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy,
        replyHandler: @escaping ((any Error)?) -> Void
    ) {
        guard let item = item as? Item, let name = name.string else { return replyHandler(posix(EINVAL)) }
        if readOnly { return replyHandler(posix(EROFS)) }
        if let value, value.count > maximumXattrSize { return replyHandler(posix(E2BIG)) }
        let e = item.setXattr(name, value, policy: policy)
        replyHandler(e == 0 ? nil : posix(e))
    }

    func listXattrs(of item: FSItem, replyHandler: @escaping ([FSFileName]?, (any Error)?) -> Void) {
        guard let item = item as? Item else { return replyHandler(nil, posix(EINVAL)) }
        replyHandler(item.xattrNames().map { FSFileName(string: $0) }, nil)
    }
}
