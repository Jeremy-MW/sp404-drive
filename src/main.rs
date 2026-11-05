use clap::{Parser, Subcommand};

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
    /// More logging in the drive's log file (-v debug, -vv trace).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Mount the SP-404MKII in the background. It stays mounted until `sp404drive unmount`
    /// or until the device is unplugged or switched off.
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
    /// Unmount a drive mounted with `sp404drive mount`, and wait until it is gone.
    Unmount {
        #[arg(default_value = DEFAULT_MOUNT)]
        mount_point: String,
    },
    /// List connected SP-404MKII units.
    Ports,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let log_level = match cli.verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    match cli.command {
        Command::Mount {
            mount_point,
            port,
            read_only,
            cache_secs,
        } => platform::mount(&mount_point, port, read_only, cache_secs, log_level),
        Command::Unmount { mount_point } => platform::unmount(&mount_point),
        Command::Ports => {
            let ports = platform::list_ports()?;
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

#[cfg(windows)]
mod platform {
    use std::time::Duration;

    use anyhow::bail;
    use sp404_drive::host::{self, HOST_EXE};

    /// How long `mount` waits for the drive to report.
    const MOUNT_WAIT: Duration = Duration::from_secs(30);
    /// How long `unmount` waits for files still being saved.
    const UNMOUNT_WAIT: Duration = Duration::from_secs(60);

    pub fn list_ports() -> anyhow::Result<Vec<String>> {
        host::list_ports()
    }

    pub fn mount(
        mount_point: &str,
        port: Option<String>,
        read_only: bool,
        cache_secs: u64,
        log_level: &str,
    ) -> anyhow::Result<()> {
        let host_exe = std::env::current_exe()?.with_file_name(HOST_EXE);
        if !host_exe.exists() {
            bail!("{} is missing", host_exe.display());
        }
        let mut args = vec![
            mount_point.to_string(),
            "--cache-secs".into(),
            cache_secs.to_string(),
            "--log-level".into(),
            log_level.into(),
        ];
        if read_only {
            args.push("--read-only".into());
        }
        if let Some(port) = port {
            args.extend(["--port".into(), port]);
        }
        let log = host::log_path(mount_point);
        match host::start(&host_exe, &args, MOUNT_WAIT)? {
            Ok(()) => {
                println!(
                    "SP-404MKII mounted at {mount_point}{}.",
                    if read_only { " (read-only)" } else { "" }
                );
                println!("Log: {}", log.display());
                Ok(())
            }
            Err(reason) => bail!("{reason} (log: {})", log.display()),
        }
    }

    pub fn unmount(mount_point: &str) -> anyhow::Result<()> {
        if host::unmount(mount_point, UNMOUNT_WAIT)? {
            println!("Unmounted {mount_point}.");
        } else {
            println!(
                "{mount_point} is still unmounting after {} s (saving files?); see {}",
                UNMOUNT_WAIT.as_secs(),
                host::log_path(mount_point).display()
            );
        }
        Ok(())
    }
}

// The macOS front end (FUSE) plugs in here; see the README. `ports` works already.
#[cfg(not(windows))]
mod platform {
    use anyhow::bail;

    #[cfg(target_os = "macos")]
    pub fn list_ports() -> anyhow::Result<Vec<String>> {
        Ok(sp404_drive::link::find_ports()?
            .into_iter()
            .map(|p| format!("{}  {}", p.name, p.instance_id))
            .collect())
    }

    #[cfg(not(target_os = "macos"))]
    pub fn list_ports() -> anyhow::Result<Vec<String>> {
        bail!("not implemented on this platform yet")
    }

    pub fn mount(
        _mount_point: &str,
        _port: Option<String>,
        _read_only: bool,
        _cache_secs: u64,
        _log_level: &str,
    ) -> anyhow::Result<()> {
        bail!("not implemented on this platform yet")
    }

    pub fn unmount(_mount_point: &str) -> anyhow::Result<()> {
        bail!("not implemented on this platform yet")
    }
}
