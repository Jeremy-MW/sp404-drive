// The menu-bar item and what it does.
//
// - Switched on or plugged in: mounts the unit, if "Mount Automatically" is on.
// - Switched off or unplugged: unmounts its volume. FSKit leaves the volume mounted when
//   the device goes, and the extension's sandbox refuses unmount(2), so the app does it.
// - The menu: Mount, Mount Read-Only, Eject, Show in Finder, and the settings.
// - Quit ejects first: with the app gone, switching the unit off would leave the volume.
import AppKit
import ServiceManagement

private let autoMountKey = "MountAutomatically"
private let welcomedKey = "Welcomed"
private func describe(_ error: any Error) -> String {
    (error as? Drive.Failure)?.message ?? error.localizedDescription
}

private let extensionSettings = URL(
    string: "x-apple.systempreferences:com.apple.LoginItems-Settings.extension")!

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate, NSMenuDelegate {
    private let watcher = UnitWatcher()
    private var statusItem: NSStatusItem!
    private var mounts: [Mounted] = []
    /// What the app is doing now ("Mounting…"), shown in the menu; nil when idle.
    private var busy: String?
    /// The last automatic mount's failure, shown in the menu until the next change.
    private var lastProblem: String?
    /// A unit that was switched on while the app was busy, or while its last volume was
    /// still being unmounted: mounted when that's done.
    private var waitingPort: String?
    /// Retry the mount at launch once if it fails: just after the app's installed or
    /// updated, macOS takes a few seconds to register the file system extension it carries,
    /// and a mount then can fail, or stall until the extension gives up (after 10 s).
    private var retryLaunchMount = false

    private var autoMount: Bool {
        get { UserDefaults.standard.object(forKey: autoMountKey) as? Bool ?? true }
        set { UserDefaults.standard.set(newValue, forKey: autoMountKey) }
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
        statusItem.button?.toolTip = "SP-404 Drive"
        let menu = NSMenu()
        menu.delegate = self
        menu.autoenablesItems = false
        statusItem.menu = menu

        let center = NSWorkspace.shared.notificationCenter
        for name in [NSWorkspace.didMountNotification, NSWorkspace.didUnmountNotification] {
            center.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated { self?.refresh() }
            }
        }
        watcher.onAppear = { [weak self] port in self?.unitAppeared(port) }
        watcher.onDisappear = { [weak self] port in self?.unitDisappeared(port) }
        watcher.start()
        refresh()

        if !UserDefaults.standard.bool(forKey: welcomedKey) {
            UserDefaults.standard.set(true, forKey: welcomedKey)
            welcome()
        }
        if autoMount, let port = watcher.ports.first {
            // Opened (or started at login) with the unit already on.
            waitingPort = port
            retryLaunchMount = true
            mountIfWaiting()
        }
    }

    /// Opened again from Finder or Spotlight while running: show the menu.
    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows: Bool) -> Bool {
        statusItem.button?.performClick(nil)
        return false
    }

    // MARK: The unit

    private func unitAppeared(_ port: String) {
        lastProblem = nil
        refresh()
        guard autoMount else { return }
        waitingPort = port
        mountIfWaiting()
    }

    private func unitDisappeared(_ port: String) {
        if waitingPort == port { waitingPort = nil }
        refresh()
        // Its volume, or one mounted without a port (by hand, on the first unit found).
        for m in mounts where m.port == port || m.port == nil {
            Task {
                do {
                    _ = try await Drive.unmount(m.path)
                } catch {
                    // In use (a Finder window, a terminal): nothing can be saved to a unit
                    // that's gone, so force it.
                    _ = try? await Drive.forceUnmount(m.path)
                }
                self.refresh()
                // Switched off and straight back on: mount it again now the old volume's gone.
                self.mountIfWaiting()
            }
        }
    }

    /// Mounts the unit that was switched on, unless the app is busy or a volume for it is
    /// still mounted (then this runs again when that's done).
    private func mountIfWaiting() {
        guard let port = waitingPort, autoMount, busy == nil, watcher.ports.contains(port),
            !mounts.contains(where: { $0.port == port || $0.port == nil })
        else { return }
        mount(port: port, readOnly: false, automatic: true)
    }

    // MARK: Actions

    private func mount(port: String?, readOnly: Bool, automatic: Bool) {
        guard busy == nil else { return }
        waitingPort = nil
        busy = "Mounting…"
        refresh()
        Task {
            defer {
                self.busy = nil
                self.refresh()
                self.mountIfWaiting()
            }
            do {
                _ = try await Drive.mount(port: port, readOnly: readOnly)
                self.lastProblem = nil
                self.refresh()
                if !automatic { self.showInFinder() }
            } catch {
                if automatic {
                    // No alert for something nobody clicked; the menu says what happened.
                    self.lastProblem = describe(error)
                    if self.retryLaunchMount, let port {
                        self.retryLaunchMount = false
                        Task {
                            try? await Task.sleep(for: .seconds(15))
                            self.waitingPort = port
                            self.mountIfWaiting()
                        }
                    }
                } else {
                    self.alert("Couldn't mount the SP-404MKII", describe(error))
                }
            }
        }
    }

    @objc private func mountReadWrite() { mount(port: watcher.ports.first, readOnly: false, automatic: false) }
    @objc private func mountReadOnly() { mount(port: watcher.ports.first, readOnly: true, automatic: false) }

    @objc private func eject() {
        guard busy == nil else { return }
        let paths = mounts.map(\.path)
        busy = "Ejecting…"
        refresh()
        Task {
            defer {
                self.busy = nil
                self.refresh()
                self.mountIfWaiting()
            }
            for path in paths {
                do {
                    _ = try await Drive.unmount(path)
                } catch {
                    self.alert("Couldn't eject the SP-404MKII", describe(error))
                }
            }
        }
    }

    @objc private func showInFinder() {
        guard let m = mounts.first else { return }
        NSWorkspace.shared.open(URL(fileURLWithPath: m.path, isDirectory: true))
    }

    @objc private func toggleAutoMount() {
        autoMount.toggle()
        refresh()
    }

    @objc private func toggleOpenAtLogin() {
        let service = SMAppService.mainApp
        do {
            if service.status == .enabled {
                try service.unregister()
            } else {
                try service.register()
            }
        } catch {
            alert("Couldn't change Open at Login", error.localizedDescription)
        }
        refresh()
    }

    @objc private func installCommand() {
        let link = "/usr/local/bin/sp404drive"
        let script = """
            do shell script "mkdir -p /usr/local/bin && ln -sf '\(Drive.command)' '\(link)'" \
            with administrator privileges
            """
        var error: NSDictionary?
        NSAppleScript(source: script)?.executeAndReturnError(&error)
        if let error {
            // -128: cancelled in the password dialog.
            if (error[NSAppleScript.errorNumber] as? Int) != -128 {
                alert("Couldn't install the command", "\(error[NSAppleScript.errorMessage] ?? error)")
            }
            return
        }
        alert(
            "Command installed",
            "In Terminal, type sp404drive (for example sp404drive mount). It's a link to the copy inside this app, so it's updated with the app.",
            style: .informational)
    }

    @objc private func openExtensionSettings() { NSWorkspace.shared.open(extensionSettings) }

    /// The standard About panel, cut down to the icon, the name with the version, and the
    /// GPL's legal notices: the copyright, no warranty, and where the licence and source are.
    @objc private func showAbout() {
        let version = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? ""
        let centred = NSMutableParagraphStyle()
        centred.alignment = .center
        let plain: [NSAttributedString.Key: Any] = [
            .font: NSFont.systemFont(ofSize: NSFont.systemFontSize),
            .foregroundColor: NSColor.labelColor,
            .paragraphStyle: centred,
        ]
        func link(_ text: String, _ url: String) -> NSAttributedString {
            NSAttributedString(
                string: text,
                attributes: [
                    .font: NSFont.systemFont(ofSize: NSFont.systemFontSize),
                    .link: URL(string: url)!,
                    .paragraphStyle: centred,
                ])
        }
        let credits = NSMutableAttributedString(string: "Copyright © 2026 ", attributes: plain)
        credits.append(link("polecatplugins.com", "https://polecatplugins.com"))
        credits.append(
            NSAttributedString(
                string: "\n\nFree software under version 3 of the GNU General Public License, with no warranty. ",
                attributes: plain))
        credits.append(link("Licence and source code", "https://github.com/Jeremy-MW/sp404-drive"))
        NSApp.activate()
        NSApp.orderFrontStandardAboutPanel(options: [
            .applicationName: "SP-404MkII Drive \(version)",
            // Empty, so the panel leaves out its own version line.
            .applicationVersion: "",
            .version: "",
            .credits: credits,
        ])
    }

    /// Ejects, then quits. Logging out or shutting down doesn't come here: macOS
    /// unmounts the volume itself then.
    @objc private func quit() {
        guard busy == nil else { return }
        let paths = mounts.map(\.path)
        guard !paths.isEmpty else { return NSApp.terminate(nil) }
        busy = "Ejecting…"
        refresh()
        Task {
            var problem: String?
            for path in paths {
                do {
                    _ = try await Drive.unmount(path)
                } catch {
                    problem = describe(error)
                }
            }
            self.busy = nil
            self.refresh()
            guard let problem else { return NSApp.terminate(nil) }
            NSApp.activate()
            let alert = NSAlert()
            alert.messageText = "Couldn't eject the SP-404MKII"
            alert.informativeText = """
                \(problem)

                If you quit anyway, the drive stays mounted, and switching the unit off won't \
                eject it.
                """
            alert.addButton(withTitle: "Don't Quit")
            alert.addButton(withTitle: "Quit Anyway")
            if alert.runModal() == .alertSecondButtonReturn { NSApp.terminate(nil) }
        }
    }

    // MARK: Menu

    private func refresh() {
        mounts = Drive.mounts()
        let button = statusItem.button
        button?.image = Self.logo(filled: !mounts.isEmpty)
        button?.appearsDisabled = mounts.isEmpty && watcher.ports.isEmpty
        if let menu = statusItem.menu { menuNeedsUpdate(menu) }
    }

    private var state: String {
        if let busy { return busy }
        if let m = mounts.first { return m.readOnly ? "SP-404MKII: mounted, read-only" : "SP-404MKII: mounted" }
        if watcher.ports.isEmpty { return "SP-404MKII: not connected" }
        return "SP-404MKII: connected, not mounted"
    }

    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        let connected = !watcher.ports.isEmpty
        let mounted = !mounts.isEmpty
        let idle = busy == nil

        menu.addItem(info(state))
        if let lastProblem, !mounted, idle {
            menu.addItem(info("Couldn't mount: \(lastProblem)"))
            if lastProblem.contains("System Settings") {
                menu.addItem(item("Open System Settings…", #selector(openExtensionSettings)))
            }
        }
        menu.addItem(.separator())
        menu.addItem(item("Mount", #selector(mountReadWrite), enabled: connected && !mounted && idle))
        menu.addItem(item("Mount Read-Only", #selector(mountReadOnly), enabled: connected && !mounted && idle))
        menu.addItem(item("Eject", #selector(eject), enabled: mounted && idle))
        menu.addItem(item("Show in Finder", #selector(showInFinder), enabled: mounted))
        menu.addItem(.separator())
        let auto = item("Mount Automatically When Switched On", #selector(toggleAutoMount))
        auto.state = autoMount ? .on : .off
        menu.addItem(auto)
        let login = item("Open at Login", #selector(toggleOpenAtLogin))
        login.state = SMAppService.mainApp.status == .enabled ? .on : .off
        menu.addItem(login)
        menu.addItem(item("Install Command-Line Tool…", #selector(installCommand)))
        menu.addItem(item("File System Extension Settings…", #selector(openExtensionSettings)))
        menu.addItem(.separator())
        menu.addItem(item("About SP-404MkII Drive", #selector(showAbout)))
        menu.addItem(item(mounted ? "Eject and Quit" : "Quit SP-404 Drive", #selector(quit), enabled: idle))
    }

    /// The logo for the menu bar: a ring around a downward triangle, or (mounted) a solid
    /// disc with the triangle cut out. A template image, so macOS colours it to suit the
    /// menu bar (dark on a light bar, light on a dark one) and dims it when disabled.
    private static func logo(filled: Bool) -> NSImage {
        let image = NSImage(size: NSSize(width: 18, height: 18), flipped: true) { rect in
            let c = NSPoint(x: rect.midX, y: rect.midY)
            let outer: CGFloat = 7.5
            let inner = outer * 0.79  // the logo's proportions
            func circle(_ r: CGFloat) -> NSBezierPath {
                NSBezierPath(ovalIn: NSRect(x: c.x - r, y: c.y - r, width: 2 * r, height: 2 * r))
            }
            let triangle = NSBezierPath()
            triangle.move(to: NSPoint(x: c.x - inner, y: c.y))
            triangle.line(to: NSPoint(x: c.x + inner, y: c.y))
            triangle.line(to: NSPoint(x: c.x, y: c.y + inner))
            triangle.close()
            let path = circle(outer)
            path.append(filled ? triangle : circle(inner))
            path.windingRule = .evenOdd
            NSColor.black.setFill()
            path.fill()
            if !filled { triangle.fill() }
            return true
        }
        image.isTemplate = true
        image.accessibilityDescription = filled ? "SP-404 Drive: mounted" : "SP-404 Drive"
        return image
    }

    private func item(_ title: String, _ action: Selector, enabled: Bool = true) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: action, keyEquivalent: "")
        item.target = self
        item.isEnabled = enabled
        return item
    }

    private func info(_ title: String) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: nil, keyEquivalent: "")
        item.isEnabled = false
        return item
    }

    // MARK: Dialogs

    private func welcome() {
        NSApp.activate()
        let alert = NSAlert()
        alert.messageText = "SP-404 Drive is in the menu bar"
        alert.informativeText = """
            One step before the first use: in System Settings, go to General > Login Items & \
            Extensions > Extensions, choose By Category, open File System Extensions and \
            switch on SP-404MKII.

            Then switch the SP-404MKII on, or plug it in: it appears in Finder's sidebar. \
            Quit Roland's SP-404MKII app first; only one program can use the unit at a time.
            """
        alert.addButton(withTitle: "Open System Settings")
        alert.addButton(withTitle: "Later")
        // The drive is only ejected when the unit's switched off while the app is running.
        alert.showsSuppressionButton = true
        alert.suppressionButton?.title = "Open SP-404 Drive at login"
        alert.suppressionButton?.state = .on
        let response = alert.runModal()
        if alert.suppressionButton?.state == .on, SMAppService.mainApp.status != .enabled {
            do {
                try SMAppService.mainApp.register()
            } catch {
                self.alert("Couldn't set SP-404 Drive to open at login", error.localizedDescription)
            }
        }
        if response == .alertFirstButtonReturn {
            NSWorkspace.shared.open(extensionSettings)
        }
    }

    private func alert(_ title: String, _ text: String, style: NSAlert.Style = .warning) {
        NSApp.activate()
        let alert = NSAlert()
        alert.alertStyle = style
        alert.messageText = title
        alert.informativeText = text
        if text.contains("System Settings") {
            alert.addButton(withTitle: "Open System Settings")
            alert.addButton(withTitle: "OK")
            if alert.runModal() == .alertFirstButtonReturn {
                NSWorkspace.shared.open(extensionSettings)
            }
        } else {
            alert.runModal()
        }
    }
}
