use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

#[cfg(not(windows))]
use anyhow::bail;
use clap::{Parser, Subcommand};

use sp404_drive::link::Link;
use sp404_drive::store::DeviceStore;
use sp404_drive::vfs::{Drive, Options};

#[cfg(windows)]
const DEFAULT_MOUNT: &str = "S:";
#[cfg(not(windows))]
const DEFAULT_MOUNT: &str = "/Volumes/SP-404MKII";

#[derive(Parser)]
#[command(
    name = "sp404drive",
    version,
    about = "Mount a Roland SP-404MKII as a drive"
)]
struct Cli {
    /// More logging (-v debug, -vv trace).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Mount the SP-404MKII and serve it until unmounted, Ctrl+C, or the device is unplugged.
    Mount {
        /// Drive letter (`S:`) or an empty folder to mount on.
        #[arg(default_value = DEFAULT_MOUNT)]
        mount_point: String,
        /// Serial port of the device (default: the first SP-404MKII found).
        #[arg(long)]
        port: Option<String>,
        /// Refuse all changes.
        #[arg(long)]
        read_only: bool,
        /// Seconds a directory listing is reused before asking the device again.
        #[arg(long, default_value_t = 5)]
        cache_secs: u64,
    },
    /// Unmount a drive mounted with `sp404drive mount`.
    Unmount {
        #[arg(default_value = DEFAULT_MOUNT)]
        mount_point: String,
    },
    /// List connected SP-404MKII units.
    Ports,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let level = match cli.verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level))
        .format_timestamp_millis()
        .init();

    match cli.command {
        Command::Mount {
            mount_point,
            port,
            read_only,
            cache_secs,
        } => mount(&mount_point, port, read_only, cache_secs),
        Command::Unmount { mount_point } => unmount(&mount_point),
        Command::Ports => {
            let ports = list_ports()?;
            if ports.is_empty() {
                println!("No SP-404MKII found.");
            }
            for p in ports {
                println!("{p}");
            }
            Ok(())
        }
    }
}

fn mount(
    mount_point: &str,
    port: Option<String>,
    read_only: bool,
    cache_secs: u64,
) -> anyhow::Result<()> {
    let (stop_tx, stop_rx) = mpsc::channel::<String>();
    listen_for_unmount(mount_point, stop_tx.clone())?;

    // Unplugging the device, or switching it off, fails the link's pending read at once.
    let dev = {
        let tx = stop_tx.clone();
        open_link(port, move || {
            let _ = tx.send("the SP-404MKII was disconnected".into());
        })?
    };
    log::info!("connected to the SP-404MKII on {}", dev.port_name());

    let drive = Arc::new(Drive::new(
        Arc::new(DeviceStore::new(dev)),
        Options {
            read_only,
            cache_ttl: Duration::from_secs(cache_secs),
        },
    ));

    ctrlc::set_handler(move || {
        let _ = stop_tx.send("Ctrl+C".into());
    })?;
    serve(drive, mount_point, stop_rx)
}

#[cfg(windows)]
fn list_ports() -> anyhow::Result<Vec<String>> {
    Ok(sp404_drive::link::windows::find_ports()?
        .into_iter()
        .map(|p| format!("{}  {}", p.name, p.instance_id))
        .collect())
}

#[cfg(windows)]
fn open_link(
    port: Option<String>,
    on_closed: impl FnOnce() + Send + 'static,
) -> anyhow::Result<Link> {
    use anyhow::Context;
    use sp404_drive::link::windows::{SerialPort, find_ports};
    let name = match port {
        Some(name) => name,
        None => find_ports()?
            .into_iter()
            .next()
            .map(|p| p.name)
            .context("no SP-404MKII found (USB 0582:02E7)")?,
    };
    let serial = SerialPort::open(&name)
        .with_context(|| format!("opening {name} (is Roland's SP-404MKII app still connected?)"))?;
    Ok(Link::new(&name, Arc::new(serial), on_closed)?)
}

#[cfg(windows)]
fn serve(drive: Arc<Drive>, mount_point: &str, stop: mpsc::Receiver<String>) -> anyhow::Result<()> {
    sp404_drive::winfs::run(drive, mount_point, stop)
}

#[cfg(windows)]
fn listen_for_unmount(mount_point: &str, stop: Sender<String>) -> anyhow::Result<()> {
    sp404_drive::winfs::listen_for_unmount(mount_point, move || {
        let _ = stop.send("unmount requested".into());
    })
}

#[cfg(windows)]
fn unmount(mount_point: &str) -> anyhow::Result<()> {
    sp404_drive::winfs::signal_unmount(mount_point)?;
    println!("Asked sp404drive to unmount {mount_point}.");
    Ok(())
}

// A macOS front end (FUSE) and serial port plug in here; see the README.
#[cfg(not(windows))]
fn list_ports() -> anyhow::Result<Vec<String>> {
    bail!("not implemented on this platform yet")
}

#[cfg(not(windows))]
fn open_link(
    _port: Option<String>,
    _on_closed: impl FnOnce() + Send + 'static,
) -> anyhow::Result<Link> {
    bail!("not implemented on this platform yet")
}

#[cfg(not(windows))]
fn serve(
    _drive: Arc<Drive>,
    _mount_point: &str,
    _stop: mpsc::Receiver<String>,
) -> anyhow::Result<()> {
    bail!("mounting is not implemented on this platform yet")
}

#[cfg(not(windows))]
fn listen_for_unmount(_mount_point: &str, _stop: Sender<String>) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn unmount(_mount_point: &str) -> anyhow::Result<()> {
    bail!("mounting is not implemented on this platform yet")
}
