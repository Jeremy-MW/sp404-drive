// The app carries the FSKit extension and the sp404drive command; launching it once
// registers the extension with macOS. No user interface yet.
import Foundation

let cli = Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS/sp404drive").path
print("""
    SP-404 Drive: switch on SP-404MKII in System Settings > General > Login Items & \
    Extensions > Extensions: By Category > File System Extensions.
    Then mount the unit with: "\(cli)" mount
    To type just `sp404drive`: sudo ln -sf "\(cli)" /usr/local/bin/sp404drive
    """)
