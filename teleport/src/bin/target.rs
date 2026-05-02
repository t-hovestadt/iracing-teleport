use clap::Parser;
use std::sync::mpsc;
use teleport::{target, DEFAULT_MULTICAST, DEFAULT_PORT};

/// Receive iRacing telemetry and expose it as a local memory map for SimHub.
#[derive(Parser)]
#[command(name = "target", version, about)]
struct Args {
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
    /// as running on this machine and auto-loads car profiles. The process is
    /// started when an iRacing session is detected and killed when it drops.
    #[arg(long)]
    fanalab: bool,

    /// Seconds without data before closing the telemetry map. Increase for
    /// long loading screens that exceed the default.
    #[arg(long, default_value_t = target::DEFAULT_STALE_TIMEOUT_SECS)]
    stale_timeout: u64,

    /// Raise the process to HIGH_PRIORITY_CLASS for lower scheduling jitter.
    /// Safe to use on the SimHub PC.
    #[arg(long)]
    high_priority: bool,
}

fn main() {
    // When spawned as a FanaLab compatibility stub, just park until killed.
    if std::env::args().any(|a| a == "--fanalab-stub") {
        loop {
            std::thread::park();
        }
    }

    let args = Args::parse();

    let (tx, rx) = mpsc::channel::<()>();
    ctrlc::set_handler(move || {
        println!("\nShutting down...");
        let _ = tx.send(());
    })
    .expect("failed to install Ctrl-C handler");

    let dest = if args.unicast {
        "unicast"
    } else {
        args.group.as_str()
    };
    let mode = if args.unicast { "unicast" } else { "multicast" };
    println!("target ← {dest} ({mode})");

    if let Err(e) = target::run(
        &args.bind,
        args.unicast,
        &args.group,
        args.busy_wait,
        args.pin_core,
        args.fanalab,
        args.stale_timeout,
        args.high_priority,
        rx,
    ) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
