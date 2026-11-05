# sp404-drive

Mount a Roland SP-404MKII as a drive. Plug the unit in over USB and it appears as a drive
letter with two folders:

```
S:\
  Internal\    the device's own storage: ROLAND\SP-404MKII\PROJECT_01..16, FCTRY, ...
  SD-Card\     the card in its slot: IMPORT, EXPORT, BKUP, ...
```

Explorer, DAWs and the command line can browse, copy, create, rename and delete files on it.

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

On the way out, it saves any file still being written and takes the device off its
remote screen (the app's "MKII EXIT"). Signing out of Windows ends it without that
cleanup.

The drive logs to `%LOCALAPPDATA%\sp404drive\logs\S.log` (one file per mount point,
started afresh on each mount).

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
- **Writes are uploaded when the file is closed.** The device's only dependable write is
  "create or replace a whole file", so a file being written is held in memory and
  uploaded when the program closes it. The same happens on unmount. If the upload fails,
  the error is logged; the program that wrote the file has already closed it and isn't
  told.
- **Moving between `Internal` and `SD-Card`** is a copy followed by a delete, because they
  are separate filesystems on the device. Explorer and other tools do this automatically.
- **No dates.** The device keeps no timestamps, so every file shows the time the drive was
  mounted.
- **Capacity.** Free space comes from the internal storage. The device doesn't report its
  total size or anything for the card, so the drive reports the total as equal to the free
  space.
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
               on sp404-proto), and the Windows port with overlapped I/O (windows.rs).
src/store.rs   Store trait: what the drive needs from the device. DeviceStore implements it
               over the link; MemStore (tests) in memory.
src/vfs.rs     The filesystem, independent of the OS: path mapping, listing cache, shared
               open-file state, read-handle pool, write buffering. Unit-tested.
src/winfs.rs   Windows front end: WinFsp callbacks -> vfs::Drive.
src/host.rs    The background host on Windows: serving the drive, and how sp404drive
               starts it (through Explorer, reporting over a named pipe) and stops it
               (a named event and mutex per mount point).
src/main.rs    sp404drive.exe, the command line.
src/bin/sp404drive-host.rs
               sp404drive-host.exe, the windowless background process.
```

Everything except `winfs.rs` and `link/windows.rs` builds on macOS. A Mac version needs:

1. a `src/fuse.rs` that implements `fuser::Filesystem` by calling the same `vfs::Drive`
   methods `winfs.rs` uses (`lookup`, `list`, `open`, `create`, `read`, `write`,
   `set_size`, `flush`, `check_delete`/`delete`, `rename`, `release`, `space`). FUSE works
   with inode numbers, so that file also keeps an inode-to-`VPath` table;
2. `serve`, `listen_for_unmount` and `unmount` in `main.rs` for macOS. Unmounting is
   `umount /Volumes/SP-404MKII` or Finder's Eject, and the FUSE session then ends by
   itself;
3. a `link/macos.rs` implementing `Port` for `/dev/cu.usbmodem*`: a blocking `read(2)`
   on the descriptor, which fails when the device is unplugged, plus `write(2)`;
   `cancel_read` can wake the reader through a self-pipe and `poll(2)` with no timeout.
   Port discovery goes through IOKit (USB 0582:02E7);
4. [macFUSE](https://osxfuse.github.io) or [FUSE-T](https://www.fuse-t.org) installed on
   the Mac. FUSE-T needs no kernel extension.

## What comes from SparkyMK2

We use SparkyMK2 ("Sparky") in four ways: one crate is compiled in, one file was ported, the licence was copied, and some docs and code served only as reference. Everything comes from commit `3007b5d`.

**1. Compiled in: the `sp404-proto` crate.** It's the only Sparky crate in `Cargo.toml`, as a git dependency pinned to that commit. Every call to it is in `src/link/mod.rs`:

| Sparky file | What we use |
|---|---|
| `crates/sp404-proto/src/frame.rs` | `Message`, `Channel`, `Decoder`: building and splitting the serial messages |
| `crates/sp404-proto/src/fileapi.rs` | the file requests (`open`, `close`, `read`, `write`, `seek`, `stat`, `opendir`, `readdir`, `closedir`, `mkdir`, `rmdir`, `unlink`, `rename`, `free_kb`), `parse_reply`, `Reply`, `Stat`, `DirEntry`, `CHUNK`, `WRITE_BATCH`, `ROOT`, open flags, and the path mapping that `Internal` and `SD-Card` rely on |
| `crates/sp404-proto/src/septet.rs` | used inside `fileapi.rs` to encode numbers; we don't call it directly |
| `crates/sp404-proto/src/control.rs` | `mkii_exit()` only, sent when unmounting |
| `crates/sp404-proto/src/lib.rs` | `ProtoError` |

The crate's other files (`pad.rs`, `params.rs`, `settings.rs`) are compiled because they're part of the crate, but nothing calls them.

**2. Ported: `crates/sp404-device/src/files.rs`, plus a little of that crate's `lib.rs`.** Our `src/link/mod.rs` reimplements its request-and-reply logic:
- write batching and checking the device's byte counts;
- the multi-part read;
- the 20-second wait when closing a written file;
- tolerating the SD card's refusal to close a finished directory listing;
- the 350 ms pause after opening the port.

`sp404-device` itself was a dependency in the first commit, but not any more. Its 2 ms polling loop is why we replaced its connection layer.

**3. Copied: `LICENSE`**, the GPL-3.0 text.

**4. Reference only, not in the repo:**
- `docs/re/01-transport.md` and `00-findings.md` shaped the design: the two volumes, `opendir` not reporting missing folders, the file-API commands.
- Sparky's CLI (`crates/sp404-cli`) was built once at the start of the project to check the link to the unit worked. It isn't used now.
- Sparky's `sp404-formats` crate (the SMP, `PADCONF.BIN` and pattern parsers) isn't used at all, because the drive only moves files around and never interprets them.

## License

GPL-3.0-or-later (see `LICENSE`). It uses SparkyMK2's GPL-3.0 `sp404-proto` crate, and
`src/link/mod.rs` follows its `sp404-device` file API code.
