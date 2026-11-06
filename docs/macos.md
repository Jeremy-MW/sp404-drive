# The macOS version: how it works

How sp404-drive works on macOS, what we learned about FSKit on the way, and what has been
tested. For using it, see the [README](../README.md). For what we learned about the unit
itself, see [device.md](device.md).

## The pieces

`SP-404 Drive.app` holds three things:

- the menu-bar app (Swift, `macos/App/`);
- the FSKit file-system extension (Swift, `macos/Extension/`), which runs the drive. Its
  core is the same Rust code as on Windows, built as a static library with a C interface
  (`ffi/`);
- the `sp404drive` command, in `Contents/MacOS`.

FSKit is Apple's framework for file systems that run outside the kernel. A file system
with no disk behind it ("generic URL resources", scheme `sp404`) needs macOS 26.

### Why FSKit and not FUSE

The first plan was a FUSE front end on macFUSE, using its FSKit backend, which needs no
kernel extension. A test mount worked. Two things ruled it out. macFUSE is a second
install for every user, and the drive should need nothing else installed. And under the
FSKit backend, libfuse's session gives no file descriptor (`fuse_session_fd`), which is
what the Rust `fuser` crate reads requests from, so `fuser` couldn't drive it. The drive's
own FSKit module needs no third-party software at all.

## Mounting and unmounting

- **The drive runs in the extension,** which macOS starts in a process of its own for each
  mount. `sp404drive mount` runs `mount -F -t sp404 <url> <folder>` and returns once it's
  mounted; closing the terminal leaves the drive mounted. The menu-bar app mounts and
  ejects through the `sp404drive` command it carries, so it gets the same checks and
  messages.
- **Where it mounts.** `~/Library/Application Support/SP-404 Drive/SP-404MKII` by default.
  Finder leaves out volumes mounted in the visible part of the home folder: mounted on
  `~/SP-404MKII`, the volume was in neither Finder's sidebar nor its Computer view,
  although it counted as browsable and wasn't `nobrowse`. Under `~/Library` it is in both,
  as a "Remote Volume" with an Eject button. The kernel doesn't set `MNT_LOCAL` on an FSKit
  volume without a disk, and FSKit's only mount option is read-only. (Apple's developer
  forum thread 816899 reports the same for another FSKit volume.)

  | Mounted on | Finder |
  |---|---|
  | `~/SP-404MKII` | in neither the sidebar nor the Computer view |
  | `/Users/Shared/SP-404MKII` | in the Computer view, as a "Remote Volume" with an Eject button |
  | `~/Library/Application Support/SP-404 Drive/SP-404MKII` | in the sidebar under Locations, with an Eject button, and in the Computer view |

