//! The SP-404MKII's serial port on macOS: `/dev/cu.usbmodem*`, created by Apple's own USB
//! CDC-ACM driver (`AppleUSBACMData`). No Roland driver is involved.
//!
//! The port is opened non-blocking, and every wait happens in `select(2)`: the reader
//! waits on the port and on a wake-up pipe with no timeout, so it sleeps until bytes
//! arrive, the device goes away, or [`Port::cancel_read`] writes to the pipe. A write
//! that finds the output buffer full waits for room, with a deadline. (`poll(2)` would be
//! the obvious call, but on macOS it doesn't support device files.)

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{BAUD, Port, USB_PID, USB_VID};

/// Write deadline: how long a write may wait for room in the output buffer, as on Windows.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// `IOSSIOSPEED` from `<IOKit/serial/ioss.h>`, `_IOW('T', 2, speed_t)`: sets a speed that
/// termios has no constant for, such as 921600.
const IOSSIOSPEED: libc::c_ulong = 0x8008_5402;

fn check(r: libc::c_int) -> io::Result<libc::c_int> {
    if r == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// Mark `fd` close-on-exec and non-blocking.
fn set_flags(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor we own.
    unsafe {
        let fl = check(libc::fcntl(fd, libc::F_GETFL))?;
        check(libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK))?;
        let fd_fl = check(libc::fcntl(fd, libc::F_GETFD))?;
        check(libc::fcntl(fd, libc::F_SETFD, fd_fl | libc::FD_CLOEXEC))?;
    }
    Ok(())
}

pub struct SerialPort {
    fd: OwnedFd,
    /// Readable once [`Port::cancel_read`] has been called; never drained, so every later
    /// read fails too.
    wake_read: OwnedFd,
    wake_write: OwnedFd,
    /// Keeps writes whole.
    writing: Mutex<()>,
}

impl SerialPort {
    /// Open `path` (`/dev/cu.usbmodem1101`), raw, 8N1, at [`BAUD`], with DTR and RTS
    /// asserted (the official app's line state) and no flow control.
    pub fn open(path: &str) -> io::Result<Self> {
        let c_path =
            CString::new(path).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // O_NONBLOCK also keeps open() from waiting for a carrier.
        // SAFETY: open(2) with a valid C string; the descriptor is owned at once.
        let fd = unsafe {
            let raw = check(libc::open(
                c_path.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            ))?;
            OwnedFd::from_raw_fd(raw)
        };
        let f = fd.as_raw_fd();
        // SAFETY: termios and ioctl calls on the descriptor just opened, with arguments
        // of the sizes the calls expect.
        unsafe {
            // Nobody else may open the port while the drive holds it, as on Windows.
            check(libc::ioctl(f, libc::TIOCEXCL as libc::c_ulong))?;
            let mut t: libc::termios = std::mem::zeroed();
            check(libc::tcgetattr(f, &mut t))?;
            libc::cfmakeraw(&mut t);
            t.c_cflag &= !(libc::CSIZE | libc::PARENB | libc::CSTOPB | libc::CRTSCTS);
            t.c_cflag |= libc::CS8 | libc::CLOCAL | libc::CREAD;
            t.c_cc[libc::VMIN] = 1;
            t.c_cc[libc::VTIME] = 0;
            check(libc::tcsetattr(f, libc::TCSANOW, &t))?;
            let speed: libc::speed_t = BAUD as libc::speed_t;
            check(libc::ioctl(f, IOSSIOSPEED, &speed))?;
            let lines: libc::c_int = libc::TIOCM_DTR | libc::TIOCM_RTS;
            check(libc::ioctl(f, libc::TIOCMBIS as libc::c_ulong, &lines))?;
            check(libc::tcflush(f, libc::TCIFLUSH))?;
        }
        let mut pipe = [0 as RawFd; 2];
        // SAFETY: pipe(2) fills both descriptors, owned at once.
        let (wake_read, wake_write) = unsafe {
            check(libc::pipe(pipe.as_mut_ptr()))?;
            (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1]))
        };
        set_flags(wake_read.as_raw_fd())?;
        set_flags(wake_write.as_raw_fd())?;
        Ok(SerialPort {
            fd,
            wake_read,
            wake_write,
            writing: Mutex::new(()),
        })
    }

    /// Wait until the port can take more bytes, or `deadline` passes.
    fn wait_writable(&self, deadline: Instant) -> io::Result<()> {
        let f = self.fd.as_raw_fd();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let mut tv = libc::timeval {
                tv_sec: left.as_secs() as libc::time_t,
                tv_usec: left.subsec_micros() as libc::suseconds_t,
            };
            // SAFETY: a zeroed fd_set filled with FD_SET, and a select(2) over it.
            let ready = unsafe {
                let mut set: libc::fd_set = std::mem::zeroed();
                libc::FD_ZERO(&mut set);
                libc::FD_SET(f, &mut set);
                libc::select(
                    f + 1,
                    std::ptr::null_mut(),
                    &mut set,
                    std::ptr::null_mut(),
                    &mut tv,
                )
            };
            match ready {
                -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
                -1 => return Err(io::Error::last_os_error()),
                0 => return Err(io::ErrorKind::TimedOut.into()),
                _ => return Ok(()),
            }
        }
    }
}

