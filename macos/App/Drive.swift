// Mounting and unmounting, through the sp404drive command the app carries (so the app
// gets its checks and messages), and the drive's mounts, from the kernel's mount table.
import Foundation

/// A mounted SP-404MKII volume.
struct Mounted: Equatable {
    /// Where it's mounted.
    var path: String
    /// The resource URL (`sp404://unit?port=...`), as `mount` shows the source.
    var source: String

    var readOnly: Bool
    /// The serial port in the URL, if any.
    var port: String? {
        URLComponents(string: source)?.queryItems?.first { $0.name == "port" }?.value
    }
}

enum Drive {
    static let command = Bundle.main.bundleURL
        .appendingPathComponent("Contents/MacOS/sp404drive").path

    /// Every mounted SP-404MKII volume (file system type sp404).
    static func mounts() -> [Mounted] {
        var table: UnsafeMutablePointer<statfs>?
        let count = getmntinfo(&table, MNT_NOWAIT)
        guard count > 0, let table else { return [] }
        func text<T>(_ field: T) -> String {
            withUnsafeBytes(of: field) { String(decoding: $0.prefix { $0 != 0 }, as: UTF8.self) }
        }
        return UnsafeBufferPointer(start: table, count: Int(count))
            .filter { text($0.f_fstypename) == "sp404" }
            .map {
                Mounted(
                    path: text($0.f_mntonname), source: text($0.f_mntfromname),
                    readOnly: $0.f_flags & UInt32(MNT_RDONLY) != 0)
            }
    }

    struct Failure: Error {
        /// What the command said, without its "Error: " prefix.
        var message: String
    }

    static func mount(port: String?, readOnly: Bool) async throws(Failure) -> String {
        var args = ["mount"]
        if let port { args += ["--port", port] }
        if readOnly { args.append("--read-only") }
        return try await run(command, args)
    }

    static func unmount(_ path: String) async throws(Failure) -> String {
        try await run(command, ["unmount", path])
    }

    /// `umount -f`: for a volume whose unit is gone, where nothing can be saved anyway.
    static func forceUnmount(_ path: String) async throws(Failure) -> String {
        try await run("/sbin/umount", ["-f", path])
    }

    /// Runs a command off the main thread; its output, or its error output as the failure.
    private static func run(_ tool: String, _ args: [String]) async throws(Failure) -> String {
        let result: Result<String, Failure> = await withCheckedContinuation { done in
            let process = Process()
            process.executableURL = URL(fileURLWithPath: tool)
            process.arguments = args
            let out = Pipe()
            let err = Pipe()
            process.standardOutput = out
            process.standardError = err
            process.standardInput = FileHandle.nullDevice
            process.terminationHandler = { p in
                let output = String(decoding: out.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
                let error = String(decoding: err.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
                if p.terminationStatus == 0 {
                    done.resume(returning: .success(output))
                } else {
                    var message = error.trimmingCharacters(in: .whitespacesAndNewlines)
                    if message.hasPrefix("Error: ") { message.removeFirst("Error: ".count) }
                    if message.isEmpty { message = "\(tool) failed (exit status \(p.terminationStatus))" }
                    done.resume(returning: .failure(Failure(message: message)))
                }
            }
            do {
                try process.run()
            } catch {
                done.resume(returning: .failure(Failure(message: "can't run \(tool): \(error.localizedDescription)")))
            }
        }
        return try result.get()
    }
}