- **Not `/Volumes`.** It belongs to root; `mount -F` doesn't create a missing mount point
  (it says "invalid file system", exit 66, for any missing folder); and NetFS
  (`NetFSMountURLSync`, Finder's "Connect to Server" route) refuses an FSKit scheme with
  "Operation not supported". So `sp404drive mount` creates the folder and `unmount`
  removes it. Another folder must be empty (a lone `.DS_Store` doesn't count).
- **Settings travel in the URL,** because the extension opens the drive in `loadResource`,
  before `-o` options arrive: `sp404://unit?port=/dev/cu.usbmodem1101&cache-secs=5`, plus
  `&read-only` and `-r` for a read-only mount. The URL reaches `loadResource` verbatim
  (`FSGenericURLResource.url`). The extension logs what it received and refuses an unknown
  setting or a bad value ("Invalid argument"). `mount` shows the URL as the volume's
  source (`f_mntfromname`), which is how `sp404drive` finds its mounts in `getfsstat`.
- **Unmounting.** `sp404drive unmount` runs `umount` and waits for it, at most 60 s.
  `umount` (and Finder's Eject, and `diskutil unmount`) returns once the extension has
  saved everything, waited out the unit's session (up to 3 s) and closed the port. If
  `umount` is still running after 60 s, `unmount` fails (exit status 1) and says not to
  unplug the unit or start Roland's app yet (not tested: nothing took that long).
- **Errors, in words:** a second mount of the same unit ("already mounted at ..."); no unit
  connected ("no SP-404MKII found", checked before mounting); the port held by another
  program ("in use by another program, probably Roland's SP-404MKII app"); macOS not
  starting the extension (it isn't switched on, or the app isn't installed; the message
  says where to switch it on); macOS older than 26; a folder that isn't empty; `unmount`
  while a program has a file open or a terminal is in a folder on the drive ("in use").

`mount -F` can be used by hand:

```
mount -F -t sp404 -r sp404://unit ~/SP404      # read-only (or: -o rdonly)
mount -F -t sp404 sp404://unit ~/SP404         # read-write
umount ~/SP404
```

## Switching the unit off

- **An FSKit volume outlives its device, and the extension can't unmount it.** When the
  unit is switched off, the drive notices at once (the serial read ends with "Device not
  configured"), fails every request from then on and tells FSKit the volume is no longer
  ready (`containerStatus`). macOS leaves the volume mounted anyway, and FSKit has no call
  for an extension to unmount its own volume. A test build that called `unmount(2)` from
  the extension, plain and with `MNT_FORCE`, failed with EPERM; the kernel logged
  `Sandbox: SP404FS deny(1) file-unmount`. FSKit extensions must be sandboxed, so the
  unmount has to come from outside.
- **So the menu-bar app does it.** It watches, through IOKit notifications (no polling),
  for the unit's serial node (`IOSerialBSDClient`, matched by the USB parent's vendor and
  product ids) appearing and going. On a terminated notification it unmounts the volume:
  36–44 ms after the switch-off in the tests, also with the drive open in a Finder window.
  If the plain unmount is refused (a program has a file open there), it forces it
  (`umount -f`): nothing can be saved to a unit that's gone. Without the app running, the
  dead volume stays until it's ejected, which then takes 0.03 s.

## The volume

- **The serial link.** `src/link/macos.rs` opens the port raw at 921600 baud with DTR and
  RTS asserted and claims it exclusively (`TIOCEXCL`). The port is non-blocking, and every
  wait happens in `select(2)`: the reader waits on the port and on a wake-up pipe with no
  timeout, so it sleeps until bytes arrive, the device goes away, or another thread wakes
  it to stop. A write that finds the output buffer full waits for room, with a 5 s
  deadline. (`poll(2)` would be the obvious call, but on macOS it doesn't support device
  files.) The port is found by USB id (`0582:02E7`) through the `serialport` crate's port
  listing; its own port code isn't used.
- **Opening the drive in `loadResource`,** on the port named in the URL or the first unit
  found by USB id. It never falls back to any other `/dev/cu.usbmodem*`, which could be a
  different device; the lookup by USB id works inside the extension's sandbox. Each mount
  gets a new extension process; a second mount of the same unit runs in a fresh process
  and is refused by the port's exclusive claim ("Resource busy"), not by FSKit.
- **Fail in `loadResource`, not `activate`.** After a failed `activate`, FSKit keeps the
  resource loaded and refuses every later mount ("Resource busy") until the extension
  process ends. After a failed load, the next mount works.
- **Read-only.** `mount -r` and `-o rdonly` don't reach `loadResource` (its options are
  empty); they arrive in the volume's `activate` as `-o ro` and `-o rdonly`. The volume
  then asks FSKit for a read-only mount (`requestedMountOptions`). Without that, the mount
  wasn't read-only to the kernel (`statvfs` showed no `ST_RDONLY`) even with `-r`; with
  it, the kernel refuses changes with "Read-only file system" before they reach the
  extension. The extension and the core refuse them too, as a backstop.
- **Items.** Each FSKit item holds the core's item number (`src/items.rs`); the volume
  keeps one Swift object per number while FSKit holds it and tells the core when FSKit
  reclaims it. Directory listings give `.` and `..` when FSKit asks for names only, and
  cookies that find their place again if the folder changes between calls.
- **Writing.** FSKit's open and close calls carry modes (read, write, and the modes kept on
  close), which the core turns into "upload when the last writer closes", as on Windows.
  FSKit passes each `write()` on at once, and the program's `close()` waits for
  `closeItem`. **An error from `closeItem` is what `close()` returns,** so unlike on
  Windows a failed upload reaches the program (`cp` prints "Input/output error" and exits
  1). `fsync`, `F_FULLFSYNC` and `sync` never reach the volume at all, so they save
  nothing: closing the file, or unmounting, does.
- **Deleted or replaced while open.** A file deleted, or replaced by a `rename` over it,
  while a handle is still open on it is never saved again: it lives on in memory for that
  handle and is thrown away at close. Before this, the close uploaded stale bytes over the
  file renamed in, mixed them into a new file made at that path, or brought a deleted
  `mkstemp` file back. If the file's contents weren't in memory (only read, not written),
  later reads and writes through the old handle fail with "Input/output error" rather than
  return another file's data; the page cache may still serve what was read before.
- **Names** are passed to the core precomposed (NFC). The unit stores names as raw UTF-8
  without normalising them (see [device.md](device.md#names)), so the core matches a name
  that isn't found as given against the folder's names in NFC, and ignores case for ASCII
  letters only: two names that differ only in the case of a non-ASCII letter stay two
  items.
- **Errors.** The unit refuses requests without saying why, so a refusal is "Input/output
  error" (`EIO`), not "Permission denied"; "Permission denied" is kept for what the drive
  itself won't do (the fixed folders, housekeeping folders). A housekeeping file over
  16 MiB is "File too large". A path over 199 bytes is "Invalid argument".
- **Moving between `Internal` and `SD-Card`:** `rename` returns "Cross-device link"
  (`EXDEV`) before anything is sent, and `mv` then copies and deletes.
- **No dates.** Every file shows the mount time. Setting times, permissions, owner or flags
  is accepted and ignored; only a file's size can be set.
- **Capacity.** `df` shows the internal storage's size and free space for both folders,
  since the card's aren't available over USB, so a space check before a copy onto the
  card uses the internal storage's free space.
- **Memory.** A file being written is held whole in the extension's memory until it's
  uploaded. Housekeeping files are capped at 16 MiB each.

### Mac housekeeping files

macOS writes files the unit has no use for: `._*` (extended attributes on a volume that
refuses them), `.DS_Store`, `.Trashes`, `.fseventsd`, `.Spotlight-V100`,
`.TemporaryItems`, `.apdisk`, `.VolumeIcon.icns`.

- **Refusing them breaks whole copies.** When the drive refused `.DS_Store` and `._*` with
  "Permission denied", `cp -R` and `ditto` of a folder holding one exited with 1, and
  openrsync gave up on the folder entirely.
- **So they're kept in memory instead,** until deleted or unmounted, and never created on
  the unit. Such a file isn't listed (so `diff -r` against the source reports it as
  missing), but it can be opened, read, written, renamed and deleted by name. Ones already
  on the card, from a card reader, are ordinary files. A folder holding only such files
  counts as empty. Folders with these names, and such files beside `Internal` and
  `SD-Card`, are refused.
- **Copy tools write under temporary names.** rsync writes `..DS_Store.XXXXXX` and ditto
  `.BC.T_XXXXXX`, then rename them to `.DS_Store`; openrsync renames a file while it's
  still open, then closes it, so the upload has to follow the rename. A hidden file (name
  starting with `.`) the drive created in this mount and then renamed to a housekeeping
  name is taken off the unit into memory. Any other file renamed to such a name (`mv
  song.wav ._song.wav`) is renamed on the unit as asked, so its contents aren't lost at
  unmount. Renamed back to an ordinary name, an in-memory file goes to the unit.
- **Extended attributes** are kept in memory per file and never sent to the unit, so `cp`,
  `cp -p`, `cp -Rp` and `ditto` leave no `._` file on the card. They're lost when FSKit
  lets the item go, and on unmount.

## Logging

The drive's lines go to the unified log at "notice" level or above, so they're kept:
`/usr/bin/log show --last 10m --predicate 'subsystem == "com.polecatplugins.sp404drive"'`.
Every open, close, write and size change is also logged at "debug" level, which is only
seen live: `/usr/bin/log stream --level debug --predicate '...'`. (In zsh, plain `log` is
a shell builtin, hence the full path.)

## Lessons about macOS and FSKit

- **Prove the risky parts with a throwaway extension first.** A test volume holding one
  file (`status.txt`, saying whether the port opened) answered the four questions that
  could have sunk the route: the FSKit entitlement (`com.apple.developer.fskit.fsmodule`)
  was granted and automatic signing put it in the profiles; a volume with no disk behind
  it works (`FSSupportsGenericURLResources`), on macOS 26; an ordinary user can mount it
  with `mount -F`, no root; and the sandboxed extension can open `/dev/cu.usbmodem*` with
  `TIOCEXCL` (`com.apple.security.device.serial`).
- **The switch is under By Category.** System Settings > General > Login Items &
  Extensions > Extensions, **By Category** > File System Extensions. `pluginkit -e use`
  doesn't count for FSKit. Once on, it stayed on through every replacement of the app,
  across Developer ID and Apple Development signatures.
- **`FSClient.installedExtensions` doesn't list third-party extensions** (only Apple's
  exFAT, MS-DOS and FTP), so it can't tell whether ours is switched on.
- **`mount -F` messages:** a failed `loadResource` prints "Loading resource: ... <error>"
  then "Unable to invoke task" (exit 69); an unknown file system type prints only the
  latter. So "Unable to invoke task" alone means macOS didn't start the extension (most
  likely switched off; assumed, not tested).
