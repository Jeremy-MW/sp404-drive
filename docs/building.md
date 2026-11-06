# Building and contributing

## Windows

You need:

- Rust (stable, MSVC): `winget install Rustlang.Rustup`.
- libclang, which `winfsp-sys` needs to generate its bindings. Any LLVM install works
  (`winget install LLVM.LLVM`). Otherwise, point `LIBCLANG_PATH` at a folder that contains
  `libclang.dll` (a Doxygen install's `bin` folder has one).
- The Windows SDK's resource compiler (`rc.exe`), which comes with Visual Studio's C++
  tools, for the icon and version information.
- For the installer: [NSIS](https://nsis.sourceforge.io) 3 (`winget install NSIS.NSIS`).
- To run a development build: WinFsp (`winget install WinFsp.WinFsp`), since only the
  installer carries it.

```
cargo build --release
```

This builds `sp404drive.exe`, `sp404drive-host.exe` and `sp404drive-tray.exe` in
`target\release\`; they belong in the same folder. `.cargo\config.toml` links the C runtime
statically, so they need no Visual C++ Redistributable.

The installer:

```
powershell -File windows\build-installer.ps1
```

It builds the programs (`-SkipBuild` uses those already built), downloads WinFsp's
installer (the version pinned in the script) once into `target\installer` and checks its
SHA-256 every time, writes the app icon and the licence page (the README's
use-at-your-own-risk notice, then the GPL), and compiles `windows\installer.nsi`. The
result is `target\installer\SP-404-Drive-<version>-windows-x64-setup.exe`, about 2.6 MB.
It isn't signed yet.

## macOS

You need:

- Xcode 26 and [XcodeGen](https://github.com/yonaskolb/XcodeGen) (`brew install
  xcodegen`).
- Rust through rustup (`brew install rustup`, then `rustup default stable`). The build uses
  the `aarch64-apple-darwin` target.
- An Apple Developer team whose provisioning profiles may carry the FSKit entitlement
  (`com.apple.developer.fskit.fsmodule`). `macos/project.yml` names the project's team
  (`DEVELOPMENT_TEAM`) and bundle ids (`com.polecatplugins.sp404drive` and `.fsmodule`);
  change both to build under your own. Signing is automatic: Xcode creates the profiles.
  If automatic signing fails with a vague provisioning error, check that the latest
  Program License Agreement has been accepted.

Build and install a development build:

```
cd macos
xcodegen generate
xcodebuild -project SP404Drive.xcodeproj -scheme SP404Drive -configuration Release \
  -derivedDataPath build -allowProvisioningUpdates -allowProvisioningDeviceRegistration build
ditto "build/Build/Products/Release/SP-404 Drive.app" "/Applications/SP-404 Drive.app"
open "/Applications/SP-404 Drive.app"
```

(Remove the old app first, unmounted, so no stale files are left inside it.)

The extension target's first build phase runs `macos/build-rust.sh`, which runs `cargo
build -p sp404-ffi --target aarch64-apple-darwin` (with `--release` for Release) and links
`libsp404_ffi.a` with IOKit, CoreFoundation and libiconv. It finds cargo in
`/opt/homebrew/opt/rustup/bin` or `~/.cargo/bin`. The app target's last build phase runs
`build-rust.sh cli`, which builds the `sp404drive` command, copies it into the app's
`Contents/MacOS` and signs it with the app's identity and the hardened runtime; Xcode signs
the app after that, so the app's signature covers it. The phases run outside Xcode's script
sandbox, since cargo reads `~/.cargo` and writes `target/`.

A notarised release (Developer ID). `notarytool` needs a keychain profile made once with
`xcrun notarytool store-credentials`; put its name in place of `<profile>`:

```
cd macos
xcodegen generate
xcodebuild archive -project SP404Drive.xcodeproj -scheme SP404Drive -configuration Release \
  -derivedDataPath build -archivePath build/release/SP404Drive.xcarchive \
  -allowProvisioningUpdates -allowProvisioningDeviceRegistration
xcodebuild -exportArchive -archivePath build/release/SP404Drive.xcarchive \
  -exportPath build/release/export -exportOptionsPlist ExportOptions.plist \
  -allowProvisioningUpdates
cd build/release
ditto -c -k --keepParent "export/SP-404 Drive.app" submit.zip
xcrun notarytool submit submit.zip --keychain-profile <profile> --wait
xcrun stapler staple "export/SP-404 Drive.app"
ditto "export/SP-404 Drive.app" "SP-404 Drive.app"
ditto -c -k --keepParent "SP-404 Drive.app" SP-404-Drive-<version>-macos-arm64.zip
spctl -a -vv "SP-404 Drive.app"
```

Notes on that:

- `ExportOptions.plist` asks for `developer-id`, automatic signing and the team. The export
  re-signs everything inside the app with the Developer ID certificate and a timestamp,
  `Contents/MacOS/sp404drive` included; a bare executable there needs nothing extra.
  Xcode creates a "Mac Team Direct Provisioning Profile" with the FSKit entitlement.
- **Staple, then zip again.** The zip sent to the notary service has no ticket in it.
- `spctl -t exec` on the bare command says "does not seem to be an app", which is normal
  for something that isn't a bundle.
- The archive warns that `SP404FS.appex` "must be embedded in the parent app bundle's
  Extensions directory"; the archived app has it there and works, so the warning is
  spurious.
- The app's version (`CFBundleShortVersionString` in `project.yml`) is kept in step with
  `Cargo.toml`; XcodeGen writes 1.0 unless told otherwise.
- `macos/build` is ignored by git, as are the generated project, Info.plists and
  entitlements file.

## Before committing

`cargo fmt --all`, `cargo clippy --workspace --all-targets` (no warnings) and `cargo test
--workspace`. The tests run against an in-memory device (`MemStore`) that behaves like the
unit: it refuses to change open files, closes idle handles and hands out the lowest free
handle number. To check the shared code still compiles for the other platform: `rustup
target add aarch64-apple-darwin` (or the Windows target) and `cargo check --target ...`.

Commits use a GitHub noreply address (`<id>+<login>@users.noreply.github.com`), not a
personal email. A `commit-msg` hook in `.githooks/` checks this; switch it on once per
clone:

```
git config core.hooksPath .githooks
```

## Code layout

```
src/link/      The serial link: request/reply logic over a Port (mod.rs, platform-neutral,
               on sp404-proto), the Windows port with overlapped I/O (windows.rs), and
               the macOS port, waiting in select(2) (macos.rs).
src/store.rs   Store trait: what the drive needs from the device. DeviceStore implements it
               over the link; MemStore (tests) in memory.
src/vfs.rs     The filesystem, independent of the OS: path mapping, listing cache, shared
               open-file state, read-handle pool, write buffering. Unit-tested.
src/winfs.rs   Windows front end: WinFsp callbacks -> vfs::Drive.
src/host.rs    The background host on Windows: serving the drive, and how sp404drive
               starts it (through Explorer, reporting over a named pipe) and stops it
               (a named event and mutex per mount point).
src/items.rs   The macOS core over vfs::Drive, for FSKit: numbered items (the root is
               item 2), open modes and the upload when the last writer closes, files
               deleted or replaced while open, NFC name matching, and Mac housekeeping
               files kept in memory. Unit-tested.
src/main.rs    sp404drive(.exe), the command line; src/mac_cli.rs is its macOS part
               (mount and unmount through mount -F and umount). Unit-tested.
src/bin/sp404drive-host.rs
               sp404drive-host.exe, the windowless background process.
src/bin/sp404drive-tray/
               sp404drive-tray.exe, the notification-area app on Windows: main.rs (the
               menu, mounting on switch-on, the installer's options), ui.rs (the icon,
               menu and dialogs), settings.rs (the registry: settings, Open at Sign-in,
               the system PATH), logo.rs (the logo drawn in code, and the .ico writer,
               shared with build.rs).
build.rs       WinFsp's delay-loading, and the Windows resources: the app icon, version
               information, the tray app's manifest.
windows/       The installer: installer.nsi (NSIS), build-installer.ps1 (builds it,
               fetching and checking WinFsp's installer), THIRD-PARTY-NOTICES.txt.
.cargo/        config.toml: the C runtime linked statically on Windows.
ffi/           The macOS core as a static library with a C interface (crate sp404-ffi,
               libsp404_ffi.a): src/lib.rs (open and close the drive, items, read, write,
               space, logging; errors as errno values; panics caught), include/sp404.h,
               and include/module.modulemap so Swift can import it as SP404Core.
macos/         The Mac app and its FSKit extension (Swift), as an XcodeGen project
               (project.yml): the menu-bar app in App/ (AppDelegate.swift: the menu,
               mounting on switch-on and unmounting on switch-off; UnitWatcher.swift:
               IOKit notifications for the unit's serial port; Drive.swift: runs
               sp404drive, reads the mount table; AppIcon.icon: the app icon),
               Extension/FileSystem.swift (loading the resource, the URL's settings,
               giving up on a load that's never activated), Extension/Volume.swift (every
               FSKit call -> sp404.h), Extension/Item.swift (an item and its in-memory
               extended attributes), build-rust.sh (builds ffi/ and the sp404drive
               command for Xcode), and ExportOptions.plist (the Developer ID export).
examples/      Checks run against the unit with the drive unmounted: link_check (the
               link), idle_handles (issue #1 and the unit's session), list_tree (what is
               really in a folder).
docs/          This page, device.md (the unit), windows.md and macos.md (how each
               version works, and what's been tested), sparkymk2.md (what comes from
               SparkyMK2).
```

## Lessons for working on it

- **No polling.** The drive never wakes up to check something: every wait is on an event
  (a serial read, a device notification, a mutex), with at most a deadline. Check new
  dependencies for hidden polling, such as a short-timeout read loop.
- **Reproduce before theorising, then split the layers.** Issue #1 ("reads return another
  file's contents") had two plausible theories, both wrong. A one-minute script reading
  files in alternating order reproduced it on the first run; a second card ruled out the
  card; a small program talking to the unit directly, with the drive unmounted, found the
  cause in four short experiments.
- **One clean run proves nothing for a timing bug.** Straight after a remount, one run had
  no bad reads at all; the next two, set up identically, had 13 and 15.
- **A test double that behaves like the device catches real bugs.** Making `MemStore`
  refuse to change open files reproduced an append bug and caught the stale-size bug that
  followed; making it close idle handles and hand out the lowest free number reproduced
  issue #1 before the fix.
- **`cargo test` doesn't rebuild the binary you launched by hand.** Run `cargo build`
  before live tests.
- **Test copies of folders the way they really exist,** with their housekeeping files, not
  only clean test files.
- **Write "not tested" until it's measured,** then replace it with the number.
