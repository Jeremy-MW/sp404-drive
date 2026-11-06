//! The serial link to the SP-404MKII, without polling.
//!
//! Portions of this file are adapted from SparkyMK2
//! (https://github.com/TuscanPL/SparkyMK2), `crates/sp404-device/src/files.rs` at commit
//! 3007b5d, by the SparkyMK2 contributors, licensed GPL-3.0-or-later. Modified in
//! November 2026 for sp404-drive: the request/reply logic now runs over this module's
//! own event-driven link instead of `sp404-device`'s connection. As part of sp404-drive,
//! this file is distributed under GPL-3.0 only.
//!
//! The drive runs in the background for hours, so the link waits for the device instead
//! of reading on a timer: a reader thread blocks on the port until bytes arrive, and a
//! vanished device fails the pending read at once, which is how the drive learns it was
//! unplugged. Message framing and the file API's encoding come from SparkyMK2's sans-IO
//! `sp404-proto` crate.
//!
//! The OS-specific part is a [`Port`]: Windows overlapped I/O in `windows.rs`, and on
//! macOS a `/dev/cu.*` port that waits in `select(2)` in `macos.rs`. Each provides
//! `SerialPort::open` and `find_ports`, re-exported here.

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
pub mod windows;

#[cfg(target_os = "macos")]
pub use macos::{PortInfo, SerialPort, find_ports};
#[cfg(windows)]
pub use windows::{PortInfo, SerialPort, find_ports};

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sp404_proto::fileapi::{self, DirEntry, Reply, Stat};
use sp404_proto::{Channel, Decoder, Message, ProtoError};

/// USB ids of the SP-404MKII's serial function.
pub const USB_VID: u16 = 0x0582;
pub const USB_PID: u16 = 0x02E7;
pub const BAUD: u32 = 921_600;

/// The official app waits this long after opening the port before its first message.
const SETTLE: Duration = Duration::from_millis(350);
/// Deadline for a reply.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Closing a file just written can take the device a while, as it finishes storing it.
const CLOSE_AFTER_WRITE: Duration = Duration::from_secs(20);
/// How long the device's session lasts after the last message from the host (measured at
/// 2.2-2.5 s), with a margin. See [`Link::let_session_lapse`].
const SESSION: Duration = Duration::from_secs(3);
/// Open flag: read only (newlib `O_RDONLY`).
pub const OPEN_READ_ONLY: u32 = fileapi::flags::READ_ONLY;

/// A byte stream to the device.
pub trait Port: Send + Sync + 'static {
    /// Wait until bytes arrive and return them. Waits without waking while the line is
    /// idle. `Ok(0)` may happen and means nothing; an error means the port is gone or
    /// [`Port::cancel_read`] was called.
    fn read(&self, buf: &mut [u8]) -> io::Result<usize>;
    /// Write all of `bytes`. May run while a read is pending.
    fn write_all(&self, bytes: &[u8]) -> io::Result<()>;
    /// Make a pending or future `read` fail, so the reader thread can end.
    fn cancel_read(&self);
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("device refused {0}")]
    Refused(String),
    #[error("the link to the SP-404MKII is closed")]
    Closed,
    #[error("timed out waiting for {0}")]
    Timeout(String),
    #[error("unexpected reply to {0}")]
    Unexpected(String),
    #[error("protocol: {0}")]
    Proto(#[from] ProtoError),
    #[error("serial port: {0}")]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct Link {
    name: String,
    port: Arc<dyn Port>,
    inbox: Mutex<Receiver<Message>>,
    /// Held for a whole multi-step operation; see [`Link::exclusive`].
    operation: Mutex<()>,
    closed: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    /// When the last message went to the device.
    last_sent: Mutex<Option<Instant>>,
}

impl Link {
    /// Start talking over `port`. `on_closed` runs once, on the reader thread, the moment
    /// the port fails (the device was unplugged or switched off).
    pub fn new(
        name: &str,
        port: Arc<dyn Port>,
        on_closed: impl FnOnce() + Send + 'static,
    ) -> io::Result<Self> {
        std::thread::sleep(SETTLE);
        let (tx, rx) = mpsc::channel();
        let closed = Arc::new(AtomicBool::new(false));
        let stopping = Arc::new(AtomicBool::new(false));
        let reader = {
            let (port, closed, stopping) = (port.clone(), closed.clone(), stopping.clone());
            std::thread::Builder::new()
                .name("sp404-reader".into())
                .spawn(move || read_loop(&*port, tx, &closed, &stopping, on_closed))?
        };
        Ok(Link {
            name: name.to_string(),
            port,
            inbox: Mutex::new(rx),
            operation: Mutex::new(()),
            closed,
            stopping,
            reader: Some(reader),
            last_sent: Mutex::new(None),
        })
    }

    pub fn port_name(&self) -> &str {
        &self.name
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Run `f` with no other `exclusive` caller in between. Each request is atomic, but
    /// a listing or a file transfer is many of them.
    pub fn exclusive<T>(&self, f: impl FnOnce(&Self) -> T) -> T {
        let _held = self.operation.lock().unwrap_or_else(|e| e.into_inner());
        f(self)
    }

    fn send(&self, msg: &Message) -> Result<()> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        *self.last_sent.lock().unwrap() = Some(Instant::now());
        self.port.write_all(&msg.encode()).map_err(|e| {
            if self.is_closed() {
                Error::Closed
            } else {
                Error::Io(e)
            }
        })
    }