- **Mounting while macOS registers a replaced extension can wedge FSKit.** Once, a mount
  within a second of the app being replaced loaded the resource (and opened the port),
  then `fskitd` logged "Can't start new task, previous task is running" instead of
  activating, and refused the next mount with "resource state is 4". Only ending the
  extension process cleared it; four more replacements didn't repeat it. Now the extension
  gives up 10 s after a load that's never activated (it lets the port go and exits), and
  the app retries its launch mount once, 15 s after a failure. Reading `fskitd`'s log
  lines found it; `sp404drive` only saw a failed `mount -F`.
- **A busy unmount can be transient.** Once, straight after an `rsync`, `umount` said
  "Resource busy" though `lsof` showed nothing on the volume a moment later, and the retry
  worked. Something system-side, most likely indexing, briefly held it.
- **The page cache hides the device.** Reads of a file already read once in that mount
  come from memory (0.1 ms). To test what reaches the unit, mount afresh, open with
  `F_NOCACHE`, use new offsets each run, and print each read's time (about 20 ms per 64 KB
  from the unit).
- **Never test EXDEV with `mv`.** `mv` falls back to copying when `rename()` says
  "Cross-device link", so it would write to the destination. Use a raw `rename(2)` (Python's
  `os.rename`).
- **A temporary fault injection answers "would the error reach the program?"** A real
  upload failure needs the unit to go away mid-write. A test build that failed `closeItem`
  for one file name answered it in a minute; the injection was never committed.
