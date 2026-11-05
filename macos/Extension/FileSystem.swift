// The SP-404MKII as an FSKit file system: one volume per mount, holding the drive's core
// (ffi/, through sp404.h). Mount it with:
//
//     mount -F -t sp404 sp404://unit <folder>         (add -r for read-only)
//     umount <folder>
//
// Log lines (this extension's and the core's) go to the unified log, subsystem
// com.polecatplugins.sp404drive:
//
//     log show --last 10m --predicate 'subsystem == "com.polecatplugins.sp404drive"'

import ExtensionFoundation
import Foundation
import FSKit
import SP404Core
import os

let log = Logger(subsystem: "com.polecatplugins.sp404drive", category: "fsmodule")
let coreLog = Logger(subsystem: "com.polecatplugins.sp404drive", category: "core")

@main
struct SP404Extension: UnaryFileSystemExtension {
    let fileSystem = SP404FileSystem()
}

/// The core's log lines: error and warn as errors, info as notices (both kept in the
/// log store), debug and trace as debug.
private let coreLogLine: SpLogFn = { level, message in
    guard let message else { return }
    let text = String(cString: message)
    switch level {
    case 1, 2: coreLog.error("\(text, privacy: .public)")
    case 3: coreLog.notice("\(text, privacy: .public)")
    default: coreLog.debug("\(text, privacy: .public)")
    }
}

/// Whether the mount asked for read-only. `mount -r` and `-o rdonly` reach the volume's
/// `activate` as `-o ro` and `-o rdonly`; FSKit documents `--rdonly` for `loadResource`.
func wantsReadOnly(_ options: [String]) -> Bool {
    var it = options.makeIterator()
    while let arg = it.next() {
        switch arg {
        case "--rdonly", "-r":
            return true
        case "-o":
            if let list = it.next(), list.split(separator: ",").contains(where: { $0 == "rdonly" || $0 == "ro" }) {
                return true
            }
        default:
            break
        }
    }
    return false
}

/// Passed to the core as the context of its "unit gone" callback. Retained until the
/// drive is closed, after which the core can't call it any more.
final class UnitWatch: @unchecked Sendable {
    weak var fileSystem: SP404FileSystem?
}

let unitGone: SpClosedFn = { context in
    guard let context else { return }
    let watch = Unmanaged<UnitWatch>.fromOpaque(context).takeUnretainedValue()
    // The core fails every request from now on; the volume stays until it's unmounted.
    log.error("the SP-404MKII went away (unplugged or switched off)")
    // FSKit's container state goes to "not ready" on device termination; say so, in case
    // FSKit unmounts on it. (Not tested on the unit.)
    watch.fileSystem?.containerStatus = .notReady(status: fs_errorForPOSIXError(ENXIO))
}

/// The file system: one volume, holding the drive from loadResource to unmount.
final class SP404FileSystem: FSUnaryFileSystem, FSUnaryFileSystemOperations, @unchecked Sendable {
    private let lock = NSLock()
    private var volume: Volume?

    func probeResource(
        resource: FSResource,
        replyHandler: @escaping (FSProbeResult?, (any Error)?) -> Void
    ) {
        replyHandler(
            .usableButLimited(name: "SP-404MKII", containerID: FSContainerIdentifier(uuid: Volume.uuid)),
            nil)
    }

    func loadResource(
        resource: FSResource,
        options: FSTaskOptions,
        replyHandler: @escaping (FSVolume?, (any Error)?) -> Void
    ) {
        let args = options.taskOptions
        log.notice("loadResource: options \(args, privacy: .public)")
        sp_set_logger(coreLogLine, 3)
        // The drive is opened here, so a missing unit or a busy port fails the load: FSKit
        // cleans up after a failed load, but after a failed activate it keeps the
        // resource loaded and refuses the next mount ("Resource busy").
        let volume = Volume(fileSystem: self, readOnly: wantsReadOnly(args))
        if let error = volume.openDrive(readOnly: volume.readOnly) {
            containerStatus = .notReady(status: error)
            return replyHandler(nil, error)
        }
        lock.withLock { self.volume = volume }
        containerStatus = .ready
        replyHandler(volume, nil)
    }

    func unloadResource(
        resource: FSResource,
        options: FSTaskOptions,
        replyHandler: @escaping ((any Error)?) -> Void
    ) {
        log.notice("unloadResource")
        let volume = lock.withLock {
            defer { self.volume = nil }
            return self.volume
        }
        // Normally closed already, on unmount.
        volume?.closeDrive()
        containerStatus = .notReady(status: fs_errorForPOSIXError(ENXIO))
        replyHandler(nil)
    }
}
