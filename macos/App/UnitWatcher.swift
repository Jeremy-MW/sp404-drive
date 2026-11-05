// Tells the app when an SP-404MKII's serial port appears or disappears, through IOKit
// notifications on the main run loop: no polling. The serial node (IOSerialBSDClient) is
// watched rather than the USB device, so the /dev/cu.* path exists when the app hears
// about it.
import Foundation
import IOKit
import IOKit.serial

/// Roland's USB vendor id and the SP-404MKII's product id (as the core's find_ports).
private let rolandVendor = 0x0582
private let sp404Product = 0x02E7

@MainActor
final class UnitWatcher {
    /// Called with the callout path (`/dev/cu.usbmodem1101`) when a unit appears.
    var onAppear: (String) -> Void = { _ in }
    /// Called with the callout path when a unit disappears (unplugged or switched off).
    var onDisappear: (String) -> Void = { _ in }

    /// The callout paths of the units connected now.
    private(set) var ports: [String] = []

    private var notifyPort: IONotificationPortRef?
    private var appearIterator: io_iterator_t = 0
    private var goneIterator: io_iterator_t = 0
    /// Registry entry ids of the serial nodes we know, with their paths: a terminated
    /// node's properties may already be gone, so its path is looked up here.
    private var known: [UInt64: String] = [:]

    func start() {
        guard notifyPort == nil, let port = IONotificationPortCreate(kIOMainPortDefault) else { return }
        notifyPort = port
        CFRunLoopAddSource(
            CFRunLoopGetMain(), IONotificationPortGetRunLoopSource(port).takeUnretainedValue(),
            .defaultMode)
        let me = Unmanaged.passUnretained(self).toOpaque()
        let appeared: IOServiceMatchingCallback = { refcon, iterator in
            guard let refcon else { return }
            let watcher = Unmanaged<UnitWatcher>.fromOpaque(refcon).takeUnretainedValue()
            MainActor.assumeIsolated { watcher.drain(iterator, appeared: true) }
        }
        let terminated: IOServiceMatchingCallback = { refcon, iterator in
            guard let refcon else { return }
            let watcher = Unmanaged<UnitWatcher>.fromOpaque(refcon).takeUnretainedValue()
            MainActor.assumeIsolated { watcher.drain(iterator, appeared: false) }
        }
        IOServiceAddMatchingNotification(
            port, kIOFirstMatchNotification, IOServiceMatching(kIOSerialBSDServiceValue),
            appeared, me, &appearIterator)
        IOServiceAddMatchingNotification(
            port, kIOTerminatedNotification, IOServiceMatching(kIOSerialBSDServiceValue),
            terminated, me, &goneIterator)
        // Arm both notifications; the units already connected count as appearing, but
        // without the callback (nothing was "switched on" just now).
        drain(appearIterator, appeared: true, initial: true)
        drain(goneIterator, appeared: false, initial: true)
    }

    private func drain(_ iterator: io_iterator_t, appeared: Bool, initial: Bool = false) {
        while case let service = IOIteratorNext(iterator), service != 0 {
            defer { IOObjectRelease(service) }
            var id: UInt64 = 0
            IORegistryEntryGetRegistryEntryID(service, &id)
            if appeared {
                guard isSP404(service), let path = calloutPath(service) else { continue }
                known[id] = path
                if !ports.contains(path) { ports.append(path) }
                if !initial { onAppear(path) }
            } else if let path = known.removeValue(forKey: id) {
                ports.removeAll { $0 == path }
                if !initial { onDisappear(path) }
            }
        }
    }

    private func calloutPath(_ service: io_object_t) -> String? {
        IORegistryEntryCreateCFProperty(service, kIOCalloutDeviceKey as CFString, kCFAllocatorDefault, 0)?
            .takeRetainedValue() as? String
    }

    /// Whether the serial node belongs to an SP-404MKII: its USB device (a parent) has
    /// Roland's vendor id and the SP-404MKII's product id.
    private func isSP404(_ service: io_object_t) -> Bool {
        func number(_ key: String) -> Int? {
            IORegistryEntrySearchCFProperty(
                service, kIOServicePlane, key as CFString, kCFAllocatorDefault,
                IOOptionBits(kIORegistryIterateRecursively | kIORegistryIterateParents)) as? Int
        }
        return number("idVendor") == rolandVendor && number("idProduct") == sp404Product
    }
}
