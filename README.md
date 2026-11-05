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
src/items.rs   Numbered items over vfs::Drive, for FSKit: item numbers, open modes, the
               upload when the last writer closes, and Mac housekeeping files kept in
               memory. Unit-tested.
src/main.rs    sp404drive(.exe), the command line; src/mac_cli.rs is its macOS part
               (mount and unmount through mount -F and umount).
src/bin/sp404drive-host.rs
               sp404drive-host.exe, the windowless background process.
ffi/           The core as a static library with a C interface (crate sp404-ffi):
               src/lib.rs, include/sp404.h, and a module map so Swift can import it.
macos/         The Mac app and its FSKit extension (Swift), as an XcodeGen project:
               Extension/FileSystem.swift (loading, options), Volume.swift (every FSKit
               call -> sp404.h), Item.swift; build-rust.sh builds ffi/ and the
               sp404drive command for Xcode.
examples/      Checks run against the unit with the drive unmounted: link_check (the
               link), idle_handles (issue #1), list_tree (what is really in a folder).
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
`~/.cargo/bin`. The app target's last build phase runs `build-rust.sh cli`, which builds
the `sp404drive` command, copies it into the app's `Contents/MacOS` and signs it with the
app's identity and the hardened runtime (with a secure timestamp when archiving); Xcode
signs the app after that, so the app's signature covers it. The phases run on every build (cargo itself skips work that's up to date)
and outside Xcode's script sandbox, since cargo reads `~/.cargo` and writes `target/`. For
now the app is **arm64 only**; an Intel or universal build (a second cargo target and
`lipo`) is left for later.

### The Mac drive

The extension serves the real drive: the root holds `Internal` and `SD-Card`, as on
Windows. Install `SP-404 Drive.app` in `/Applications`, open it once, and switch the
extension on in System Settings (see above).

The app carries the `sp404drive` command in `Contents/MacOS`. To type just `sp404drive`,
link it into `/usr/local/bin` (on the default `PATH`) once:

```
sudo mkdir -p /usr/local/bin
sudo ln -sf "/Applications/SP-404 Drive.app/Contents/MacOS/sp404drive" /usr/local/bin/sp404drive
```

The link follows the app when it's updated in place. Then, with Roland's SP-404MKII app
closed:

```
sp404drive ports                 # list connected SP-404MKII units
sp404drive mount                 # mount on ~/SP-404MKII (or: sp404drive mount <folder>)
sp404drive mount --read-only     # refuse all changes
sp404drive unmount               # unmount, and wait until it's gone and the port is free
```

The options are the same as on Windows (`--port`, `--read-only`, `--cache-secs`); `-v`
does nothing on the Mac (the log is the unified log, below). `--port` takes
`/dev/cu.usbmodem1101` or `cu.usbmodem1101` and must be an SP-404MKII's port.

How it works:

- **No background process of its own.** The drive runs in the extension, which macOS
  starts in a process of its own for each mount, so `mount` just runs
  `mount -F -t sp404 <url> <folder>` and returns once it's mounted (0.5 s). Closing the
  terminal leaves the drive mounted.