    fn wait(
        inbox: &Receiver<Message>,
        what: &str,
        timeout: Duration,
        matches: &mut impl FnMut(&Message) -> bool,
    ) -> Result<Message> {
        let deadline = Instant::now() + timeout;
        loop {
            match inbox.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(m) if matches(&m) => return Ok(m),
                Ok(m) => log::debug!("ignored ch{:02x} message", m.channel.to_byte()),
                Err(RecvTimeoutError::Timeout) => return Err(Error::Timeout(what.to_string())),
                Err(RecvTimeoutError::Disconnected) => return Err(Error::Closed),
            }
        }
    }

    /// Send `payload` on the file channel and wait for its reply.
    fn file_call_within(&self, payload: Vec<u8>, what: &str, timeout: Duration) -> Result<Reply> {
        let inbox = self.inbox.lock().unwrap();
        // Drop anything left over (notifications, late replies).
        while inbox.try_recv().is_ok() {}
        self.send(&Message::long(Channel::File, payload))?;
        // Short messages on this channel are notifications, never replies.
        let reply = Self::wait(&inbox, what, timeout, &mut is_file_reply)?;
        Ok(fileapi::parse_reply(reply.payload())?)
    }

    fn file_call(&self, payload: Vec<u8>, what: &str) -> Result<Reply> {
        self.file_call_within(payload, what, TIMEOUT)
    }

    fn status_call(&self, payload: Vec<u8>, what: &str) -> Result<i32> {
        match self.file_call(payload, what)? {
            Reply::Status { result: -1, .. } => Err(Error::Refused(what.to_string())),
            Reply::Status { result, .. } => Ok(result),
            Reply::Data { .. } => Err(Error::Unexpected(what.to_string())),
        }
    }

    fn data_call(&self, payload: Vec<u8>, what: &str) -> Result<Option<Vec<u8>>> {
        match self.file_call(payload, what)? {
            Reply::Data { data, .. } => Ok(Some(data)),
            Reply::Status { result: -1, .. } => Ok(None),
            Reply::Status { .. } => Err(Error::Unexpected(what.to_string())),
        }
    }

    pub fn open_file(&self, path: &str, flags: u32) -> Result<i32> {
        self.status_call(fileapi::open(path, flags), &format!("open {path}"))
    }

    pub fn close_file(&self, handle: i32) -> Result<()> {
        self.status_call(fileapi::close(handle), "close").map(drop)
    }

    /// Close a file that was written to, allowing the device time to finish storing it.
    pub fn close_written(&self, handle: i32) -> Result<()> {
        match self.file_call_within(fileapi::close(handle), "close", CLOSE_AFTER_WRITE)? {
            Reply::Status { result: -1, .. } => Err(Error::Refused("close".into())),
            Reply::Status { .. } => Ok(()),
            Reply::Data { .. } => Err(Error::Unexpected("close".into())),
        }
    }

    pub fn seek(&self, handle: i32, offset: u32) -> Result<u32> {
        self.status_call(fileapi::seek(handle, offset), "seek")
            .map(|r| r as u32)
    }

    /// `None` when the path does not exist.
    pub fn stat(&self, path: &str) -> Result<Option<Stat>> {
        match self.data_call(fileapi::stat(path), &format!("stat {path}"))? {
            Some(data) => Ok(Some(Stat::parse(&data)?)),
            None => Ok(None),
        }
    }

    pub fn unlink(&self, path: &str) -> Result<()> {
        self.status_call(fileapi::unlink(path), &format!("unlink {path}"))
            .map(drop)
    }

    /// Create a directory; fails if the name is taken.
    pub fn mkdir(&self, path: &str) -> Result<()> {
        self.status_call(fileapi::mkdir(path), &format!("mkdir {path}"))
            .map(drop)
    }

    /// Remove a directory; fails unless it is empty.
    pub fn rmdir(&self, path: &str) -> Result<()> {
        self.status_call(fileapi::rmdir(path), &format!("rmdir {path}"))
            .map(drop)
    }

    /// Move or rename a file or directory within one volume.
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.status_call(fileapi::rename(from, to), &format!("rename {from}"))
            .map(drop)
    }

    /// Free space on the internal storage.
    pub fn free_kb(&self) -> Result<u32> {
        self.status_call(fileapi::free_kb(fileapi::ROOT), "free space")
            .map(|r| r as u32)
    }

    /// Read up to `count` bytes; the device answers in chunks of at most [`fileapi::CHUNK`].
    pub fn read(&self, handle: i32, count: u32) -> Result<Vec<u8>> {
        let inbox = self.inbox.lock().unwrap();
        while inbox.try_recv().is_ok() {}
        self.send(&Message::long(Channel::File, fileapi::read(handle, count)))?;
        let mut out = Vec::with_capacity(count as usize);
        loop {
            let reply = Self::wait(&inbox, "read", TIMEOUT, &mut is_file_reply)?;
            match fileapi::parse_reply(reply.payload())? {
                Reply::Data { data, last, .. } => {
                    out.extend_from_slice(&data);
                    if last {
                        return Ok(out);
                    }
                }
                Reply::Status { result: -1, .. } => return Err(Error::Refused("read".into())),
                Reply::Status { .. } => return Ok(out),
            }
        }
    }

    /// Write `data` at the current position in batches, checking the device's byte counts.
    pub fn write(&self, handle: i32, data: &[u8]) -> Result<()> {
        let chunks: Vec<&[u8]> = data.chunks(fileapi::CHUNK).collect();
        for batch in chunks.chunks(fileapi::WRITE_BATCH) {
            let expected: usize = batch.iter().map(|c| c.len()).sum();
            let inbox = self.inbox.lock().unwrap();
            while inbox.try_recv().is_ok() {}
            for (i, chunk) in batch.iter().enumerate() {
                let last = i + 1 == batch.len();
                self.send(&Message::long(
                    Channel::File,
                    fileapi::write(handle, chunk, last),
                ))?;
            }
            let reply = Self::wait(&inbox, "write", TIMEOUT, &mut is_file_reply)?;
            match fileapi::parse_reply(reply.payload())? {
                Reply::Status { result, .. } if result as usize == expected => {}
                Reply::Status { result, .. } => {
                    return Err(Error::Refused(format!(
                        "write ({result} of {expected} bytes accepted)"
                    )));
                }
                Reply::Data { .. } => return Err(Error::Unexpected("write".into())),
            }
        }
        Ok(())
    }

    /// Create or replace a file with `data`.
    pub fn write_file(&self, path: &str, data: &[u8]) -> Result<()> {
        let handle = self.open_file(path, fileapi::flags::CREATE_TRUNCATE_READ_WRITE)?;
        let written = self.write(handle, data);
        let closed = self.close_written(handle);
        written?;
        closed
    }

    /// List a directory, without `.` and `..`.
    pub fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>> {
        let dir = self.status_call(fileapi::opendir(path), &format!("opendir {path}"))?;
        let mut entries = Vec::new();
        let result = loop {
            match self.data_call(fileapi::readdir(dir), "readdir") {
                Ok(Some(data)) => {
                    let entry = DirEntry::parse(&data)?;
                    if !entry.is_dot() {
                        entries.push(entry);
                    }
                }
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        // The SD card closes the handle itself once readdir runs out, then refuses
        // closedir. The entries are in hand, so that is not a failure.
        if let Err(e) = self.status_call(fileapi::closedir(dir), "closedir") {
            log::debug!("closedir on {path}: {e}");
        }
        result.map(|()| entries)
    }

    /// Wait, sending nothing, until the device's session has lapsed; call it last, before
    /// the port closes.
    ///
    /// Every message from the host starts or extends a session of about 2.2-2.5 s. While
    /// it lasts, the device pushes a message on every pad hit; if the port closes then,
    /// with pads being hit quickly, the device freezes until the port is opened again. The
    /// reader thread keeps taking in whatever arrives meanwhile. One wait, only as long as
    /// needed; none if the last message was long enough ago.
    pub fn let_session_lapse(&self) {
        let last = *self.last_sent.lock().unwrap();
        let wait = lapse_wait(last, Instant::now());
        if !wait.is_zero() && !self.is_closed() {
            log::debug!("waiting {wait:?} for the device's session to lapse");
            std::thread::sleep(wait);
        }
    }
}

/// How much longer the session lasts after a message sent at `last`.
fn lapse_wait(last: Option<Instant>, now: Instant) -> Duration {
    last.map_or(Duration::ZERO, |t| {
        SESSION.saturating_sub(now.duration_since(t))
    })
}

impl Drop for Link {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.port.cancel_read();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn is_file_reply(m: &Message) -> bool {
    m.channel == Channel::File && !m.is_short()
}

fn read_loop(
    port: &dyn Port,
    tx: Sender<Message>,
    closed: &AtomicBool,
    stopping: &AtomicBool,
    on_closed: impl FnOnce(),
) {
    let mut decoder = Decoder::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match port.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                decoder.push(&buf[..n]);
                while let Some(m) = decoder.next_message() {
                    if tx.send(m).is_err() {
                        return;
                    }
                }
            }
            Err(e) => {
                closed.store(true, Ordering::SeqCst);
                if !stopping.load(Ordering::SeqCst) {
                    log::warn!("serial link lost: {e}");
                    on_closed();
                }
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_out_the_session_only_when_needed() {
        let now = Instant::now();
        assert_eq!(lapse_wait(None, now), Duration::ZERO);
        assert_eq!(lapse_wait(Some(now), now), SESSION);
        let a_second_ago = now - Duration::from_secs(1);
        assert_eq!(
            lapse_wait(Some(a_second_ago), now),
            SESSION - Duration::from_secs(1)
        );
        let long_ago = now - Duration::from_secs(10);
        assert_eq!(lapse_wait(Some(long_ago), now), Duration::ZERO);
    }
}
