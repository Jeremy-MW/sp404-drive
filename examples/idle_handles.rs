//! Check how long the SP-404MKII keeps an idle file handle open (issue #1), and what keeps
//! it open.
//!
//! The unit closes every open handle once the link has had no requests for a couple of
//! seconds, and hands the numbers out again. Run with the drive unmounted:
//!
//! ```text
//! cargo run --example idle_handles              # timeout, and what resets it
//! cargo run --example idle_handles -- --unit    # 10 s idle while you use the unit
//! ```
//!
//! Uses `FCTRY/FACTORY.SVD` on the internal storage and the first file in the card's root.
//! Measured on 2026-11-05: alive after 2.2 s idle, closed after 2.5 s, on both volumes. Any
//! message from the host keeps handles open, on either volume or the control channel; using
//! the unit itself doesn't.

use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use sp404_drive::link::windows::{SerialPort, find_ports};
use sp404_drive::link::{Link, OPEN_READ_ONLY, Port};
use sp404_proto::{Channel, Message, control};

const INTERNAL: &str = "FCTRY/FACTORY.SVD";

fn alive(link: &Link, h: i32) -> String {
    match link.seek(h, 0).and_then(|_| link.read(h, 16)) {
        Ok(d) => format!("alive ({} bytes)", d.len()),
        Err(e) => format!("closed ({e})"),
    }
}

/// Open `path`, run `request` every 500 ms for 6 s, then see whether the handle survived.
fn kept_open(
    link: &Link,
    path: &str,
    what: &str,
    mut request: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let h = link.open_file(path, OPEN_READ_ONLY)?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(6) {
        sleep(Duration::from_millis(500));
        request()?;
    }
    println!("{path}, 6 s with {what} every 500 ms: {}", alive(link, h));
    let _ = link.close_file(h);
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let unit_activity = std::env::args().any(|a| a == "--unit");
    let name = find_ports()?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no SP-404MKII found"))?
        .name;
    let port = Arc::new(SerialPort::open(&name)?);
    let link = Link::new(&name, port.clone(), || {})?;
    let card = link
        .list_dir("SD:")?
        .into_iter()
        .find(|e| !e.is_dir())
        .map(|e| format!("SD:{}", e.name))
        .ok_or_else(|| anyhow::anyhow!("no file in the card's root"))?;
    println!("on {name}: {INTERNAL} and {card}");

    if unit_activity {
        let h = link.open_file(&card, OPEN_READ_ONLY)?;
        println!("{card} open; nothing sent for 10 s");
        sleep(Duration::from_secs(10));
        println!("{card}, after 10 s: {}", alive(&link, h));
        let _ = link.close_file(h);
        link.let_session_lapse();
        return Ok(());
    }

    for path in [INTERNAL, card.as_str()] {
        for ms in [2000u64, 2200, 2500, 3000] {
            let h = link.open_file(path, OPEN_READ_ONLY)?;
            sleep(Duration::from_millis(ms));
            println!("{path}, idle {ms} ms: {}", alive(&link, h));
            let _ = link.close_file(h);
        }
    }

    kept_open(&link, &card, "a stat of the same file", || {
        link.stat(&card)?;
        Ok(())
    })?;
    kept_open(&link, &card, "a stat on the internal storage", || {
        link.stat(INTERNAL)?;
        Ok(())
    })?;
    kept_open(&link, INTERNAL, "a listing of the card's root", || {
        link.list_dir("SD:")?;
        Ok(())
    })?;
    kept_open(&link, &card, "a failed stat (no such file)", || {
        assert!(link.stat("SD:NO_SUCH_FILE_404")?.is_none());
        Ok(())
    })?;
    let status = Message::short(Channel::Control, &control::status_request()).encode();
    kept_open(&link, &card, "a control-channel status request", || {
        port.write_all(&status)?;
        Ok(())
    })?;

    link.let_session_lapse();
    Ok(())
}
