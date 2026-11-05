// FSKit check: a read-only volume with one file, status.txt, reporting whether this
// sandboxed extension could open the SP-404MKII's serial port. Mount it with:
//
//     mount -F -t sp404 sp404://unit ~/SP404-test
//
// It opens and closes the port only; nothing is sent to the unit.

import ExtensionFoundation
import Foundation
import FSKit
import os

let log = Logger(subsystem: "com.polecatplugins.sp404drive", category: "fsmodule")

@main
struct SP404Extension: UnaryFileSystemExtension {
    let fileSystem = SP404FileSystem()
}

/// Try to open the unit's serial port the way the drive will: read/write, no
/// controlling terminal, exclusive.
func serialPortReport() -> String {
    let dev = (try? FileManager.default.contentsOfDirectory(atPath: "/dev")) ?? []
    let ports = dev.filter { $0.hasPrefix("cu.usbmodem") }.sorted()
    guard let port = ports.first else {
        return "serial port: none found in /dev (listing /dev gave \(dev.count) entries)\n"
    }
    let path = "/dev/" + port
    let fd = open(path, O_RDWR | O_NOCTTY | O_NONBLOCK)
    if fd < 0 {
        let e = errno
        return "serial port: could not open \(path): \(String(cString: strerror(e))) (errno \(e))\n"
    }
    var excl = "TIOCEXCL ok"
    if ioctl(fd, TIOCEXCL) != 0 {
        excl = "TIOCEXCL failed: \(String(cString: strerror(errno)))"
    }
    close(fd)
    return "serial port: opened \(path) (\(excl)), closed again\n"
}

final class SP404FileSystem: FSUnaryFileSystem, FSUnaryFileSystemOperations, @unchecked Sendable {
    func probeResource(
        resource: FSResource,
        replyHandler: @escaping (FSProbeResult?, (any Error)?) -> Void
    ) {
        log.info("probeResource")
        replyHandler(
            .usableButLimited(name: "SP-404MKII", containerID: FSContainerIdentifier(uuid: Volume.uuid)),
            nil)
    }

    func loadResource(
        resource: FSResource,
        options: FSTaskOptions,
        replyHandler: @escaping (FSVolume?, (any Error)?) -> Void
    ) {
        let report = serialPortReport()
        log.info("loadResource: \(report, privacy: .public)")
        containerStatus = .ready
        replyHandler(Volume(report: report), nil)
    }

    func unloadResource(
        resource: FSResource,
        options: FSTaskOptions,
        replyHandler: @escaping ((any Error)?) -> Void
    ) {
        log.info("unloadResource")
        replyHandler(nil)
    }
}

final class Node: FSItem, @unchecked Sendable {
    let id: FSItem.Identifier
    let name: FSFileName
    let isDir: Bool
    let data: Data

    init(id: UInt64, name: String, isDir: Bool, data: Data = Data()) {
        self.id = FSItem.Identifier(rawValue: id)!
        self.name = FSFileName(string: name)
        self.isDir = isDir
        self.data = data
    }

    func attributes(time: timespec) -> FSItem.Attributes {
        let a = FSItem.Attributes()
        a.type = isDir ? .directory : .file
        a.mode = isDir ? UInt32(S_IFDIR | 0o555) : UInt32(S_IFREG | 0o444)
        a.linkCount = isDir ? 2 : 1
        a.uid = getuid()
        a.gid = getgid()
        a.size = UInt64(data.count)
        a.allocSize = UInt64(data.count)
        a.fileID = id
        a.parentID = isDir ? .parentOfRoot : .rootDirectory
        a.modifyTime = time
        a.changeTime = time
        a.accessTime = time
        a.birthTime = time
        return a
    }
}

