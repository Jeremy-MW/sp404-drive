# sp404-drive

Use a Roland SP-404MKII as a drive on Windows or macOS. Switch the unit on with its USB
cable connected, and it appears on your computer with two folders:

- **Internal**: the unit's own storage (your projects, the factory library)
- **SD-Card**: the card in its slot (IMPORT, EXPORT and the rest)

Copy, rename and delete files with File Explorer, Finder, your DAW or anything else, as
with any drive.

> Not affiliated with or endorsed by Roland Corporation. "SP-404MKII" and "Roland" are
> trademarks of Roland Corporation.

> **Use at your own risk.** This software is provided as is, without warranty of any
> kind, and you use it at your own risk. The software authors cannot be held responsible
> for any loss of or damage to data, however caused. The software authors always
> recommend taking regular backups of your work (projects, samples and everything else on
> the unit and its card), so nothing gets lost or damaged.

## Windows

### What you need

Windows 10 or 11, 64-bit. Nothing else: the installer includes everything.

### Install

1. Download `SP-404-Drive-<version>-windows-x64-setup.exe` from the
   [latest release](https://github.com/Jeremy-MW/sp404-drive/releases/latest) and run it.
   It asks for administrator rights once.
2. When it's finished, SP-404 Drive starts and says it's in the notification area. Leave
   **Open SP-404 Drive at sign-in** ticked: the drive only appears by itself while the app
   is running.
3. Windows 11 hides new icons under the **^** on the taskbar. To keep SP-404 Drive in view,
   click **Taskbar Settings** in the welcome message and switch it on under "Other system
   tray icons".

To update, run the new installer. To uninstall: Settings > Apps > Installed apps >
SP-404 Drive. The installer may also have added WinFsp, which the drive runs on; it has its
own entry there, and other programs may use it.

### Use

Switch the SP-404MKII on and it appears as drive **S:**. Switch it off and it goes.

The icon in the notification area is a ring when nothing is mounted (faded when no unit is
connected) and a solid disc when the drive is mounted. Click it for the menu:

| Menu item | What it does |
|---|---|
| (the first line) | what's happening now |
| Mount | show the unit as S: and open it |
| Mount Read-Only | the same, but nothing can be changed |
| Eject | remove the drive safely |
| Open in File Explorer | open S: |
| Mount Automatically When Switched On | on by default; switch it off to mount only from the menu |
| Open at Sign-in | start SP-404 Drive when you sign in |
| About SP-404MkII Drive | version and website |
| Quit | ejects first, then quits |

## macOS

### What you need

macOS 26 or later, on a Mac with Apple silicon.

### Install

1. Download `SP-404-Drive-<version>-macos-arm64.zip` from the
   [latest release](https://github.com/Jeremy-MW/sp404-drive/releases/latest) and open it.
2. Move **SP-404 Drive** to Applications, and open it. It goes in the menu bar.
3. Leave **Open SP-404 Drive at login** ticked: the drive only appears and disappears by
   itself while the app is running.
4. Switch on its extension, once. Click **Open System Settings** in the welcome message
   (or go to System Settings > General > Login Items & Extensions > Extensions), choose
   **By Category**, then **File System Extensions**, and switch on **SP-404MKII**.

To update: choose **Eject and Quit** in the menu, replace SP-404 Drive in Applications
with the new one, and open it.

### Use

Switch the SP-404MKII on and it appears in Finder's sidebar, under Locations, as
**SP-404MKII**. Switch it off and it's ejected. If you remove it from the sidebar, it stays
removed; it's still under **Go > Computer**, and you can drag it back.

Sometimes macOS's file system service gets stuck on an earlier mount, for example just
after an update. SP-404 Drive then asks for your password, restarts the service and mounts.

The menu-bar icon is a ring when nothing is mounted (dimmed when no unit is connected) and
a solid disc when the drive is mounted. Click it for the menu:

| Menu item | What it does |
|---|---|
| (the first line) | what's happening now |
| Mount | show the unit in Finder |
| Mount Read-Only | the same, but nothing can be changed |
| Eject | remove the drive safely |
| Show in Finder | open the drive |
| Mount Automatically When Switched On | on by default; switch it off to mount only from the menu |
| Open at Login | start SP-404 Drive when you log in |
| Install Command-Line Tool… | for using `sp404drive` in Terminal (optional) |
| File System Extension Settings… | where the extension is switched on |
| About SP-404MkII Drive | version and website |
| Quit | ejects first, then quits |

## Using Roland's SP-404MKII app

The drive and Roland's app can't use the unit at the same time. To switch:

1. **Eject** in SP-404 Drive's menu.
2. Use Roland's app.
3. Quit Roland's app, then choose **Mount**.

After an Eject, the unit stays unmounted until you choose Mount or switch it off and on.
To use Roland's app every time you switch on, turn off **Mount Automatically When Switched
On**.

## Before you switch off or unplug

- **Let copies finish.** A file is saved to the unit when the program writing it is done
  with it: when the copy dialog closes, or when your DAW finishes saving.
- **To be sure, Eject first.** Eject only finishes once everything is saved. It can take up
  to 3 seconds after you last used the drive.
- **Don't pull the cable while playing pads quickly** within 3 seconds of using the drive:
  the unit can freeze. If it does, connect it, then Mount and Eject, to bring it back.
- If the unit loses power in the middle of saving a file, that file is left incomplete. On
  Windows, the program that saved it isn't told.

## Good to know

- **Internal is your live projects.** Changing files there changes the projects on the
  unit. Use **Mount Read-Only** if you only want to look or copy files off.
- **Speed.** About 4 MB/s, as fast as the unit's USB link allows: a 160 MB sample takes
  about 40 seconds. Big folders take a moment to open. Cards formatted exFAT (most cards
  over 32 GB) are slower at this than FAT32 cards.
- **Moving files between Internal and SD-Card** copies them, then deletes the originals.
- **No dates.** The unit doesn't keep them, so every file shows the time the drive was
  mounted.
- **Free space** shown is the unit's internal storage. The card's free space isn't
  available.
- **Names.** Upper and lower case count as the same letter, except for accented letters
  (`été` and `Été` are two names). The unit refuses some names with accents (`É.txt`, for
  example), and very long paths; you'll see an error for those.
- **On a Mac,** the hidden files macOS makes (`.DS_Store`, `._` files) are never written to
  the unit.

## Command line (optional)

`sp404drive` does the same as the menu, from a terminal. On Windows the installer sets it
up; on a Mac, choose **Install Command-Line Tool…** in the menu.

```
sp404drive ports               # list connected units
sp404drive mount               # mount the unit
sp404drive mount --read-only   # mount it, refusing all changes
sp404drive unmount             # eject it, and wait until it's done
```

Other options: `--port <port>` to choose a unit, `--cache-secs N` to change how long a
folder listing is reused (5 seconds by default).

**Logs:** on Windows, `%LOCALAPPDATA%\sp404drive\logs\`. On a Mac, see "Logging" in
[docs/macos.md](docs/macos.md#logging).

## More

- [docs/device.md](docs/device.md): what we learned about the SP-404MKII over USB
- [docs/windows.md](docs/windows.md) and [docs/macos.md](docs/macos.md): how each version
  works, and what has been tested
- [docs/building.md](docs/building.md): building from source, the code, contributing
- [docs/sparkymk2.md](docs/sparkymk2.md): exactly what comes from SparkyMK2

## Credits

sp404-drive talks to the unit using the protocol mapped by
[SparkyMK2](https://github.com/TuscanPL/SparkyMK2), and uses its `sp404-proto` crate. Like
SparkyMK2, it contains no Roland code, firmware or assets, and it is largely AI-written:
an AI assistant (Claude) wrote most of the code and documentation, working with the
author, and it was tested on a real SP-404MKII.

## License

Copyright © 2026 polecatplugins.com.

GNU General Public License, version 3 only (see `LICENSE`), including its Disclaimer of
Warranty and Limitation of Liability (sections 15 and 16).
