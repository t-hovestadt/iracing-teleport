use clap::{Parser, Subcommand};
use teleport::{source, target, DEFAULT_MULTICAST, DEFAULT_PORT};
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
        #[arg(long, default_value_t = format!("{DEFAULT_MULTICAST}:{DEFAULT_PORT}"))]
        target: String,

        /// Send directly to one host instead of multicast.
        #[arg(long)]
        unicast: bool,

        /// Pin the worker thread to a specific CPU core (0-based).
        #[arg(long, value_name = "N")]
        pin_core: Option<usize>,

        /// Raise the process to HIGH_PRIORITY_CLASS for lower scheduling jitter.
        /// On the iRacing PC this competes with iRacing — only use if the machine
        /// is dedicated to streaming with no game running.
        #[arg(long)]
        high_priority: bool,
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

        /// Spin on the receive socket instead of sleeping. Burns one CPU core but
        /// shaves ~500 µs of OS scheduler jitter off every frame.
        #[arg(long)]
        busy_wait: bool,

        /// Pin the worker thread to a specific CPU core (0-based).
        #[arg(long, value_name = "N")]
        pin_core: Option<usize>,

        /// Spawn a dummy iRacingSim64DX11.exe process so FanaLab detects iRacing
        /// as running on this machine and auto-loads car profiles.
        #[arg(long)]
        fanalab: bool,

        /// Seconds without data before closing the telemetry map. Increase for
        /// long loading screens that exceed the default.
        #[arg(long, default_value_t = teleport::target::DEFAULT_STALE_TIMEOUT_SECS)]
        stale_timeout: u64,

        /// Raise the process to HIGH_PRIORITY_CLASS for lower scheduling jitter.
        /// Safe to use on the SimHub PC.
        #[arg(long)]
        high_priority: bool,
    },
}

fn main() {
    // When spawned as a FanaLab compatibility stub, just park until killed.
    if std::env::args().any(|a| a == "--fanalab-stub") {
        loop { std::thread::park(); }
    }

    let cli = Cli::parse();

    let (tx, rx) = mpsc::channel::<()>();
    ctrlc::set_handler(move || {
        println!("\nShutting down...");
        let _ = tx.send(());
    })
    .expect("failed to install Ctrl-C handler");

    let result = match cli.command {
        Command::Source { bind, target, unicast, pin_core, high_priority } => {
            let mode = if unicast { "unicast" } else { "multicast" };
            println!("source → {target} ({mode})");
            source::run(&bind, &target, unicast, pin_core, high_priority, rx)
        }
        Command::Target { bind, group, unicast, busy_wait, pin_core, fanalab, stale_timeout, high_priority } => {
            let dest = if unicast { "unicast" } else { group.as_str() };
            let mode = if unicast { "unicast" } else { "multicast" };
            println!("target ← {dest} ({mode})");
            target::run(&bind, unicast, &group, busy_wait, pin_core, fanalab, stale_timeout, high_priority, rx)
        }
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
