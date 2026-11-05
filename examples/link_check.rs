//! Check the serial link to the SP-404MKII without mounting anything: find the unit, ask
//! for the internal storage's free space, list both roots, and read the first 16 bytes of
//! a file on the card. Changes nothing on the unit. Run with the drive unmounted and
//! Roland's app closed:
//!
//! ```text
//! cargo run --example link_check                         # first SP-404MKII found
//! cargo run --example link_check -- /dev/cu.usbmodem1101 # or COM4 on Windows
//! cargo run --example link_check -- --hold               # then keep the port open
//! ```
//!
//! Before closing the port it waits out the unit's session, as the drive does on unmount.
//! With `--hold` it then keeps the port open, sending nothing, until the unit is unplugged
//! or switched off: a check that the reader notices at once, and sleeps until then.

use std::sync::{Arc, mpsc};
use std::time::Instant;

use anyhow::Context;
use sp404_drive::link::{Link, OPEN_READ_ONLY, SerialPort, find_ports};

fn show(link: &Link, label: &str, path: &str) -> anyhow::Result<Option<String>> {
    let started = Instant::now();
    let entries = link
        .list_dir(path)
        .with_context(|| format!("listing {label}"))?;
    println!(
        "{label}: {} entries in {} ms",
        entries.len(),
        started.elapsed().as_millis()
    );
    for e in entries.iter().take(12) {
        println!("  {}{}", e.name, if e.is_dir() { "/" } else { "" });
    }
    if entries.len() > 12 {
        println!("  ... and {} more", entries.len() - 12);
    }
    Ok(entries
        .iter()
        .find(|e| !e.is_dir())
        .map(|e| format!("{path}{}", e.name)))
}

fn main() -> anyhow::Result<()> {
    let ports = find_ports()?;
    for p in &ports {
        println!("found {}  {}", p.name, p.instance_id);
    }
    let hold = std::env::args().any(|a| a == "--hold");
    let name = match std::env::args().skip(1).find(|a| !a.starts_with("--")) {
        Some(name) => name,
        None => ports
            .first()
            .map(|p| p.name.clone())
            .context("no SP-404MKII found (USB 0582:02E7)")?,
    };
    let port = SerialPort::open(&name)
        .with_context(|| format!("opening {name} (is the drive or Roland's app using it?)"))?;
    let (closed_tx, closed) = mpsc::channel();
    let link = Link::new(&name, Arc::new(port), move || {
        let _ = closed_tx.send(Instant::now());
    })?;
    println!("opened {name}");

    let result = (|| -> anyhow::Result<()> {
        let free = link.free_kb().context("asking for free space")?;
        println!(
            "internal storage free: {:.2} GB",
            free as f64 / 1024.0 / 1024.0
        );
        show(&link, "Internal", "")?;
        if let Some(file) = show(&link, "SD-Card", "SD:")? {
            let stat = link.stat(&file)?.context("the file vanished")?;
            let h = link.open_file(&file, OPEN_READ_ONLY)?;
            let head = link.read(h, 16);
            let _ = link.close_file(h);
            let head = head?;
            println!("{file}: {} bytes, starts {:02X?}", stat.size, head);
        }
        Ok(())
    })();

    let started = Instant::now();
    link.let_session_lapse();
    println!(
        "waited {} ms for the unit's session to lapse",
        started.elapsed().as_millis()
    );
    if hold && result.is_ok() {
        println!("holding the port, sending nothing: unplug the unit or switch it off");
        let at = closed.recv()?;
        println!(
            "the link closed after {:.1} s; the reader noticed {} ms before this line",
            started.elapsed().as_secs_f64(),
            at.elapsed().as_millis()
        );
    } else {
        println!("closing the port");
    }
    result
}