- **Where it mounts.** `~/SP-404MKII` by default, created by `mount` and removed by
  `unmount`. `/Volumes` isn't possible for an ordinary user: it belongs to root, `mount -F`
  doesn't create the folder ("invalid file system"), and macOS's network-mount route
  (NetFS, as Finder's "Connect to Server" uses) refuses the `sp404` scheme ("Operation not
  supported"). Another folder must be empty (mounting would hide its contents) and is
  created if missing. The volume is listed as a browsable volume
  (`FileManager.mountedVolumeURLs` with hidden volumes skipped), so it should appear in
  Finder's sidebar under Locations; Finder itself is not tested yet.
- **Settings travel in the URL**, because the extension opens the drive in `loadResource`,
  before `-o` options arrive: `sp404://unit?port=/dev/cu.usbmodem1101&cache-secs=5`, plus
  `&read-only` and `-r` for a read-only mount. The extension logs what it received, and
  refuses an unknown setting or a bad value ("Invalid argument"). `mount` shows the URL as
  the volume's source, which is how `sp404drive` finds its mounts.
- **Unmounting.** `sp404drive unmount` runs `umount` and waits for it, at most 60 s.
  `umount` returns once the extension has saved everything, waited out the unit's session
  (up to 3 s) and closed the port. Finder's Eject and `diskutil unmount` take the same path
  (`diskutil` tested; Finder not yet); they leave the empty `~/SP-404MKII` folder behind,
  and the next `mount` uses it. A `.DS_Store` that Finder may put in that empty folder
  doesn't count as content, and `unmount` removes it with the folder. If `umount` is still
  running after 60 s, `unmount` fails (exit status 1) and says not to unplug the unit or
  start Roland's app yet, so a script doesn't carry on while the drive may still hold the
  port (not tested: nothing took that long). With no folder given, `unmount` takes
  whichever SP-404MKII volume is mounted.
- **Errors, in words:** a second mount of the same unit ("already mounted at ..."); no unit
  connected ("no SP-404MKII found", checked before mounting); the port held by another
  program ("in use by another program, probably Roland's SP-404MKII app"); macOS not
  starting the extension (it isn't switched on, or the app isn't installed: the message
  says where to switch it on); macOS older than 26; a folder that isn't empty (a lone
  `.DS_Store` doesn't count); `unmount`
  while a program has a file open or a terminal is in a folder on the drive ("in use").

`mount -F` can still be used by hand:

```
mount -F -t sp404 -r sp404://unit ~/SP404      # read-only (or: -o rdonly)
mount -F -t sp404 sp404://unit ~/SP404         # read-write
umount ~/SP404
```

How the volume works:

- **Opening and closing the drive.** `loadResource` opens the drive, on the port named in
  the URL or the first unit found by USB id (0582:02E7). It never falls back to just any
  `/dev/cu.usbmodem*`, which could be another device; the lookup by USB id works inside the
  extension's sandbox (measured with a hand mount without `port=`). Each mount gets a new extension process; a
  second load in the same process while the drive is open is refused without touching it. A held port (Roland's app) fails the mount with "Resource busy"; a missing unit
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
  as on Windows. FSKit passes each write on at once, and the program's `close()` waits until
  the file is on the unit. Unlike on Windows, **a failed upload is reported to the program**:
  the error FSKit gets from the close is what `close()` returns, so `cp` says
  "Input/output error" and exits with 1 (tested by making the close fail on purpose, in a
  test build; a real upload failure needs the unit to go away mid-write, not tested).
  `fsync`, `F_FULLFSYNC` and `sync` never reach the volume (measured), so they don't save
  anything: closing the file, or unmounting, does.
- **Names** are case-insensitive for ASCII letters and case-preserving, as on Windows, and
  are passed to the core precomposed (NFC). Measured on the unit: it stores a name as the
  UTF-8 it's given, without normalising it, so a card written by a Mac can hold decomposed
  (NFD) names; `Été.txt` and `été.txt` are two different files there. The core matches a
  name that isn't found as given against the folder's names in NFC, so such a file is found
  by its precomposed name (and the other way round), and two names that differ only in
  the case of a non-ASCII letter stay two items. The unit refuses to create some non-ASCII
  names (`É.txt`, `ÉTÉ.txt`, `Ü.txt` beside `ü.txt`, `Cafe\u{301}3.txt`) while it accepts
  others (`été.txt`, `Été.txt`, `ü.txt`, `ñ.txt`, `日本.txt`, `e\u{301}x.txt`); why is unknown.
  The drive reports such a refusal as "Input/output error".
- **Errors.** The unit refuses a request without saying why (a full card, a name it won't
  take), so a refusal is "Input/output error" (`EIO`), not "Permission denied"; "Permission
  denied" is kept for what the drive itself won't do (the fixed folders, housekeeping
  folders). A housekeeping file over 16 MiB is "File too large".
- **Deleted or replaced while open.** A file deleted, or replaced by a rename, while a
  program still has it open is never saved again, and its handle never reads the unit
  again: the path now names nothing, or another file. What that program wrote stays
  readable and writable through its handle until it closes it, then is thrown away (the
  `mkstemp` and `unlink` pattern). If its contents weren't in memory (only read, not
  written), later reads and writes through the old handle fail with "Input/output error"
  rather than return another file's data; the page cache may still serve reads of what was
  read before.
