# The SP-404MKII over USB: what we learned

What building sp404-drive taught us about the unit itself, measured on a real SP-404MKII
from Windows 11 and macOS 26. The protocol comes from
[SparkyMK2](https://github.com/TuscanPL/SparkyMK2) (commit `3007b5d`); see its
`docs/re/` for the message formats and the file API. This page records what we found on
top of that. The unit's firmware version wasn't recorded during these tests.

Where a finding is assumed or untested, it says so.

## The connection

- **It isn't USB storage.** The unit shows up as a serial port (USB `0582:02E7`) carrying
  a file API, plus a separate audio/MIDI device (`0582:0281`). A drive has to be a
  user-mode file system that turns file requests into serial messages.
- **No driver is needed.** Windows uses its own USB serial driver; macOS uses Apple's
  CDC-ACM driver, and the port appears as `/dev/cu.usbmodem*`.
- **Line settings.** 921600 baud, 8N1, no flow control, DTR and RTS asserted (the
  official app's line state). Roland's app waits 350 ms after opening the port before its
  first message, and the drive does the same. On macOS, 921600 has no termios constant;
  it's set with the `IOSSIOSPEED` ioctl.
- **The port is exclusive.** One program at a time: the drive or Roland's app, never both.
- **Unplugging or switching off ends a pending read at once.** On Windows with overlapped
  I/O, the read fails with `ERROR_OPERATION_ABORTED` (995); the drive was gone 6 ms after
  the cable was pulled. With a short-timeout read loop instead, the read failed with error
  22 ("The device does not recognize the command"). On macOS the read ends with "Device
  not configured".

## The two volumes

- **Two path roots.** In SparkyMK2's path form, `""` is the internal storage root and
  `"SD:"` the card root. `"/"` and `"/SP404REMOTE//"` list as empty, so use `""`.
- **The internal root holds more than projects:** `FCTRY` (the factory library, a 175 MB
  `FACTORY.SVD`), `ROLAND/SP-404MKII/PROJECT_01..16` with `QSPI.bin` beside them, and a
  set of `.DAT` and `DOOMOS.*` files. Writing there edits the unit's live state, and the
  unit reads some files only when a project loads.
- **Moving between the volumes** isn't possible in one request: they're separate file
  systems, so a move is a copy and a delete.
- **Capacity.** Only the internal storage reports free space. Neither volume reports its
  total size, and the card reports neither.
- **No timestamps.** The file API carries no dates.

## File handles

- **The unit won't replace a file it has open.** Opening a file for writing (`0x602`,
  create and truncate) while a read handle is open on it is refused with −1. Closing the
  read handle first fixes it. Delete and rename are assumed to behave the same; not
  tested.
- **Handles are small numbers, lowest free first** (1, 2, 3, …), so a closed handle's
  number goes to the next file opened. At least 10 files can be open at once.
- **The unit closes idle handles by itself.** See the next section. Together with the
  numbering, this means a client that keeps a handle across a pause can end up reading a
  different file through the same number. That was sp404-drive's issue #1: reads returned
  another file's contents.

## The host session

The unit keeps a "host session" alive while the computer talks to it. Any message from the
computer starts it or extends it by about 2.2–2.5 s.

**What the session affects:**

- **When it lapses, the unit closes every open file handle,** on both volumes, without a
  message. Reads and closes on a closed handle answer −1.
- **While it lasts, every pad hit makes the unit push its project-settings block** (`7D`,
  147 bytes of payload, in a 163-byte message with its 16-byte header) on channel 05,
  unasked. The block is the same for every pad, empty pads included, byte for byte: it
  says that something happened, not which pad. Which pad, and a fresh hit on the same pad,
  still come from the status reply (`7E`: byte 5 the selected pad, bytes 8–11 the sounding
  pad's frame count, which restarts on every hit). A client could use the push as a cue to
  ask for the status at once, instead of asking on a timer.

**What keeps it alive** (measured with `examples/idle_handles.rs`: a handle open on one
file, then a request every 500 ms for 6 s):

| Request every 500 ms | Handle still open? |
|---|---|
| `stat` of the same file (card) | yes |
| `stat` on the internal storage, handle on the card | yes |
| Listing of the card's root, handle on the internal storage | yes |
| `stat` of a missing file (answered −1) | yes |
| Control-channel status request (`FE 66 00`, as Roland's app sends every 500 ms) | yes |
| Nothing | open at 2.2 s, closed at 2.5 s, on both volumes |
| Nothing for 10 s, while pads were played on the unit | closed |

So the session is about traffic from the computer, on any channel. Nothing done on the unit
itself extends it. Roland's app never sees handles close, because its status request keeps
the session alive.

**The pushes, logged byte by byte:** after a set of file requests, with pads being tapped,
the pushes arrived every 60–330 ms starting 19 ms after the last reply. 23 arrived, all
within 2.4 s, then none, although the tapping went on: the session had lapsed. With a
`stat` every 500 ms keeping it alive, pads 1–16 hit in order one per second (some of them
empty) gave exactly 16 identical blocks.

## Closing the port can freeze the unit

**If the port closes while a session is alive and pads are being hit quickly** (faster than
about 300 ms apart, an estimate), the unit freezes. Its controls and power switch stop
responding. The computer still lists it, but writes to the port fail with error 22. The
likeliest cause is that the unit tries to send its pushes to a port nobody reads any more,
and blocks.

**To bring it back,** open the port and close it again, sending nothing. The screen goes
blank as the port opens, which looks like a power-off, pressed during the freeze, being
carried out.

**To avoid it,** keep the port open and reading until about 3 s after the last message,
then close it. sp404-drive does this when it unmounts.

Each of these variants opened the port, waited 350 ms, then did what's listed, with pads
being tapped quickly:

| Variant | Sent | Port closed | Froze? |
|---|---|---|---|
| Open only | nothing | after 350 ms | no |
| Files | list, open, seek, read, close | ~2 ms after the last reply | **yes** |
| Exit | MKII EXIT (no reply) | under 1 ms after sending | **yes** |
| Exit, held | MKII EXIT after 5 s | held open | no |
| Files, held | list, open, seek, read, close | held open | no |
| Files, then wait | list, open, seek, read, close | 3.6 s later, 1.2 s after the last push | no |

Any message ending a session freezes the unit if the port closes straight after it; none
does while the port stays open or once the session has lapsed. The same sequence with no
pads being played, repeated 11 times, never froze it. Pulling the cable at the wrong
moment can still freeze it; no program can prevent that.

**Open questions:** whether Roland's app can freeze the unit the same way when it
disconnects during fast pad playing; whether the pushes are always the `7D` block or
depend on what the unit is showing.

## Names

All of this was measured on the SD card. The internal storage hasn't been tested.

- **Case is ignored for ASCII letters only.** A rename whose path spelled an existing
  folder in a different case worked. `Été.txt` and `été.txt` were two different files.
- **Names are stored as raw UTF-8, not normalised.** A decomposed (NFD) name, as macOS
  often produces, is listed back decomposed. A card written from a Mac can hold both forms.
- **Some non-ASCII names are refused, with no reason given (−1).** Refused: `É.txt`,
  `ÉTÉ.txt`, `Ü.txt` (beside an existing `ü.txt`), `Cafe\u{301}3.txt`. Accepted:
  `été.txt`, `Été.txt`, `ü.txt`, `ñ.txt`, `日本.txt`, `e\u{301}x.txt`. The rule hasn't
  been found.
- **A full path can be at most 199 bytes.**
- **The unit's refusals never say why.** A full card and a name it won't take look the
  same.

## Speed

- **Directory listings are expensive.** `readdir` gives names only; each size takes a
  `stat`. Cache listings.
- **Through sp404-drive's event-driven link on Windows:** listing 149 samples took 0.3 s,
  reads ran at 3.8 MB/s and a 1 MB write took 0.27 s. Through a short-timeout read loop the
  same took 3.7 s, ran at 3.0 MB/s and took 0.65 s.
- **On macOS:** reads at 3.1–4.0 MB/s, writes at 3.7–4.0 MB/s.
- **Writes run faster than reads** (about 4.5 against 3.0 MB/s in one comparison), because
  writes are batched (15 chunks before waiting for an answer) while reads are asked for one
  at a time.
- **exFAT makes the unit slower at finding files.** A 256 GB exFAT card and a 31 GB FAT32
  card with the same contents, through the unit and in a card reader:

  | | FAT32 in reader | exFAT in reader | FAT32 in unit | exFAT in unit |
  |---|---|---|---|---|
  | `stat` per file | 0.14 ms | 0.11–0.17 ms | 2.0 ms | 4.9 ms |
  | Open + 4 KiB read, median | 18.5 ms | 14.4–15.0 ms | 55 ms | 69–72 ms |
  | Open + 4 KiB read, mean (the first open in a folder lists it) | 18.6–19.1 ms | 14.8–15.4 ms | 80 ms | 226–230 ms |
  | Sequential 1 MiB reads | 23 MB/s | 140–149 MB/s | 3.0 MB/s | 2.9–3.1 MB/s |
  | Random 4 KiB reads, median | 0.88 ms | 0.60–0.64 ms | 7.2 ms | 6.0–6.6 ms |
  | Sequential writes | 12.2–12.3 MB/s | 109–112 MB/s | 3.5–4.1 MB/s | 4.5 MB/s |
  | Create, write and close a 4 KiB file, median | 9.7 ms | 10.4–10.7 ms | 10.1–10.3 ms | 16.6–17.8 ms |
  | Delete a 4 KiB file, median | 3.9–4.0 ms | 5.0–5.4 ms | 3.0 ms | 5.9 ms |

  In the reader the exFAT card is the faster one at everything, so it isn't the card's
  hardware. Through the unit, it loses only on finding, creating and deleting files (2–3
  times slower); moving data costs the same, since the serial link caps it. So it's the
  unit's handling of exFAT, or the larger cluster size that usually goes with it.
  Reformatting the FAT32 card as exFAT would settle which.
- **The unit's own import and export run at about 10 MB/s** either way, far below what a
  fast card manages in a reader (a 661 MB project: 53–63 s to import, 65–71 s to export,
  timed by hand once on each card).

## Cards

- **The dirty flag matters on exFAT only.** With the flag set (`fsutil dirty set` in a card
  reader), an exFAT card gives "FOLDER ERROR", with every pad flashing at full brightness,
  as soon as Project is chosen in the import menu. Every time, also after power-cycling
  the unit with the card in. A FAT32 card with the flag set works normally. The unit never
  clears the flag, not even over a power cycle; a disk check on a computer (`chkdsk /f`,
  or Windows' "Scan and fix") does.
- **Windows writes to a card it sees,** even one that is only browsed: `System Volume
  Information/` (`IndexerVolumeGuid`, `WPSettings.dat`) at the root, and `$RECYCLE.BIN/`
  when files are deleted in Explorer through a card reader. Deleted files keep their space
  until the recycle bin is emptied.
- **The unit's own files at the card root** include `DEVICES.CSV`, a MIDI device list.

## Files and folders

The file formats themselves are documented in SparkyMK2's `docs/re/02-files.md` to
`04-patterns.md`. What we observed on top of them:

- **Projects can live on the card.** A folder in `EXPORT/` named `PROJECT_xx` (a number,
  the same form as the internal slots) is a project the unit can play and edit straight
  from the card, with no import or export.
- **Export folders are named `P<source project number>_<project name>/`.** SparkyMK2 gives
  `PROJECT_<project name>/`, which is out of date.
- **A `.wav` copied into `IMPORT/` over USB** shows up in the unit's IMPORT list without
  taking the card out.
- **`QSPI.bin`** (596 bytes, in `ROLAND/SP-404MKII/` beside the projects) isn't described
  by SparkyMK2. One snapshot suggests system-wide settings: 32-bit little-endian words,
  zeros at `0x000–0x00F`, small values (mostly 0 or 1) at `0x010–0x0C7`, zeros at
  `0x0C8–0x107`, two 164-byte records of the same layout at `0x108` and `0x1AC` that
  differ in only a few bytes, and `AA AA AA AA` at `0x250`, probably an end marker. That
  is a guess. Comparing copies taken before and after changing one SYSTEM setting would
  tell. Don't write it: if the unit reads it at startup, a bad write could reset its
  settings or stop it booting.

## "Unsupported File" when importing a project

Importing a project export from the card sometimes stops with "Unsupported File". The
failure is intermittent: the same unchanged exports failed on one attempt and loaded on
another, and one export stopped at a different pad on each failed attempt with identical
data. The trigger isn't known.

Ruled out so far, by exports that failed once and then loaded unchanged, and by six
imports in a row that all loaded:

- extra files in the export folder (`PADCONF.old` or `.bad` beside `PADCONF.BIN`, and
  `.SMP.old` copies in `SMPL/`);
- "ghost" pads (settings but no sample file);
- a missing `PICTURE/` folder;
- space on the unit.

Every failure so far was from a large exFAT card, where the unit finds files 2–3 times
more slowly (see "Speed"). That is a lead, not a finding.

## Still open

- Whether the unit's "closed" confirmation means the data is on the card: very likely, not
  proven by a power cut.
- Whether delete and rename are refused on an open file, as opening for writing is.
- How names behave on the internal storage.
