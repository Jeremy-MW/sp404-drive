//! Check how long the SP-404MKII keeps an idle file handle open (issue #1).
//!
//! The unit closes every open handle once the link has had no requests for a couple of
//! seconds, and hands the numbers out again. Run with the drive unmounted:
//!
//! ```text
//! cargo run --example idle_handles [device path]
//! ```
//!
//! The default path is the factory library, `FCTRY/FACTORY.SVD`; a card file looks like
//! `SD:EXPORT/PROJECT/<export>/PADCONF.BIN`. Measured on 2026-11-05: alive after 2.2 s
//! idle, closed after 2.5 s, on both volumes; other requests keep it alive.

use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use sp404_drive::link::windows::{SerialPort, find_ports};
use sp404_drive::link::{Link, OPEN_READ_ONLY};

fn alive(link: &Link, h: i32) -> String {
    match link.seek(h, 0).and_then(|_| link.read(h, 16)) {
        Ok(d) => format!("alive ({} bytes)", d.len()),
        Err(e) => format!("closed ({e})"),
    }
}

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "FCTRY/FACTORY.SVD".into());
    let port = find_ports()?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no SP-404MKII found"))?
        .name;
    let link = Link::new(&port, Arc::new(SerialPort::open(&port)?), || {})?;
    println!("{path} on {port}");

    for ms in [1000u64, 2000, 2200, 2500, 3000] {
        let h = link.open_file(&path, OPEN_READ_ONLY)?;
        sleep(Duration::from_millis(ms));
        println!("idle {ms} ms: handle {h} {}", alive(&link, h));
        let _ = link.close_file(h);
    }

    let h = link.open_file(&path, OPEN_READ_ONLY)?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(6) {
        sleep(Duration::from_millis(500));
        link.stat(&path)?;
    }
    println!(
        "6 s with a stat every 500 ms: handle {h} {}",
        alive(&link, h)
    );
    let _ = link.close_file(h);

    let _ = link.mkii_exit();
    Ok(())
}