- **Space.** The volume shows the internal storage's size and free space, as on Windows,
  for `Internal` and `SD-Card` alike: the card's aren't available over USB. A space check
  before a copy onto the card would therefore use the internal storage's free space
  (Finder not tested).
- **Times, permissions, owners, flags.** Every item shows the mount time. Setting times,
  permissions, owner or flags is accepted and ignored, since the unit keeps none of them;
  only a file's size can be set.
- **Extended attributes** are kept in memory, per item, and never reach the unit. macOS
  sets them on most files it copies (Finder info, quarantine); a volume that refuses them
  makes macOS write `._` files beside each file instead. They are lost when FSKit lets an
  item go, and on unmount. With them, `cp`, `cp -p`, `cp -Rp` and `ditto` of files with
  extended attributes left no `._` file on the card (checked over the link with
  `list_tree`). Finder itself is not tested yet.
- **Mac housekeeping files** (`._*`, `.DS_Store`, `.Trashes`, `.fseventsd`,
  `.Spotlight-V100`, `.TemporaryItems`, `.apdisk`, `.VolumeIcon.icns`) are never created on
  the unit. A new file with one of these names is kept in memory instead, until it's
  deleted or the drive is unmounted, so the program writing it carries on: copying a folder
  that holds a `.DS_Store` or a `._` file with `cp -R` or `ditto` failed ("Permission
  denied", exit 1) while the drive refused them, and rsync gave up on the whole folder. Such
  a file isn't listed, since it isn't on the unit, but can be opened, read, written,
  renamed and deleted by name (so `diff -r` against the source reports it as missing). A
  copying program's temporary file renamed to such a name is taken off the unit and kept in
  memory: rsync writes `..DS_Store.XXXXXX` and ditto `.BC.T_XXXXXX`, then renames them to
  `.DS_Store` (measured). That applies only to a hidden file (its name starts with `.`) the
  drive created since it was mounted; any other file renamed to such a name (`mv song.wav
  ._song.wav`) is renamed on the unit as asked, so its contents are never lost at unmount.
  Renamed back to an ordinary name, an in-memory file goes to the unit. A folder
  that holds only such files counts as empty. Folders with these names, and such files in
  the drive's root (beside `Internal` and `SD-Card`), are refused. Ones already on the card (from a card reader) are ordinary files.
- **Unplugging.** The core notices at once (the serial read ends), logs it and fails every
  request from then on; the extension also sets FSKit's container state to "not ready".
  Whether FSKit then unmounts the volume by itself is not tested; `umount` ends it.
- **Logging.** The extension's and the core's lines go to the unified log at "notice" level
  or above, so they're kept:
  `log show --last 10m --predicate 'subsystem == "com.polecatplugins.sp404drive"'`. Every
  open, close, write and size change is also logged at "debug" level, which is only seen
  live: `log stream --level debug --predicate '...'`.

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

Writing, tested on 2026-11-05 on the unit (Release build, mounted read-write), only inside
a test folder on the SD card, which was deleted afterwards. Nothing under `Internal` was
changed (the log shows every change, all in the test folder). Contents were checked after
unmounting and mounting again, against the source files:

| Test | Result |
|---|---|
| Mount read-write | 0.5 s |
| `mkdir`; `cp` of 300 KB, 1 MB, 4 MB; `ditto` of 1 MB, 1.5 MB, 4 MB | all identical after remounting; one upload per file, at close |
| Write speed (`cp` of 32 MiB, twice) | 8.3 s and 9.1 s: 4.0 and 3.7 MB/s; a 4 MiB upload took 0.89-1.02 s |
| Overwrite longer with shorter, shorter with longer; `>` redirect over a file | correct sizes and contents |
| Append (`>>` three times; `cat >>` 1000 bytes onto 300 KB) | correct |
| `truncate` shorter and longer on a closed file | correct; uploaded at once |
| Rename in a folder, case-only rename (file and folder), move between folders, move a folder | correct |
| Rename a file while it's being written, then write more | ends up under the new name with all of it |
| A writer closes while a reader keeps the file open | uploaded at the writer's close |
| `rm`; `rmdir` of an empty folder; `rm -r` | correct |
| `rmdir` of a folder with files | refused: "Directory not empty" |
| `rename()` from `SD-Card` into `Internal` (raw `rename(2)`, not `mv`) | "Cross-device link" (`EXDEV`), before anything is sent; `Internal` unchanged. `mv` then copies and deletes, so it wasn't run |
| `cp -p`, `cp`, `ditto`, `cp -Rp` of files with extended attributes | attributes kept while mounted, gone after remounting; no `._` file on the card |
| `touch .DS_Store`, `touch ._x` | succeed; kept in memory, not on the card |
| `cp -R`, `cp -Rp`, `ditto`, `rsync -a` (twice) of a folder holding `.DS_Store` and `._a.bin` | all succeed (before the change: `cp` and `ditto` exit 1, rsync gave up); the card gets only `a.bin`, checked over the link with `list_tree` |
| A close made to fail on purpose (test build only) | `close()` returns EIO; `cp` prints "Input/output error" and exits 1 |
| `fsync`, `F_FULLFSYNC`, `sync` on a file being written | nothing reaches the volume; the file is uploaded at close |
| Issue #1: two files open, 64 KB reads alternating with 3 s pauses, so the unit closes its handles between reads (`F_NOCACHE` and fresh offsets after a fresh mount, to bypass the page cache) | 70 reads in six runs, all correct; in the two runs whose timings were printed, every read took about 20 ms, so it reached the unit |

Mounting and unmounting with `sp404drive`, tested on 2026-11-05 on the unit (Release
build, the command run from inside the installed app through a symlink, and from
`target/release`). The one write was a 300 KB file in a test folder on the card, deleted
afterwards:

| Test | Result |
|---|---|
| `sp404drive mount --read-only --cache-secs 9` | 0.48-0.55 s; `~/SP-404MKII` created; the extension logged port `/dev/cu.usbmodem1101`, cache 9 s, read-only; `touch` refused ("Read-only file system") |
| `sp404drive mount` (read-write), `mkdir`, `cp` of 300 KB | mounted without `read-only`; the file was identical after `diskutil unmount` and a new mount |
| `sp404drive mount` again; `mount <other folder>`; `mount --port cu.usbmodem1101 <other folder>` | all refused: "already mounted at ~/SP-404MKII" |
| `mount -F` by hand while mounted (the URL alone, and with other settings) | refused by the port's exclusive claim: "Resource busy"; the first mount carried on |
| `sp404drive mount` while another program holds the port | "in use by another program, probably Roland's SP-404MKII app"; the folder it created is removed again |
| `sp404drive unmount` just after use | 3.03-3.05 s (waiting out the session); `lsof` shows the port free as it returns; `~/SP-404MKII` removed |
| `diskutil unmount ~/SP-404MKII` just after use, and when idle | 3.02 s and 0.14 s; port free; the empty folder stays, and the next `mount` uses it |
| `sp404drive unmount` while a process's working directory is on the drive | "in use: a program has a file open there, or a terminal is in a folder inside it"; still mounted |
| `sp404drive unmount` with nothing mounted, or on a folder that isn't the drive | "the SP-404MKII isn't mounted" |
| `--port /dev/cu.nosuch`; `--port cu.Bluetooth-Incoming-Port` | "no serial port"; "isn't an SP-404MKII" (nothing opened) |
| A folder that isn't empty, a file instead of a folder, `--cache-secs` too large | refused before mounting |
| `mount -F` by hand with `?bogus=1`, or `?port=` not a `/dev/` path | "Invalid argument" (the log says which); the next mount works |

Fixes after a code review, tested on 2026-11-05 on the unit (Release build, read-write,
only inside a test folder on the card, deleted afterwards; contents checked after
remounting, and over the link with `list_tree`). Each case was also turned into a unit test
against the in-memory device, which fails without the fix:

| Test | Result |
|---|---|
| `B.bin` open for writing with 50,000 bytes unsaved; `rename("A.bin", "B.bin")`; one more write, then close | the write and `close()` succeed; `B.bin` holds A's 100,000 bytes (before the fix, the close saved B's stale bytes over it) |
| `B2.bin` open for writing; deleted, made again and written as `NEWNEW` by another handle; the old handle writes 1 byte and closes | `close()` returns "Input/output error"; `B2.bin` is `NEWNEW` (before: the old handle's bytes mixed into it) |
| `open(O_CREAT\|O_EXCL)`, `unlink`, write 32 KB, read it back (`F_NOCACHE`), close | written and read back correctly; nothing reaches the card (before: the file came back at close) |
| `K.bin` open for reading; `rename("R.bin", "K.bin")`; read again | K's bytes (from the page cache), never R's; the card holds R's data at `K.bin` |
| `mv ü.txt ._u.txt` and `mv K.bin .apdisk` (files already on the card) | renamed on the card, contents kept (before: taken off the unit into memory, gone at unmount) |
| `cp -R`, `ditto`, `rsync -a` (twice) of a folder holding `.DS_Store` and `._a.bin` | all exit 0, contents identical; the log shows rsync's and ditto's temporary files taken off the unit; the card holds only `a.bin` and `sub/b.txt` |
| Files with names in NFD, and `Été.txt` beside `été.txt`, put on the card over the link | `éx.txt` typed precomposed finds the decomposed file (same inode as its NFD name); `Été.txt` and `été.txt` are two items with their own contents; `ÉX.TXT` isn't found (non-ASCII case counts) |
| `touch É.txt` (a name the unit refuses) | "Input/output error" (before: "Permission denied") |
| `mount -F -t sp404 -r sp404://unit` (no port) | the extension found the unit by USB id (`/dev/cu.usbmodem1101`) |
| `~/SP-404MKII` holding only a `.DS_Store`; `sp404drive mount`, then `unmount` | mounted; `unmount` (3.02 s) removed the folder and the `.DS_Store` |
| `cp` of 32 MiB, remounted read-only, compared | 8.36 s (4.0 MB/s, as before); identical; `touch` refused ("Read-only file system") |

Not tested yet: Finder (its sidebar, Eject, copies), Spotlight and Quick Look, unplugging
while mounted (and so a real upload failure), mounting with no unit attached (the
extension's "not supported by device" path; `sp404drive` checks for the unit first), the
message when the extension is switched off (`mount -F` of an unknown file system type
prints only "Unable to invoke task", which `sp404drive` reads as "not switched on"; the
switched-off case should look the same), macOS before 26, `sp404drive unmount` giving up
after 60 s, a Developer ID export and notarisation of this version (with the command inside the app).

Still to do for the Mac version:

1. testing Finder copies and moves, Eject and the sidebar, and Spotlight and Quick Look
   (they shouldn't read whole files);
2. unplugging while mounted, and what FSKit does then;
3. an Intel or universal build, and a notarised release;
4. perhaps mounting from the app itself (a menu bar item), and at login or plug-in.

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
