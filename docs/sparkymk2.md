# What comes from SparkyMK2

sp404-drive is built on [SparkyMK2](https://github.com/TuscanPL/SparkyMK2) ("Sparky"), a
clean-room, open-source client for the SP-404MKII. We use it in four ways: one crate is
compiled in, one file was ported, the licence was copied, and some docs and code served
only as reference. Everything comes from commit `3007b5d`.

**1. Compiled in: the `sp404-proto` crate.** It's the only Sparky crate in `Cargo.toml`, as
a git dependency pinned to that commit. Every call to it is in `src/link/mod.rs`:

| Sparky file | What we use |
|---|---|
| `crates/sp404-proto/src/frame.rs` | `Message`, `Channel`, `Decoder`: building and splitting the serial messages |
| `crates/sp404-proto/src/fileapi.rs` | the file requests (`open`, `close`, `read`, `write`, `seek`, `stat`, `opendir`, `readdir`, `closedir`, `mkdir`, `rmdir`, `unlink`, `rename`, `free_kb`), `parse_reply`, `Reply`, `Stat`, `DirEntry`, `CHUNK`, `WRITE_BATCH`, `ROOT`, open flags, and the path mapping that `Internal` and `SD-Card` rely on |
| `crates/sp404-proto/src/septet.rs` | used inside `fileapi.rs` to encode numbers; we don't call it directly |
| `crates/sp404-proto/src/lib.rs` | `ProtoError` |

The crate's other files (`control.rs`, `pad.rs`, `params.rs`, `settings.rs`) are compiled
because they're part of the crate, but nothing calls them.

**2. Ported: `crates/sp404-device/src/files.rs`, plus a little of that crate's `lib.rs`.**
Our `src/link/mod.rs` reimplements its request-and-reply logic:

- write batching and checking the device's byte counts;
- the multi-part read;
- the 20-second wait when closing a written file;
- tolerating the SD card's refusal to close a finished directory listing;
- the 350 ms pause after opening the port.

`sp404-device` itself was a dependency in the first commit, but not any more. Its
connection reads the port in a short-timeout loop, a sensible fit for an interactive app;
a drive that sits in the background for hours needed a link that waits for the device
without waking up, so we wrote our own connection layer and kept the protocol code.

**3. Copied: `LICENSE`**, the GPL-3.0 text.

**4. Reference only, not in the repo:**

- `docs/re/01-transport.md` and `00-findings.md` shaped the design: the two volumes,
  `opendir` not reporting missing folders, the file-API commands.
- Sparky's CLI (`crates/sp404-cli`) was built once at the start of the project to check
  the link to the unit worked. It isn't used now.
- Sparky's `sp404-formats` crate (the SMP, `PADCONF.BIN` and pattern parsers) isn't used at
  all, because the drive only moves files around and never interprets them.

What we found on top of Sparky's work is in [device.md](device.md).
