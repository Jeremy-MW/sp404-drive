// A GUI-subsystem program that creates no window: it never has a console or a window of
// its own, whoever starts it.
#![windows_subsystem = "windows"]

//! The drive's background process; see `sp404_drive::host`. `sp404drive mount` starts it.

#[cfg(windows)]
fn main() {
    use clap::Parser;
    use sp404_drive::host::{self, MountArgs};

    #[derive(Parser)]
    struct Args {
        mount_point: String,
        #[arg(long)]
        port: Option<String>,
        #[arg(long)]
        read_only: bool,
        #[arg(long, default_value_t = 5)]
        cache_secs: u64,
        #[arg(long, default_value = "info")]
        log_level: String,
        /// Named pipe to report to (given by `sp404drive mount`).
        #[arg(long)]
        report_pipe: Option<String>,
    }

    let args = Args::parse();
    let pipe = args.report_pipe.as_deref();
    // Before the log: a second host for the same mount point must not touch the first's.
    let running = match host::Running::claim(&args.mount_point) {
        Ok(running) => running,
        Err(e) => {
            host::report_to_pipe(pipe, Err(format!("{e:#}")));
            std::process::exit(1);
        }
    };
    let log_path = host::log_path(&args.mount_point);
    if let Some(dir) = log_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let target = match std::fs::File::create(&log_path) {
        Ok(file) => env_logger::Target::Pipe(Box::new(file)),
        Err(_) => env_logger::Target::Stderr,
    };
    env_logger::Builder::new()
        .parse_filters(&args.log_level)
        .target(target)
        .format_timestamp_millis()
        .init();
    std::panic::set_hook(Box::new(|info| log::error!("{info}")));

    let mount = MountArgs {
        mount_point: args.mount_point,
        port: args.port,
        read_only: args.read_only,
        cache_secs: args.cache_secs,
    };
    if let Err(e) = host::serve(&mount, running, |status| host::report_to_pipe(pipe, status)) {
        log::error!("{e:#}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("sp404drive-host is not implemented on this platform yet");
    std::process::exit(1);
}
