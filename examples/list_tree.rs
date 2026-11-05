//! List a folder on the SP-404MKII and everything below it, straight over the serial
//! link, without the drive: a check of what is really on the unit after a test. Changes
//! nothing. Run with the drive unmounted and Roland's app closed:
//!
//! ```text
//! cargo run --example list_tree -- SD:SP404DRIVE-TEST               # a folder on the card
//! cargo run --example list_tree -- SD:IMPORT /dev/cu.usbmodem1101   # on this port
//! ```
//!
//! Device paths as in SparkyMK2: `SD:` and below for the card, `""` and below for the
//! internal storage. Like the drive, it waits out the unit's session before closing the
//! port.

use std::sync::Arc;

use anyhow::Context;
use sp404_drive::link::{Link, SerialPort, find_ports};

fn walk(link: &Link, dir: &str, depth: usize, count: &mut (usize, usize)) -> anyhow::Result<()> {
    let mut entries = link
        .list_dir(dir)
        .with_context(|| format!("listing {dir}"))?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for e in entries {
        let path = if dir.ends_with(':') || dir.is_empty() {
            format!("{dir}{}", e.name)
        } else {
            format!("{dir}/{}", e.name)
        };
        let indent = "  ".repeat(depth);
        if e.is_dir() {
            count.0 += 1;
            println!("{indent}{}/", e.name);
            walk(link, &path, depth + 1, count)?;
        } else {
            count.1 += 1;
            let size = link.stat(&path)?.map_or(0, |s| s.size);
            println!("{indent}{}  {size}", e.name);
        }
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = args
        .next()
        .context("usage: list_tree <device path> [port]")?;
    let name = match args.next() {
        Some(name) => name,
        None => find_ports()?
            .first()
            .map(|p| p.name.clone())
            .context("no SP-404MKII found (USB 0582:02E7)")?,
    };
    let port = SerialPort::open(&name)
        .with_context(|| format!("opening {name} (is the drive or Roland's app using it?)"))?;
    let link = Link::new(&name, Arc::new(port), || {})?;
    let mut count = (0, 0);
    println!("{dir}/");
    let result = walk(&link, &dir, 1, &mut count);
    link.let_session_lapse();
    result?;
    println!("{} folders, {} files", count.0, count.1);
    Ok(())
}
