//! Mount a Roland SP-404MKII as a drive. The drive's root holds two folders: `Internal`
//! (the device's own storage: projects, samples, the factory library) and `SD-Card` (the
//! card in its slot: IMPORT, EXPORT, backups).
//!
//! - [`link`]: the serial link to the device, event-driven (no polling), using
//!   SparkyMK2's protocol crate.
//! - [`store`]: what the drive needs from the device, implemented over the link.
//! - [`vfs`]: the filesystem itself, independent of the OS.
//! - `host`: the background process that serves the drive on Windows, and how
//!   `sp404drive` starts and stops it.
//! - `winfs`: the Windows front end (WinFsp). A macOS front end would sit beside it and
//!   drive the same [`vfs::Drive`] through FUSE.

#[cfg(windows)]
pub mod host;
pub mod link;
pub mod store;
pub mod vfs;
#[cfg(windows)]
pub mod winfs;
