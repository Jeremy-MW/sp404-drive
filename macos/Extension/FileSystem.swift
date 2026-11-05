// The SP-404MKII as an FSKit file system: one volume per mount, holding the drive's core
// (ffi/, through sp404.h). `sp404drive mount` and `unmount` run these:
//
//     mount -F -t sp404 sp404://unit <folder>         (add -r for read-only)
//     umount <folder>
//
// Settings go in the URL's query, since the volume opens the drive in loadResource,
// before the mount's -o options arrive (in activate):
//
//     sp404://unit?port=/dev/cu.usbmodem1101&cache-secs=5&read-only
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

/// What the resource's URL asks for: `port` (default: the first SP-404MKII found),
/// `cache-secs` (how long a folder listing is reused, default 5) and `read-only`
/// (present, or `=1`/`=true`). An unknown key or a bad value fails the mount with EINVAL.
struct MountSettings: Sendable {
    var port: String?
    var cacheSecs: UInt32 = 5
    var readOnly = false

    init(url: URL?) throws {
        guard let url else { return }
        guard let parts = URLComponents(url: url, resolvingAgainstBaseURL: false) else {
            throw MountSettingsError(text: "unreadable URL \(url.absoluteString)")
        }
        for item in parts.queryItems ?? [] {
            let value = item.value
            switch item.name {
            case "port":
                guard let value, value.hasPrefix("/dev/") else {
                    throw MountSettingsError(text: "port must be a /dev/ path, not \(value ?? "nothing")")
                }
                port = value
            case "cache-secs":
                guard let value, let secs = UInt32(value) else {
                    throw MountSettingsError(text: "cache-secs must be a number of seconds, not \(value ?? "nothing")")
                }
                cacheSecs = secs
            case "read-only":
                switch value {
                case nil, "", "1", "true": readOnly = true
                case "0", "false": readOnly = false
                default: throw MountSettingsError(text: "read-only must be 1 or 0, not \(value!)")
                }
            default:
                throw MountSettingsError(text: "unknown setting \(item.name) in \(url.absoluteString)")
            }
        }
    }
}

struct MountSettingsError: Error {
    let text: String
}

/// The POSIX error for the core's reason for not opening the drive, as `mount` will print
/// it: "Operation not supported by device" for no unit (or no such port), "Resource
/// busy" when another program holds the port.
func openErrorCode(_ reason: String) -> Int32 {
    if reason.contains("no SP-404MKII found") {
        return ENODEV
    }
    if reason.hasPrefix("opening") {
        // "opening /dev/...: Resource busy (os error 16) (is Roland's ...)"
        if let range = reason.range(of: "(os error "),
            let code = Int32(reason[range.upperBound...].prefix(while: { $0.isNumber }))
        {
            return code == ENOENT ? ENODEV : code
        }
        return EBUSY
    }
    return EIO
}

/// Calls `body` with `string` as a C string, or with NULL.
func withOptionalCString<R>(_ string: String?, _ body: (UnsafePointer<CChar>?) -> R) -> R {
    guard let string else { return body(nil) }
    return string.withCString { body($0) }
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
        let url = (resource as? FSGenericURLResource)?.url
        log.notice(
            "loadResource: \(url?.absoluteString ?? "no URL", privacy: .public), options \(args, privacy: .public)")
        sp_set_logger(coreLogLine, 3)
        // One drive per extension process. A second load while a volume holds the drive
        // must not touch it (or the container's state). After an unmount the drive is
        // closed, and a new load replaces the volume.
        if lock.withLock({ self.volume?.isOpen ?? false }) {
            log.error("loadResource: the SP-404MKII is already loaded")
            return replyHandler(nil, fs_errorForPOSIXError(EBUSY))
        }
        let settings: MountSettings
        do {
            settings = try MountSettings(url: url)
        } catch {
            let text = (error as? MountSettingsError)?.text ?? "\(error)"
            log.error("loadResource: \(text, privacy: .public)")
            return replyHandler(nil, fs_errorForPOSIXError(EINVAL))
        }
        log.notice(
            "settings: port \(settings.port ?? "first found", privacy: .public), cache \(settings.cacheSecs) s, read-only \(settings.readOnly)"
        )
        // The drive is opened here, so a missing unit or a busy port fails the load: FSKit
        // cleans up after a failed load, but after a failed activate it keeps the
        // resource loaded and refuses the next mount ("Resource busy").
        let volume = Volume(
            fileSystem: self, settings: settings, readOnly: settings.readOnly || wantsReadOnly(args))
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