- **A Debug build hides the code in `SP404FS.debug.dylib`,** beside the extension's
  executable; `nm` on the executable itself shows none of the core's symbols.
- **Draw small template images in code.** The menu-bar icon is the logo drawn with
  `NSBezierPath` into an `NSImage(size:flipped:drawingHandler:)` marked `isTemplate`, so
  it's sharp at any scale and macOS colours it. The app icon is an Icon Composer document
  (`AppIcon.icon`: `icon.json` plus SVG layers) written by hand; `ictool` (inside Icon
  Composer.app) renders each appearance to PNG for checking.

## Tested on the unit

All on macOS 26 on Apple silicon, Release builds. The unit's projects were only read:
every write went to a test folder on the SD card, deleted afterwards.

Read-only (2026-11-05):

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

Writing (2026-11-05), contents checked after unmounting and mounting again:

| Test | Result |
|---|---|
| Mount read-write | 0.5 s |
| `mkdir`; `cp` of 300 KB, 1 MB, 4 MB; `ditto` of 1 MB, 1.5 MB, 4 MB | all identical after remounting; one upload per file, at close |
| Write speed (`cp` of 32 MiB, twice) | 8.3 s and 9.1 s: 4.0 and 3.7 MB/s; a 4 MiB upload took 0.89–1.02 s |
| Overwrite longer with shorter, shorter with longer; `>` redirect over a file | correct sizes and contents |
| Append (`>>` three times; `cat >>` 1000 bytes onto 300 KB) | correct |
| `truncate` shorter and longer on a closed file | correct; uploaded at once |
| Rename in a folder, case-only rename (file and folder), move between folders, move a folder | correct |
| Rename a file while it's being written, then write more | ends up under the new name with all of it |
| A writer closes while a reader keeps the file open | uploaded at the writer's close |
| `rm`; `rmdir` of an empty folder; `rm -r` | correct |
| `rmdir` of a folder with files | refused: "Directory not empty" |
| `rename()` from `SD-Card` into `Internal` (raw `rename(2)`) | "Cross-device link" (`EXDEV`), before anything is sent; `Internal` unchanged |
| `cp -p`, `cp`, `ditto`, `cp -Rp` of files with extended attributes | attributes kept while mounted, gone after remounting; no `._` file on the card |
| `touch .DS_Store`, `touch ._x` | succeed; kept in memory, not on the card |
| `cp -R`, `cp -Rp`, `ditto`, `rsync -a` (twice) of a folder holding `.DS_Store` and `._a.bin` | all succeed; the card gets only `a.bin` |
| A close made to fail on purpose (test build only) | `close()` returns EIO; `cp` prints "Input/output error" and exits 1 |
| `fsync`, `F_FULLFSYNC`, `sync` on a file being written | nothing reaches the volume; the file is uploaded at close |
| Issue #1: two files open, 64 KB reads alternating with 3 s pauses, so the unit closes its handles between reads (`F_NOCACHE`, fresh offsets, fresh mount) | 70 reads in six runs, all correct; every timed read took about 20 ms, so it reached the unit |

