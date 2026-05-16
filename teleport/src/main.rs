use clap::{Parser, Subcommand};
use std::sync::mpsc;

/// Stream iRacing telemetry over the network so SimHub (or any iRacing-compatible
/// app) can run on a different machine than your iRacing installation.
#[derive(Parser)]
#[command(name = "teleport", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Read iRacing telemetry and broadcast it over UDP.
    Source {
        /// Local address to bind the UDP socket to.
        #[arg(long, default_value = "0.0.0.0:0")]
        bind: String,

        /// Destination — multicast group:port or, in unicast mode, the target machine's address.
        #[arg(long, default_value_t = format!("{}:{}", teleport::DEFAULT_MULTICAST, teleport::DEFAULT_PORT))]
        target: String,

        /// Send directly to one host instead of multicast.
        #[arg(long)]
        unicast: bool,

        /// Skip CPU 0 exclusion (for non-iRacing sims or when using Process Lasso).
        #[arg(long)]
        no_cpu_exclude: bool,

        /// Spin on the iRacing data-ready event instead of sleeping (lower jitter, costs one CPU core).
        #[arg(long)]
        busy_wait: bool,

        /// Set HIGH_PRIORITY_CLASS on this process.
        #[arg(long)]
        high_priority: bool,

        /// Pin this process to a specific CPU core (0-based).
        #[arg(long, value_name = "CORE")]
        pin_core: Option<usize>,

        /// Spawn a dummy iRacingSim64DX11.exe so FanaLab / fanatec-tuner detect iRacing.
        #[arg(long)]
        fanalab: bool,

        /// Zero the local shared-memory map on shutdown (no-op on source — map lives on target).
        #[arg(long)]
        zero_on_exit: bool,
    },

    /// Receive telemetry and expose it as a local iRacing memory map.
    Target {
        /// Address and port to listen on.
        #[arg(long, default_value_t = format!("0.0.0.0:{}", teleport::DEFAULT_PORT))]
        bind: String,

        /// Multicast group to join (ignored in unicast mode).
        #[arg(long, default_value = teleport::DEFAULT_MULTICAST)]
        group: String,

        /// Expect a direct unicast stream instead of multicast.
        #[arg(long)]
        unicast: bool,

        /// Skip CPU 0 exclusion.
        #[arg(long)]
        no_cpu_exclude: bool,

        /// Spin on recv instead of sleeping (lower jitter, costs one CPU core).
        #[arg(long)]
        busy_wait: bool,

        /// Set HIGH_PRIORITY_CLASS on this process.
        #[arg(long)]
        high_priority: bool,

        /// Pin this process to a specific CPU core (0-based).
        #[arg(long, value_name = "CORE")]
        pin_core: Option<usize>,

        /// Spawn a dummy iRacingSim64DX11.exe so FanaLab / fanatec-tuner detect iRacing.
        #[arg(long)]
        fanalab: bool,

        /// Zero the shared-memory map before dropping it on shutdown.
        #[arg(long)]
        zero_on_exit: bool,
    },
}

fn main() {
    // Hidden stub mode: sleep forever so FanaLab / fanatec-tuner see a process
    // named iRacingSim64DX11.exe without this binary doing any real work.
    if std::env::args().any(|a| a == "--fanalab-stub") {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }

    let cli = Cli::parse();

    let (tx, rx) = mpsc::channel::<()>();
    ctrlc::set_handler(move || {
        println!("\nShutting down...");
        let _ = tx.send(());
    })
    .expect("failed to install Ctrl-C handler");

    let result = match cli.command {
        Command::Source {
            bind,
            target,
            unicast,
            no_cpu_exclude,
            busy_wait,
            high_priority,
            pin_core,
            fanalab,
            zero_on_exit,
        } => {
            if no_cpu_exclude {
                eprintln!("[cpu] CPU 0 exclusion disabled by --no-cpu-exclude flag");
            } else {
                teleport::avoid_cpu0();
            }
            let mode = if unicast { "unicast" } else { "multicast" };
            println!("source → {target} ({mode})");
            teleport::run_source(
                teleport::SourceConfig {
                    bind,
                    target,
                    unicast,
                    busy_wait,
                    pin_core,
                    high_priority,
                    fanalab,
                    zero_on_exit,
                },
                rx,
            )
        }
        Command::Target {
            bind,
            group,
            unicast,
            no_cpu_exclude,
            busy_wait,
            high_priority,
            pin_core,
            fanalab,
            zero_on_exit,
        } => {
            if no_cpu_exclude {
                eprintln!("[cpu] CPU 0 exclusion disabled by --no-cpu-exclude flag");
            } else {
                teleport::avoid_cpu0();
            }
            let dest = if unicast { "unicast" } else { group.as_str() };
            let mode = if unicast { "unicast" } else { "multicast" };
            println!("target ← {dest} ({mode})");
            teleport::run_target(
                teleport::TargetConfig {
                    bind,
                    unicast,
                    multicast_group: group,
                    busy_wait,
                    pin_core,
                    high_priority,
                    fanalab,
                    zero_on_exit,
                    ..Default::default()
                },
                rx,
            )
        }
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