impl Port for SerialPort {
    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let (port, wake) = (self.fd.as_raw_fd(), self.wake_read.as_raw_fd());
        let (port_ready, woken) = loop {
            // SAFETY: zeroed fd_sets filled with FD_SET, and a select(2) with no timeout.
            unsafe {
                let mut set: libc::fd_set = std::mem::zeroed();
                libc::FD_ZERO(&mut set);
                libc::FD_SET(port, &mut set);
                libc::FD_SET(wake, &mut set);
                let n = libc::select(
                    port.max(wake) + 1,
                    &mut set,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
                if n == -1 {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(e);
                }
                break (libc::FD_ISSET(port, &set), libc::FD_ISSET(wake, &set));
            }
        };
        if woken {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "read cancelled"));
        }
        if !port_ready {
            return Ok(0);
        }
        // SAFETY: reading into `buf`, within its length.
        let n = unsafe { libc::read(port, buf.as_mut_ptr().cast(), buf.len()) };
        match n {
            // In raw mode with VMIN 1, an empty read means the line hung up: the device
            // was unplugged or switched off.
            0 => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the serial port closed",
            )),
            -1 => {
                let e = io::Error::last_os_error();
                match e.kind() {
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(0),
                    _ => Err(e),
                }
            }
            n => Ok(n as usize),
        }
    }

    fn write_all(&self, mut bytes: &[u8]) -> io::Result<()> {
        let _whole = self.writing.lock().unwrap_or_else(|e| e.into_inner());
        let f = self.fd.as_raw_fd();
        let mut deadline = Instant::now() + WRITE_TIMEOUT;
        while !bytes.is_empty() {
            // SAFETY: writing from `bytes`, within its length.
            let n = unsafe { libc::write(f, bytes.as_ptr().cast(), bytes.len()) };
            if n > 0 {
                bytes = &bytes[n as usize..];
                deadline = Instant::now() + WRITE_TIMEOUT;
                continue;
            }
            if n == 0 {
                self.wait_writable(deadline)?;
                continue;
            }
            let e = io::Error::last_os_error();
            match e.kind() {
                io::ErrorKind::Interrupted => {}
                io::ErrorKind::WouldBlock => self.wait_writable(deadline)?,
                _ => return Err(e),
            }
        }
        Ok(())
    }

    fn cancel_read(&self) {
        // SAFETY: a one-byte write to our own pipe. If the pipe is full, it's readable
        // already.
        let _ = unsafe { libc::write(self.wake_write.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
    }
}

#[derive(Debug, Clone)]
pub struct PortInfo {
    /// `/dev/cu.usbmodem1101`. The number depends on the USB socket the unit is in.
    pub name: String,
    /// How the system identifies the device: `USB 0582:02E7`, plus its serial number if it
    /// reports one.
    pub instance_id: String,
}

/// Serial ports that belong to an SP-404MKII. Only the `cu.` ("callout") device is
/// listed: its `tty.` twin would wait for a carrier on open.
pub fn find_ports() -> io::Result<Vec<PortInfo>> {
    use serialport::SerialPortType;
    let ports = serialport::available_ports().map_err(io::Error::other)?;
    Ok(ports
        .into_iter()
        .filter_map(|p| match p.port_type {
            SerialPortType::UsbPort(usb)
                if usb.vid == USB_VID
                    && usb.pid == USB_PID
                    && p.port_name.starts_with("/dev/cu.") =>
            {
                let serial = usb
                    .serial_number
                    .map(|s| format!(" serial {s}"))
                    .unwrap_or_default();
                Some(PortInfo {
                    name: p.port_name,
                    instance_id: format!("USB {USB_VID:04X}:{USB_PID:04X}{serial}"),
                })
            }
            _ => None,
        })
        .collect())
}