Mounting and unmounting with `sp404drive` (2026-11-05):

| Test | Result |
|---|---|
| `sp404drive mount --read-only --cache-secs 9` | 0.48–0.55 s; the extension logged the port, cache 9 s, read-only; `touch` refused |
| `sp404drive mount` (read-write), `mkdir`, `cp` of 300 KB | identical after `diskutil unmount` and a new mount |
| `sp404drive mount` again, on another folder, or with `--port` | all refused: "already mounted at ..." |
| `mount -F` by hand while mounted | refused by the port's exclusive claim ("Resource busy"); the first mount carried on |
| `sp404drive mount` while another program holds the port | "in use by another program, probably Roland's SP-404MKII app"; the folder it created is removed again |
| `sp404drive unmount` just after use | 3.03–3.05 s; the port free as it returns; the folder removed |
| `diskutil unmount` just after use, and when idle | 3.02 s and 0.14 s; port free; the empty folder stays, and the next `mount` uses it |
| `sp404drive unmount` while a process's working directory is on the drive | "in use: ..."; still mounted |
| `sp404drive unmount` with nothing mounted, or on a folder that isn't the drive | "the SP-404MKII isn't mounted" |
| `--port /dev/cu.nosuch`; `--port cu.Bluetooth-Incoming-Port` | "no serial port"; "isn't an SP-404MKII" (nothing opened) |
| A folder that isn't empty, a file instead of a folder, `--cache-secs` too large | refused before mounting |
| `mount -F` by hand with `?bogus=1`, or `?port=` not a `/dev/` path | "Invalid argument" (the log says which); the next mount works |

