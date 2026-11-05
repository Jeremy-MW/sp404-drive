import Foundation
import FSKit

/// A file or folder FSKit holds: the core's item number, plus what only the extension
/// keeps (its parent's number, and its extended attributes, in memory).
///
/// The volume keeps one `Item` per number while FSKit holds it, until `reclaimItem`.
final class Item: FSItem, @unchecked Sendable {
    let id: UInt64
    let isDir: Bool
    private let lock = NSLock()
    private var _parent: UInt64
    private var _xattrs: [String: Data] = [:]

    init(id: UInt64, isDir: Bool, parent: UInt64) {
        self.id = id
        self.isDir = isDir
        self._parent = parent
    }

    var fileID: FSItem.Identifier { FSItem.Identifier(rawValue: id)! }

    /// The folder it was last found in (`parentOfRoot` for the root).
    var parent: UInt64 {
        get { lock.withLock { _parent } }
        set { lock.withLock { _parent = newValue } }
    }

    // Extended attributes, kept in memory only: macOS (Finder, copyfile, quarantine) sets
    // them on almost everything it copies, and a volume without them gets `._` files
    // instead, which the unit doesn't want. They last as long as the item stays in
    // FSKit's cache, at most until unmount; nothing reaches the unit.

    func xattr(_ name: String) -> Data? {
        lock.withLock { _xattrs[name] }
    }

    func xattrNames() -> [String] {
        lock.withLock { Array(_xattrs.keys) }
    }

    /// Apply `policy`; returns 0 or an errno value.
    func setXattr(_ name: String, _ value: Data?, policy: FSVolume.SetXattrPolicy) -> Int32 {
        lock.withLock {
            let exists = _xattrs[name] != nil
            switch policy {
            case .mustCreate where exists:
                return EEXIST
            case .mustReplace where !exists, .delete where !exists:
                return ENOATTR
            case .delete:
                _xattrs[name] = nil
                return 0
            default:
                _xattrs[name] = value ?? Data()
                return 0
            }
        }
    }
}
