use clap::{Parser, Subcommand};
use teleport::{source, target};
use std::sync::mpsc;

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
        Command::Source { bind, target, unicast } => {
            let mode = if unicast { "unicast" } else { "multicast" };
            println!("source → {target} ({mode})");
            source::run(&bind, &target, unicast, rx)
        }
        Command::Target { bind, group, unicast } => {
            let dest = if unicast { "unicast" } else { group.as_str() };
            let mode = if unicast { "unicast" } else { "multicast" };
            println!("target ← {dest} ({mode})");
            target::run(&bind, unicast, &group, rx)
        }
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