Fixes after a code review (2026-11-05). Each case is also a unit test against the
in-memory device, which fails without the fix:

| Test | Result |
|---|---|
| `B.bin` open for writing with 50,000 bytes unsaved; `rename("A.bin", "B.bin")`; one more write, then close | the write and `close()` succeed; `B.bin` holds A's 100,000 bytes (before: the close saved B's stale bytes over it) |
| `B2.bin` open for writing; deleted, made again and written by another handle; the old handle writes 1 byte and closes | `close()` returns "Input/output error"; the new `B2.bin` is intact (before: the old handle's bytes mixed into it) |
| `open(O_CREAT\|O_EXCL)`, `unlink`, write 32 KB, read it back, close | correct; nothing reaches the card (before: the file came back at close) |
| `K.bin` open for reading; `rename("R.bin", "K.bin")`; read again | K's bytes (from the page cache), never R's |
| `mv ü.txt ._u.txt` and `mv K.bin .apdisk` (files already on the card) | renamed on the card, contents kept (before: taken into memory, gone at unmount) |
| Files with names in NFD, and `Été.txt` beside `été.txt` | `éx.txt` typed precomposed finds the decomposed file; `Été.txt` and `été.txt` are two items; `ÉX.TXT` isn't found |
| `touch É.txt` (a name the unit refuses) | "Input/output error" (before: "Permission denied") |
| `mount -F -t sp404 -r sp404://unit` (no port) | the extension found the unit by USB id |
| Mount folder holding only a `.DS_Store`; `mount`, then `unmount` | mounted; `unmount` removed the folder and the `.DS_Store` |
| `cp` of 32 MiB, remounted read-only, compared | 8.36 s (4.0 MB/s); identical |

The notarised release (0.1.0), installed in `/Applications`:

| Test | Result |
|---|---|
| Archive, Developer ID export, `notarytool submit`, `stapler staple` | Accepted, no issues in the notary log; `codesign --verify --deep --strict` passes |
| `spctl -a -vv` on the app, and on a copy unpacked from the zip and marked as downloaded | "Notarized Developer ID" |
| `sp404drive --version` from that quarantined copy | runs |
| Replace the app, open it once, `sp404drive mount --read-only` | mounted in 0.82 s, with the extension still switched on; a 4 MB sample read in 1.05 s (3.9 MB/s) |
| `sp404drive unmount` | 3.05 s; the port free; the folder removed |

The menu-bar app (0.1.1):

| Test | Result |
|---|---|
| Switch the unit on with the app running | mounted |
| Switch it off | ejected 44 ms after the drive noticed |
| Switch it on again, open the drive in Finder, switch it off | mounted; ejected 36 ms after the drive noticed |
| Open the app with the unit on (six times, four of them straight after replacing the app) | mounted in 1–2 s; once, after a replacement, the mount stalled (see "Lessons") |
| The welcome message, "Open SP-404 Drive at login" ticked | macOS: "Login Item Added" |

Not tested yet:

- Finder: Eject from the sidebar, copies and moves by dragging, `.DS_Store` writes;
  Spotlight and Quick Look.
- A real upload failure: switching the unit off in the middle of a copy.
- Mounting with no unit attached through `mount -F`.
- The message when the extension is switched off.
- A first install on another Mac: opening the downloaded app by double-clicking it and
  switching the extension on from scratch.
- In the menu-bar app: Mount Read-Only, Eject and Quit (and its question when the eject
  fails), Install Command-Line Tool…, Open at Login across a restart, switching the unit
  off and straight back on while the old volume is still being unmounted, the forced
  unmount when a program has a file open on the drive, and the stalled-mount recovery.
- macOS before 26 (the message), Intel Macs (no build yet), `sp404drive unmount` giving up
  after 60 s, `sudo ln` into `/usr/local/bin`.

Still to do: Finder testing; an Intel or universal build (a second cargo target and
`lipo`); publishing the release.
