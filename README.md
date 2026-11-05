# sp404-drive

Mount a Roland SP-404MKII as a drive. Plug the unit in over USB and it appears as a drive
letter with two folders:

```
S:\
  Internal\    the device's own storage: ROLAND\SP-404MKII\PROJECT_01..16, FCTRY, ...
  SD-Card\     the card in its slot: IMPORT, EXPORT, BKUP, ...
```

Explorer, DAWs and the command line can browse, copy, create, rename and delete files on it.

> Not affiliated with or endorsed by Roland Corporation. "SP-404MKII" and "Roland" are
> trademarks of Roland Corporation.

The drive talks to the device over its USB serial port, using the protocol mapped by
[SparkyMK2](https://github.com/TuscanPL/SparkyMK2). Message framing and the file API's
encoding come from SparkyMK2's `sp404-proto` crate, pinned to commit `3007b5d`. The
serial link itself is the drive's own and is fully event-driven: while idle, the drive
uses no CPU and none of its threads wake up.

## Requirements (Windows)

- [WinFsp](https://winfsp.dev): `winget install WinFsp.WinFsp`.
- Rust (stable, MSVC): `winget install Rustlang.Rustup`.
- To build: libclang, which `winfsp-sys` needs to generate its bindings. Any LLVM install
  works (`winget install LLVM.LLVM`). Otherwise, point `LIBCLANG_PATH` at a folder that
  contains `libclang.dll`. The Doxygen install's `bin` folder has one.

## Build

```
cargo build --release
```

This builds two programs in `target\release\`, which belong in the same folder:

- `sp404drive.exe`: the command you type.
- `sp404drive-host.exe`: the drive itself, run in the background. It's built as a Windows
  GUI program that creates no window, so it never has a window or a console, whoever
  starts it.

## Install

Copy both into `%LOCALAPPDATA%\Programs\sp404drive\` and add that folder to your user
PATH. To update, unmount, then copy the new pair over the old.

## Use

Quit Roland's SP-404MKII app first. Only one program can use the device's link at a time.

```
sp404drive ports               # list connected SP-404MKII units
sp404drive mount               # mount on S: (or: sp404drive mount T:, or an empty folder)
sp404drive unmount             # unmount S:, and wait until it's gone
```

`mount` always runs the drive in the background. It asks Explorer to start
`sp404drive-host.exe`, as if from the Run box, waits for the host to report through a
named pipe that the drive is mounted (or why it isn't), and returns. Because Explorer
starts it, the drive is independent of whatever ran `mount`: closing that terminal (or
Claude Code, or an IDE), or pressing Ctrl+C in it, leaves the drive mounted. `mount`
needs Explorer's desktop to be running, and says so if it isn't. Mounting a second time
on the same letter is refused.

The drive unmounts when you:

- run `sp404drive unmount [S:]`, or
- unplug the device or switch it off. The serial read that is always waiting on the port
  fails at that moment, and the drive unmounts. Nothing polls.

On the way out, it saves any file still being written, then waits until 3 seconds have
passed since its last message to the unit before letting go of the port. The unit treats
any message as the start of a short session, and closing the port during one while pads
are being played quickly can freeze it (see "How it behaves"). So `unmount` can take up to
3 seconds longer than the saving itself. Signing out of Windows ends the drive without
that cleanup.

The drive logs to `%LOCALAPPDATA%\sp404drive\logs\S.log` (one file per mount point,
started afresh on each mount).

### Day-to-day sequence

The drive and Roland's SP-404MKII app can't use the unit at the same time. Nothing mounts
the drive automatically, so once unmounted it stays unmounted, across unplugging the unit
and across reboots, until you run `sp404drive mount` again. To switch to the app:

1. `sp404drive unmount` (it waits until the drive has gone and the port is free).
2. Use Roland's app.
3. Quit the app, then `sp404drive mount` when you want the drive back.

If you skip step 1, the app won't find the unit, because the drive holds the port. If the
app is still open at step 3, `mount` reports that the port is busy and asks whether the
app is still connected.

**Before switching the unit off or unplugging it**, make sure nothing is still being
written:

- A file is sent to the unit when the program writing it closes it, and Windows waits
  until the unit has confirmed it. So an Explorer copy is finished when its dialog closes
  (its progress bar sits near 100% while the file goes over, at about 3.7 MB/s). A file
  written by another program, such as a DAW render, is sent when that program closes it.
- For certainty, run `sp404drive unmount` first. It saves anything still waiting and only
  says "Unmounted" once the unit has confirmed. This is the drive's "Safely Remove".
- Don't pull the cable while playing pads quickly within 3 seconds of using the drive: the
  unit can freeze (see "How it behaves").

If the unit loses power during an upload, the file on it is left incomplete, and the
program that wrote it isn't told; only the log records the failure.

Options:

| Option | Meaning |
|---|---|
| `--port COM4` | use this serial port instead of the first SP-404MKII found |
| `--read-only` | refuse all changes |
| `--cache-secs N` | reuse a folder listing for N seconds (default 5) |
| `-v`, `-vv` | debug or trace logging in the log file |

## How it behaves

- **Speed.** Everything goes over the device's serial link. Reads and writes run at about
  3.8 MB/s. A folder listing needs one request per file to learn its size; a full `SMPL`
  folder (150 samples) takes about 0.3 seconds to list. Listings are cached for 5 seconds.
  On an exFAT card (the usual format above 32 GB) the unit takes 2–3 times as long to find
  each file, so listings and opens are that much slower; reading data is not affected.
- **Open files after a pause.** The unit closes every open file once nothing has been
  asked of it for about 2 seconds, without saying so, and gives the same handle numbers to
  the next files opened. The drive keeps up to 8 files open between reads, so after a pause
  it reopens them as needed, at the cost of a few milliseconds per file. (Before this was
  handled, reads could return another file's contents: issue #1.)
- **Writes are uploaded when the file is closed.** The device's only dependable write is
  "create or replace a whole file", so a file being written is held in memory and
  uploaded when the program closes it. The same happens on unmount. If the upload fails,
  the error is logged; the program that wrote the file has already closed it and isn't
  told.
- **Moving between `Internal` and `SD-Card`** is a copy followed by a delete, because they
  are separate filesystems on the device. Explorer and other tools do this automatically.
- **No dates.** The device keeps no timestamps, so every file shows the time the drive was
  mounted.
- **Capacity.** The size Windows shows for the drive is the unit's internal storage: its
  published 16 GB (shown as 14.9 GB, as Windows counts in binary units) and the free space
  the unit reports. Used space is the difference, so it includes the file system's own
  structures, as on any drive. The card's size and free space aren't available over USB,
  so the figures don't cover `SD-Card`.
- **The unit's session, and a freeze.** Every message from a computer starts or extends
  a session of about 2.2-2.5 seconds on the unit. While it lasts, each pad hit makes the
  unit send a message back. If the port closes during a session while pads are being hit
  quickly, the unit freezes: controls and the power switch stop working. The drive keeps
  the port open while mounted and reads everything, and waits the session out before
  closing it, so this can only happen if the cable is pulled at the wrong moment. To bring
  a frozen unit back, open and close its serial port, for example by running
  `sp404drive mount` and `sp404drive unmount`.
- **Names.** Lookups ignore case, as on Windows, and the drive uses the device's own
  spelling of each name. A full path on the device can be at most 199 bytes.
- **Internal storage is live.** Changing files under `Internal\ROLAND\SP-404MKII` edits the
  device's projects directly, and the unit reads some files only when a project loads. Use
  `--read-only` when you only want to look.

## Mounting and unmounting on Windows: the options

| Option | Status |
|---|---|
| `sp404drive mount` (always in the background) / `unmount` | implemented |
| Unmount on unplug (event-driven) | implemented |
| Mount on an empty NTFS folder instead of a letter (`sp404drive mount C:\SP404`) | implemented (WinFsp supports it); not tested yet |
| Start at login: a shortcut to `sp404drive mount` in `shell:startup`, or a Task Scheduler task | should work as is; not tested |
| Mount on plug-in: watch for USB `0582:02E7` and run `mount` | not yet |
| "Map network drive" / `net use S: \\sp404\mk2`, unmounting with Explorer's *Disconnect* (registers the binary with the WinFsp Launcher and mounts with a UNC prefix) | not yet |
| Tray icon with Mount/Unmount | not yet |

A WinFsp drive has no "Safely Remove" or "Eject" entry. Unmounting is the equivalent.
A drive letter created by an elevated process is invisible to the normal (non-elevated)
Explorer. Since Explorer starts the host, it runs unelevated even when `mount` is run
from an elevated terminal, so the letter should be visible either way (not tested yet).

## Code layout and the Mac version

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
src/items.rs   Numbered items over vfs::Drive, for FSKit: item numbers, open modes, and
               the upload when the last writer closes. Unit-tested.
src/main.rs    sp404drive.exe, the command line.
src/bin/sp404drive-host.rs
               sp404drive-host.exe, the windowless background process.
ffi/           The core as a static library with a C interface (crate sp404-ffi):
               src/lib.rs, include/sp404.h, and a module map so Swift can import it.
macos/         The Mac app and its FSKit extension (Swift), as an XcodeGen project:
               Extension/FileSystem.swift (loading, options), Volume.swift (every FSKit
               call -> sp404.h), Item.swift; build-rust.sh builds ffi/ for Xcode.
```

On macOS the serial link works: `sp404drive ports` finds the unit, and
`cargo run --example link_check` lists both volumes and reads a file over it (tested on
2026-11-05, macOS 26 on Apple Silicon). The unit appears as `/dev/cu.usbmodem*` through
Apple's own USB serial driver; no Roland driver is needed. `link/macos.rs` opens it raw at
921600 baud with DTR and RTS asserted and claims it exclusively (`TIOCEXCL`). The reader
waits in `select(2)` on the port and a wake-up pipe, with no timeout (`poll(2)` doesn't
support device files on macOS); unplugging the unit ends that wait at once, as on Windows.
The port is found by USB id (0582:02E7) through the `serialport` crate's port listing; its
own port code isn't used.

The Mac version will be an FSKit file system: Apple's own framework, with no third-party
software to install (macFUSE or FUSE-T would be a second install, and macFUSE also needs a
system extension approved). It ships as a signed, notarised `SP-404 Drive.app` that carries
the extension; the user switches the extension on once, in System Settings > General >
Login Items & Extensions > Extensions, By Category > File System Extensions. It needs
macOS 26 or later.

`macos/` holds the Xcode project (generated from `project.yml` with
[XcodeGen](https://github.com/yonaskolb/XcodeGen)). The first version of its extension was
a check: a read-only volume with one file saying whether the sandboxed extension could open
the unit's serial port. On 2026-11-05 it showed that:

- the FSKit entitlement (`com.apple.developer.fskit.fsmodule`) is granted in Developer ID
  provisioning profiles;
- a volume with no disk behind it works (`FSSupportsGenericURLResources`, URL scheme
  `sp404`);
- an ordinary user can mount it, without root: `mount -F -t sp404 sp404://unit <folder>`,
  and unmount it with `umount`;
- the sandboxed extension can open `/dev/cu.usbmodem*` (`com.apple.security.device.serial`);
- the app exports for Developer ID, notarises, and passes Gatekeeper.

### Building the Mac version

```
cd macos
xcodegen generate
xcodebuild -project SP404Drive.xcodeproj -scheme SP404Drive -configuration Release \
  -derivedDataPath build -allowProvisioningUpdates -allowProvisioningDeviceRegistration build
```

The extension target's first build phase runs `macos/build-rust.sh`, which runs
`cargo build -p sp404-ffi --target aarch64-apple-darwin` (with `--release` for the Release
configuration) and links `target/aarch64-apple-darwin/<profile>/libsp404_ffi.a` with IOKit,
CoreFoundation and libiconv. It finds cargo in `/opt/homebrew/opt/rustup/bin` or
`~/.cargo/bin`. The phase runs on every build (cargo itself skips work that's up to date)
and outside Xcode's script sandbox, since cargo reads `~/.cargo` and writes `target/`. For
now the app is **arm64 only**; an Intel or universal build (a second cargo target and
`lipo`) is left for later.

### The Mac drive

The extension now serves the real drive: the root holds `Internal` and `SD-Card`, as on
Windows. Install `SP-404 Drive.app` in `/Applications`, open it once, switch the extension
on in System Settings (see above), then:

```
mount -F -t sp404 -r sp404://unit ~/SP404      # read-only (or: -o rdonly)
mount -F -t sp404 sp404://unit ~/SP404         # read-write (writes: not tested yet)
umount ~/SP404
```

How the volume works:

- **Opening and closing the drive.** `loadResource` opens the drive (finding the unit by
  USB id). A held port (Roland's app) fails the mount with "Resource busy"; a missing unit
  should fail it with "Operation not supported by device" (not tested). Opening it here rather than in `activate`
  matters: after a failed `activate`, FSKit keeps the resource loaded and refuses every
  later mount until the extension process ends; after a failed load it doesn't.
  Unmounting closes the drive, saving any file still being written and waiting out the
  unit's session, as on Windows.
- **Read-only.** `mount -r` and `-o rdonly` don't reach `loadResource`; they arrive in the
  volume's `activate` as `-o ro` and `-o rdonly`. The volume then asks FSKit for a
  read-only mount (`requestedMountOptions`), so the kernel refuses changes with "Read-only
  file system" before they reach the extension. The extension and the core
  (`sp_drive_set_read_only`) refuse them too, as a backstop; the tests don't reach that far.
- **Items.** Each FSKit item holds the core's item number (`src/items.rs`); the volume keeps
  one Swift object per number while FSKit holds it and tells the core when FSKit reclaims
  it. Directory listings give `.` and `..` when FSKit asks for names only, and cookies that
  find their place again if the folder changes between calls.
- **Open, close and writing.** FSKit's open and close calls carry modes (read, write, and
  the modes kept on close), which the core turns into "upload when the last writer closes",
  as on Windows.
- **Names** are case-insensitive and case-preserving, and are passed to the core
  precomposed (NFC).
- **Times, permissions, owners, flags.** Every item shows the mount time. Setting times,
  permissions, owner or flags is accepted and ignored, since the unit keeps none of them;
  only a file's size can be set.
- **Extended attributes** are kept in memory, per item, and never reach the unit. macOS
  sets them on most files it copies (Finder info, quarantine); a volume that refuses them
  makes macOS write `._` files beside each file instead, which the core refuses anyway (it
  won't create `._*`, `.DS_Store` or the other Mac housekeeping names). They are lost when
  FSKit lets an item go, and on unmount. Whether this keeps `._` files away in a Finder copy
  is not tested yet.
- **Unplugging.** The core notices at once (the serial read ends), logs it and fails every
  request from then on; the extension also sets FSKit's container state to "not ready".
  Whether FSKit then unmounts the volume by itself is not tested; `umount` ends it.
- **Logging.** The extension's and the core's lines go to the unified log at "notice" level
  or above, so they're kept:
  `log show --last 10m --predicate 'subsystem == "com.polecatplugins.sp404drive"'`.

Tested on 2026-11-05 on the unit (macOS 26, Apple Silicon, Release build, mounted
read-only):

| Test | Result |
|---|---|
| Mount (`-o rdonly` and `-r`) | 0.5–0.8 s; `mount` shows `read-only` |
| `ls -la` of the root, `Internal`, `SD-Card` | correct; `Internal` (21 entries) in 0.03 s |
| `ls -laR Internal/ROLAND` | 299 lines, 0.53 s |
| `ls -laR SD-Card` (4652 files) | 12.4 s, no errors |
| 192 files (86 MB) checksummed, remounted, checksummed again 4 at a time | identical; 27.9 s and 27.6 s (3.1 MB/s with opens) |
| `cp` of a 162,431,232-byte sample off the card | 41.0 s, 3.96 MB/s; checksum matches a second read after remounting |
| `mkdir`, `touch`, `>` redirect, `open(O_CREAT)`, `xattr -w` | all refused: "Read-only file system" |
| Mount while another program holds the port | "Resource busy"; the next mount works |
| `umount` | 3.0 s just after use (waiting out the session), 0.0 s when idle for 3 s; the port is free afterwards (`lsof`) |

Not tested yet: writing (read-write mounts), Finder, Spotlight and Quick Look, unplugging
while mounted, a Developer ID export of this version.

Still to do for the Mac version:

1. testing writes on the unit (create, write, rename, delete, inside a test folder), and
   whether Finder copies leave `._` files;
2. mounting and unmounting from `sp404drive` (`mount -F`, `umount`) or from the app;
3. checking that Spotlight and Quick Look don't read whole files, and keeping `.DS_Store`
   off the unit;
4. unplugging while mounted, and what FSKit does then;
5. an Intel or universal build, and a notarised release.

## Contributing

Commits use a GitHub noreply address
(`<id>+<login>@users.noreply.github.com`), not a personal email. A `commit-msg` hook in
`.githooks/` checks this; switch it on once per clone:

```
git config core.hooksPath .githooks
```

## What comes from SparkyMK2

We use SparkyMK2 ("Sparky") in four ways: one crate is compiled in, one file was ported, the licence was copied, and some docs and code served only as reference. Everything comes from commit `3007b5d`.

**1. Compiled in: the `sp404-proto` crate.** It's the only Sparky crate in `Cargo.toml`, as a git dependency pinned to that commit. Every call to it is in `src/link/mod.rs`:

| Sparky file | What we use |
|---|---|
| `crates/sp404-proto/src/frame.rs` | `Message`, `Channel`, `Decoder`: building and splitting the serial messages |
| `crates/sp404-proto/src/fileapi.rs` | the file requests (`open`, `close`, `read`, `write`, `seek`, `stat`, `opendir`, `readdir`, `closedir`, `mkdir`, `rmdir`, `unlink`, `rename`, `free_kb`), `parse_reply`, `Reply`, `Stat`, `DirEntry`, `CHUNK`, `WRITE_BATCH`, `ROOT`, open flags, and the path mapping that `Internal` and `SD-Card` rely on |
| `crates/sp404-proto/src/septet.rs` | used inside `fileapi.rs` to encode numbers; we don't call it directly |
| `crates/sp404-proto/src/lib.rs` | `ProtoError` |

The crate's other files (`pad.rs`, `params.rs`, `settings.rs`) are compiled because they're part of the crate, but nothing calls them.

**2. Ported: `crates/sp404-device/src/files.rs`, plus a little of that crate's `lib.rs`.** Our `src/link/mod.rs` reimplements its request-and-reply logic:
- write batching and checking the device's byte counts;
- the multi-part read;
- the 20-second wait when closing a written file;
- tolerating the SD card's refusal to close a finished directory listing;
- the 350 ms pause after opening the port.

`sp404-device` itself was a dependency in the first commit, but not any more. Its connection reads the port in a short-timeout loop, a sensible fit for an interactive app; a drive that sits in the background for hours needed a link that waits for the device without waking up, so we wrote our own connection layer and kept the protocol code.

**3. Copied: `LICENSE`**, the GPL-3.0 text.

**4. Reference only, not in the repo:**
- `docs/re/01-transport.md` and `00-findings.md` shaped the design: the two volumes, `opendir` not reporting missing folders, the file-API commands.
- Sparky's CLI (`crates/sp404-cli`) was built once at the start of the project to check the link to the unit worked. It isn't used now.
- Sparky's `sp404-formats` crate (the SMP, `PADCONF.BIN` and pattern parsers) isn't used at all, because the drive only moves files around and never interprets them.

## How it was made

The drive follows SparkyMK2's clean-room approach: it contains no Roland code, firmware or
assets. What it knows about the device comes from SparkyMK2's published specifications
and code, and from testing against a real SP-404MKII.

Like SparkyMK2, it is largely AI-written. An AI assistant (Claude) wrote most of the code
and documentation, working with the author, and the drive was tried on a real unit:
browsing both volumes, reading and writing files, renaming and deleting, unmounting, and
unplugging while mounted. Anything not yet tested is marked as such in this README.

## License

GPL-3.0-or-later (see `LICENSE`). It uses SparkyMK2's GPL-3.0 `sp404-proto` crate, and
`src/link/mod.rs` follows its `sp404-device` file API code.