final class Volume: FSVolume, FSVolume.Operations, FSVolume.ReadWriteOperations,
    FSVolume.OpenCloseOperations, @unchecked Sendable
{
    static let uuid = UUID(uuidString: "5B4F0404-2021-4E55-8D2B-53503430344D")!

    let root: Node
    let status: Node
    let time: timespec

    init(report: String) {
        root = Node(id: FSItem.Identifier.rootDirectory.rawValue, name: "", isDir: true)
        status = Node(id: 3, name: "status.txt", isDir: false, data: Data(report.utf8))
        var now = timespec()
        clock_gettime(CLOCK_REALTIME, &now)
        time = now
        super.init(
            volumeID: FSVolume.Identifier(uuid: Volume.uuid),
            volumeName: FSFileName(string: "SP-404MKII"))
    }

    // MARK: Path configuration

    var maximumLinkCount: Int { 1 }
    var maximumNameLength: Int { 255 }
    var restrictsOwnershipChanges: Bool { true }
    var truncatesLongNames: Bool { false }

    var supportedVolumeCapabilities: FSVolume.SupportedCapabilities {
        let c = FSVolume.SupportedCapabilities()
        c.supportsHiddenFiles = true
        c.supports64BitObjectIDs = true
        c.caseFormat = .insensitiveCasePreserving
        return c
    }

    var volumeStatistics: FSStatFSResult {
        let s = FSStatFSResult(fileSystemTypeName: "sp404")
        s.blockSize = 4096
        s.ioSize = 65536
        s.totalBlocks = 1
        s.usedBlocks = 1
        s.totalBytes = 4096
        s.usedBytes = 4096
        return s
    }

    // MARK: Lifecycle

    func activate(options: FSTaskOptions, replyHandler: @escaping (FSItem?, (any Error)?) -> Void) {
        log.info("activate")
        replyHandler(root, nil)
    }

    func deactivate(options: FSDeactivateOptions = [], replyHandler: @escaping ((any Error)?) -> Void) {
        log.info("deactivate")
        replyHandler(nil)
    }

    func mount(options: FSTaskOptions, replyHandler: @escaping ((any Error)?) -> Void) {
        log.info("mount")
        replyHandler(nil)
    }

    func unmount(replyHandler: @escaping () -> Void) {
        log.info("unmount")
        replyHandler()
    }

    func synchronize(flags: FSSyncFlags, replyHandler: @escaping ((any Error)?) -> Void) {
        replyHandler(nil)
    }

    // MARK: Items

    func getAttributes(
        _ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem,
        replyHandler: @escaping (FSItem.Attributes?, (any Error)?) -> Void
    ) {
        guard let node = item as? Node else {
            return replyHandler(nil, fs_errorForPOSIXError(EIO))
        }
        replyHandler(node.attributes(time: time), nil)
    }

    func setAttributes(
        _ newAttributes: FSItem.SetAttributesRequest, on item: FSItem,
        replyHandler: @escaping (FSItem.Attributes?, (any Error)?) -> Void
    ) {
        replyHandler(nil, fs_errorForPOSIXError(EROFS))
    }

    func lookupItem(
        named name: FSFileName, inDirectory directory: FSItem,
        replyHandler: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void
    ) {
        if directory === root, name.string?.lowercased() == "status.txt" {
            return replyHandler(status, status.name, nil)
        }
        replyHandler(nil, nil, fs_errorForPOSIXError(ENOENT))
    }

    func reclaimItem(_ item: FSItem, replyHandler: @escaping ((any Error)?) -> Void) {
        replyHandler(nil)
    }

    func readSymbolicLink(_ item: FSItem, replyHandler: @escaping (FSFileName?, (any Error)?) -> Void) {
        replyHandler(nil, fs_errorForPOSIXError(EINVAL))
    }

    func createItem(
        named name: FSFileName, type: FSItem.ItemType, inDirectory directory: FSItem,
        attributes newAttributes: FSItem.SetAttributesRequest,
        replyHandler: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void
    ) {
        replyHandler(nil, nil, fs_errorForPOSIXError(EROFS))
    }

    func createSymbolicLink(
        named name: FSFileName, inDirectory directory: FSItem,
        attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName,
        replyHandler: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void
    ) {
        replyHandler(nil, nil, fs_errorForPOSIXError(EROFS))
    }

    func createLink(
        to item: FSItem, named name: FSFileName, inDirectory directory: FSItem,
        replyHandler: @escaping (FSFileName?, (any Error)?) -> Void
    ) {
        replyHandler(nil, fs_errorForPOSIXError(EROFS))
    }

    func removeItem(
        _ item: FSItem, named name: FSFileName, fromDirectory directory: FSItem,
        replyHandler: @escaping ((any Error)?) -> Void
    ) {
        replyHandler(fs_errorForPOSIXError(EROFS))
    }

    func renameItem(
        _ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName,
        to destinationName: FSFileName, inDirectory destinationDirectory: FSItem,
        overItem: FSItem?, replyHandler: @escaping (FSFileName?, (any Error)?) -> Void
    ) {
        replyHandler(nil, fs_errorForPOSIXError(EROFS))
    }

    func enumerateDirectory(
        _ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier,
        attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker,
        replyHandler: @escaping (FSDirectoryVerifier, (any Error)?) -> Void
    ) {
        var entries: [(FSFileName, Node)] = []
        if attributes == nil {
            entries.append((FSFileName(string: "."), root))
            entries.append((FSFileName(string: ".."), root))
        }
        entries.append((status.name, status))
        var index = Int(cookie.rawValue)
        while index < entries.count {
            let (name, node) = entries[index]
            index += 1
            let packed = packer.packEntry(
                name: name, itemType: node.isDir ? .directory : .file, itemID: node.id,
                nextCookie: FSDirectoryCookie(rawValue: UInt64(index)),
                attributes: attributes == nil ? nil : node.attributes(time: time))
            if !packed { break }
        }
        replyHandler(FSDirectoryVerifier(rawValue: 1), nil)
    }

    // MARK: Open, close, read, write

    func openItem(_ item: FSItem, modes: FSVolume.OpenModes, replyHandler: @escaping ((any Error)?) -> Void) {
        replyHandler(modes.contains(.write) ? fs_errorForPOSIXError(EROFS) : nil)
    }

    func closeItem(_ item: FSItem, modes: FSVolume.OpenModes, replyHandler: @escaping ((any Error)?) -> Void) {
        replyHandler(nil)
    }

    func read(
        from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer,
        replyHandler: @escaping (Int, (any Error)?) -> Void
    ) {
        guard let node = item as? Node, !node.isDir else {
            return replyHandler(0, fs_errorForPOSIXError(EISDIR))
        }
        let start = min(Int(offset), node.data.count)
        let count = min(length, node.data.count - start, buffer.length)
        buffer.withUnsafeMutableBytes { dst in
            node.data.copyBytes(to: dst.bindMemory(to: UInt8.self), from: start..<(start + count))
        }
        replyHandler(count, nil)
    }

    func write(
        contents: Data, to item: FSItem, at offset: off_t,
        replyHandler: @escaping (Int, (any Error)?) -> Void
    ) {
        replyHandler(0, fs_errorForPOSIXError(EROFS))
    }
}
