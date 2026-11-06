# The Windows version: how it works

How sp404-drive works on Windows, what we learned about Windows on the way, and what has
been tested. For using it, see the [README](../README.md). For what we learned about the
unit itself, see [device.md](device.md).

## The pieces

Three programs, installed in the same folder:

- `sp404drive.exe`: the command (`ports`, `mount`, `unmount`).
- `sp404drive-host.exe`: the drive itself. It's built as a Windows GUI program that creates
  no window, so it can never have a window or a console, however it's started.
- `sp404drive-tray.exe`: SP-404 Drive, the notification-area app.

The drive is a [WinFsp](https://winfsp.dev) file system. WinFsp gives a real drive letter
that every program can use.

## Mounting and unmounting

- **`mount` always runs the drive in the background.** It asks Explorer to start
  `sp404drive-host.exe`, as if from the Run box (`IShellWindows` → desktop shell view →
  `IShellDispatch2::ShellExecute`), then waits for the host to report through a named pipe
  that the drive is mounted, or why it isn't, with a deadline. Because Explorer starts it,
  the drive doesn't depend on whatever ran `mount`: closing that terminal or IDE, or
  pressing Ctrl+C, leaves the drive mounted. `mount` needs Explorer's desktop to be
  running, and says so if it isn't. Mounting twice on the same letter is refused.
- **Unmounting.** A named event per mount point asks the host to stop; a named mutex that
  the host owns for life tells waiters when it has gone (released or abandoned), without
  polling. `unmount` returns once the host has saved everything and let go of the port.
- **On the way out**, the drive uploads any file still being written, then sends nothing
  more and keeps reading until 3 s after its last message to the unit before closing the
  port (no wait if that was longer ago). That avoids the freeze described in
  [device.md](device.md#closing-the-port-can-freeze-the-unit). It doesn't close its pooled
  handles or send MKII EXIT at unmount: EXIT leaves the remote screen that Roland's app's
  connection sets up, which the drive never enters, and it only extended the session.
- **Unplugging or switching the unit off** fails the read that's always waiting on the
  port, at once, and the drive unmounts itself. Nothing polls.
- **Signing out of Windows** ends the drive without that cleanup.
- **Logs** go to `%LOCALAPPDATA%\sp404drive\logs\<letter>.log`, one file per mount point,
  started afresh on each mount. `-v` and `-vv` add debug and trace lines.

| Option | Status |
|---|---|
| `sp404drive mount` (always in the background) / `unmount` | implemented |
| Unmount on unplug (event-driven) | implemented |
| Mount on an empty NTFS folder instead of a letter (`sp404drive mount C:\SP404`) | implemented (WinFsp supports it); not tested |
| Tray icon, mount on switch-on, open at sign-in | implemented |
| "Map network drive" / `net use S: \\sp404\mk2`, unmounting with Explorer's *Disconnect* (registers the binary with the WinFsp Launcher and mounts with a UNC prefix) | not yet |

## The serial link

`src/link/windows.rs` opens the port with `FILE_FLAG_OVERLAPPED`, so a pending `ReadFile`
doesn't block writes, and sets `ReadIntervalTimeout = ReadTotalTimeoutMultiplier =
MAXDWORD`, `ReadTotalTimeoutConstant = MAXDWORD-1`, so a read returns as soon as any byte
arrives. The reader waits indefinitely: while idle, the drive uses no CPU and none of its
threads wake up. (Windows has no unbounded form of this timeout; the idle reader wakes at
most every 49.7 days.) Each write has a 5 s deadline.

Serial port names map to devices through SetupAPI (`GUID_DEVINTERFACE_COMPORT`,
`PortName` in the device key); the instance ID carries the USB vendor and product ids.
That's how `ports` finds the unit.

## The file system

- **Writes are uploaded when the file is closed.** The unit's only dependable write is
  "create or replace a whole file", so a file being written is held in memory and
  uploaded when the program closes it, or at unmount.
- **WinFsp's cleanup can't return an error,** so a failed upload can't be reported to the
  program that wrote the file: it has already closed it. The failure is logged, and
  `unmount` saves and confirms.
- **State is shared per file, not per handle.** Two handles on the same file see the same
  size and contents. Open files are keyed by the drive path, not the device path: the root
  and `Internal` both map to device path `""`.
- **Open files after a pause.** The unit closes every open handle after about 2 s of
  silence and reuses their numbers (see [device.md](device.md#file-handles)). The drive
  keeps up to 8 files open between reads and reopens them as needed after a pause, at a
  few milliseconds per file.
- **Listings are cached** for 5 s (`--cache-secs`), since sizes cost one `stat` per file.
- **Names** are looked up ignoring case, and each name is resolved to the unit's own
  spelling.
- **Capacity.** The drive reports the internal storage's published 16 GB (14.9 GB in
  Windows' binary units) and the free space the unit reports.
- **Drive letters are per logon session.** One made by an elevated process is invisible to
  the normal Explorer. Since Explorer starts the host, it runs unelevated even when
  `mount` is run elevated, so the letter should be visible either way (not tested).
- A WinFsp drive has no "Safely Remove" or "Eject" entry; unmounting is the equivalent.

## The tray app

- **One copy per user.** Started again, it opens the running copy's menu instead.
- **No polling.** It registers for serial-port device notifications (`WM_DEVICECHANGE`,
  `GUID_DEVINTERFACE_COMPORT`) and looks at the ports only when one for USB `0582:02E7`
  arrives or goes. The drive's process posts it a message once mounted, so a drive mounted
  with `sp404drive mount` shows at once, and a thread waits on the drive's running mutex,
  which wakes it when the drive ends (ejected, unit switched off, or the process gone).
  Mounting and ejecting each run on their own thread, each a single wait with a deadline.
- **Same code as the command.** Mount and Eject call what `sp404drive mount` and `unmount`
  call (`host::start` and `host::unmount`), so they get the same checks and messages.
- **Read-only** shows in the menu because a read-only drive holds a named event
  (`Local\sp404drive-read-only-S`) while it's mounted.
- **Settings** are under `HKCU\Software\Polecat Plugins\SP-404 Drive`; Open at Sign-in is a
  value in the user's `Run` key (switching it on in the menu also clears a switch-off made
  in Settings > Apps > Startup).
- **Sent messages re-enter the window procedure** while the thread waits in a shell call
  (`Shell_NotifyIconW` and the like). With the state in a `RefCell`, a device change
  arriving then would be dropped, or panic. So handlers of sent messages only post a
  message to act on; posted messages are never delivered inside such a call.
- **For the installer**, `sp404drive-tray.exe --quit` asks the running copy to eject and
  quit and waits until it has (at most 90 s); `--add-to-path` and `--remove-from-path`
  change the system PATH, keeping its type (`REG_EXPAND_SZ`) and every other entry, then
  broadcast `WM_SETTINGCHANGE`. `--write-icon <file>` writes the app icon.
- **Icons are drawn in code** (`src/bin/sp404drive-tray/logo.rs`) at the notification
  area's size and DPI. `build.rs` draws the app icon (16 to 256 px) with the same code and
  embeds it, with version information, in all three programs, plus the tray app's manifest
  (common controls 6 for its dialogs, per-monitor DPI awareness).

## The installer

- **"Nothing else to install" on Windows means carrying WinFsp.** Windows has no built-in
  way for a program to make a real drive letter: WebDAV caps files at 50 MB without an
  administrator's registry change, the NFS client isn't in Home editions, and Cloud Files
  gives a sync folder, not a drive. So the installer runs WinFsp's own signed installer
  silently when WinFsp is missing or older (`GetDLLVersion` on `bin\winfsp-x64.dll`, found
  through `InstallDir` in the registry's 32-bit view), and leaves it on uninstall, since
  other programs may use it.
- **The C runtime is a hidden dependency.** Rust's MSVC builds load `vcruntime140.dll` by
  default. `+crt-static` in `.cargo/config.toml` removes it; `dumpbin /dependents` shows
  only Windows' own DLLs, plus WinFsp's for the host.
- **NSIS strings stop at 1024 characters,** shorter than many PATHs, so an NSIS PATH edit
  can cut the PATH short. The tray app's `--add-to-path` does it in Rust instead.
- **An elevated installer must not start the app elevated.** Its drive letter would be
  invisible to the user's Explorer, and the drive's messages to it would be blocked.
  `Exec explorer.exe "<app>"` starts it unelevated, as Explorer's child.
- **Windows 11 hides new notification icons** under the ^, and apps can't pin themselves.
  The welcome message says how and opens Taskbar Settings.
- **Signing,** when it comes, needs a two-pass build for the uninstaller (generate
  `Uninstall.exe`, sign it, then embed it), since NSIS otherwise writes it on the user's
  PC.

## Lessons about Windows

- **WinFsp's DLL is delay-loaded.** Load it by full path from its registry `InstallDir`
  before first use, rather than enabling features that need the developer headers. The
  runtime installer doesn't include headers, but the `winfsp-sys` crate vendors them; it
  needs libclang for bindgen.
- **Match the `windows` crate version to `winfsp`'s** (0.61) so `NTSTATUS` conversions
  apply.
- **Short timeouts are coarser than they look.** Windows rounds them up to the timer tick
  (about 15.6 ms by default), so "2 ms" doesn't mean 500 wake-ups a second.
- **`CM_Register_Notification` works but is slow for unplug detection:** the
  device-removal notification arrived 2.5 s after the serial read had already failed.
- **Job objects decide whether a background process survives.** Programs started from a
  terminal or an IDE can sit in a job that refuses breakaway further up the chain.
  `QueryInformationJobObject` needs the exact structure size (144 bytes for the extended
  limits on x64), or it fails with error 24 and the flags are garbage.
- **WMI isn't a clean way out.** `Win32_Process.Create` puts the new process in WMI's own
  job, whose host gets recycled.
- **Explorer is.** Launching through Explorer's desktop shell view makes the process
  Explorer's child. Ask the view's background object for `IDispatch` first, then
  `IShellFolderViewDual`; asking for the latter directly gives `E_NOINTERFACE`.
- **Closing a console ends every program attached to it,** abruptly. Ctrl+C handlers don't
  cover the window's close button.
- **`Start-Process -Wait` waits for the whole process tree,** so it hangs on any background
  child. Don't use it to test detaching.
- **PATH:** keep it `REG_EXPAND_SZ` (`SetEnvironmentVariable` converts it to `REG_SZ`),
  broadcast `WM_SETTINGCHANGE`, and remember that shells already running keep the old PATH.
- **A folder can't be deleted while a shell's working directory is inside it.**
- **Small Win32 traps:** `TASKDIALOGCONFIG` is packed, so its icon union must be set whole;
  cargo rejects a build script's `rustc-link-arg-bin` for a binary that doesn't exist yet;
  a GUI program's helper options (`--quit`, `--add-to-path`) run from the installer
  without a console flashing up.

## Tested

The tray app and installer, on 2026-11-06, on Windows 11 with WinFsp 2.1.25156 already
installed, over an earlier hand-copied build that was removed first:

| Test | Result |
|---|---|
| The installer, run by hand (administrator prompt, every page) | exit code 0; the three programs, `Uninstall.exe`, `LICENSE.txt` and `THIRD-PARTY-NOTICES.txt` in `C:\Program Files\Polecat Plugins\SP-404 Drive`; that folder on the system PATH, still `REG_EXPAND_SZ`; SP-404 Drive 0.1.1 in Installed apps; the Start menu shortcut |
| Start SP-404 Drive from the finish page | running unelevated, as Explorer's child |
| The welcome message, "Open SP-404 Drive at sign-in" left ticked | the Run key holds the installed path |
| Switch the unit on, then use the drive from the menu | mounted on S:, working |
| `sp404drive-tray.exe --quit` with no copy running | returns at once (0.03 s), exit code 0 |

The drive itself, from 2026-11-04 to 2026-11-06: browsing both volumes, reading and writing
files, renaming and deleting, unmounting, unplugging while mounted, the issue #1 fix on a
FAT32 and an exFAT card, and unmounting during rapid pad tapping (five runs, no freeze;
unmount took 3.01 s straight after browsing and 0.06 s after 10 s of quiet).

Not tested yet: installing on a PC without WinFsp (or with an older one), so the bundled
WinFsp installer hasn't run; updating over a running copy with the drive mounted;
uninstalling; SmartScreen on a downloaded copy; a fresh user account; Open at Sign-in
across a restart; a taskbar colour change and a restart of Explorer while running; a
second monitor at another DPI; mounting on a folder; running `mount` elevated.
