use clap::{Parser, Subcommand};
use std::sync::mpsc;
use teleport::{source, target};

const DEFAULT_MULTICAST: &str = "239.255.0.1";
const DEFAULT_PORT: u16 = 5000;

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
        #[arg(long, default_value_t = format!("{DEFAULT_MULTICAST}:{DEFAULT_PORT}"))]
        target: String,

        /// Send directly to one host instead of multicast.
        #[arg(long)]
        unicast: bool,

        /// Skip CPU 0 exclusion (for non-iRacing sims or when using Process Lasso).
        #[arg(long)]
        no_cpu_exclude: bool,
    },

    /// Receive telemetry and expose it as a local iRacing memory map.
    Target {
        /// Address and port to listen on.
        #[arg(long, default_value_t = format!("0.0.0.0:{DEFAULT_PORT}"))]
        bind: String,

        /// Multicast group to join (ignored in unicast mode).
        #[arg(long, default_value = DEFAULT_MULTICAST)]
        group: String,

        /// Expect a direct unicast stream instead of multicast.
        #[arg(long)]
        unicast: bool,

        /// Spin on recv instead of sleeping (lower jitter, costs one CPU core).
        #[arg(long)]
        busy_wait: bool,
    },
}

fn main() {
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
        } => {
            if no_cpu_exclude {
                eprintln!("[cpu] CPU 0 exclusion disabled by --no-cpu-exclude flag");
            } else {
                teleport::avoid_cpu0();
            }
            let mode = if unicast { "unicast" } else { "multicast" };
            println!("source → {target} ({mode})");
            source::run(&bind, &target, unicast, rx)
        }
        Command::Target {
            bind,
            group,
            unicast,
            busy_wait,
        } => {
            let dest = if unicast { "unicast" } else { group.as_str() };
            let mode = if unicast { "unicast" } else { "multicast" };
            println!("target ← {dest} ({mode})");
            target::run(&bind, unicast, &group, busy_wait, rx)
        }
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
