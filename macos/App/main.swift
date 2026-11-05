// SP-404 Drive: a menu-bar app. It carries the FSKit extension (macOS registers it when
// the app is first opened) and the sp404drive command, and mounts and ejects the unit:
// automatically when it's switched on or off, or from its menu.
import AppKit

MainActor.assumeIsolated {
    let app = NSApplication.shared
    let delegate = AppDelegate()
    app.delegate = delegate
    app.setActivationPolicy(.accessory)
    withExtendedLifetime(delegate) { app.run() }
}
