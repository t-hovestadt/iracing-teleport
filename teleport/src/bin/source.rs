use clap::Parser;
use std::sync::mpsc;
use teleport::{source, DEFAULT_MULTICAST, DEFAULT_PORT};
use teleport::source::DEFAULT_KEYFRAME_INTERVAL;

/// Read iRacing telemetry and broadcast it over UDP to a SimHub PC.
#[derive(Parser)]
#[command(name = "source", version, about)]
struct Args {
    /// Local address to bind the UDP socket to.
    #[arg(long, default_value = "0.0.0.0:0")]
    bind: String,

    /// Destination — multicast group:port, or in unicast mode the target machine's IP:port.
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

    /// Seconds without telemetry data before closing and reconnecting to iRacing.
    /// Increase if iRacing takes longer than 10 s between sessions on your machine.
    #[arg(long, default_value_t = source::DEFAULT_RECONNECT_TIMEOUT_SECS)]
    reconnect_timeout: u64,

    /// Spin on WaitForSingleObject(0) instead of sleeping. Eliminates OS
    /// scheduler wake-up jitter (~0–2 ms) but burns one CPU core. On the
    /// iRacing PC this competes with iRacing; only use on a dedicated PC or
    /// if you have spare cores.
    #[arg(long)]
    busy_wait: bool,

    /// UDP datagram size in bytes. Default (9000) works on jumbo-frame links.
    /// Set to 1472 on standard 1500-byte MTU networks (LAN, WiFi) to avoid
    /// IP fragmentation. Target auto-detects whatever the source uses, so
    /// only source needs this flag.
    #[arg(long, default_value_t = source::DEFAULT_DATAGRAM_SIZE)]
    datagram_size: usize,

    /// Disable XOR-delta compression for partial frames. Delta is enabled by
    /// default when the target supports it; use this flag to force full frames
    /// on every tick (higher bandwidth, zero reconstruction risk).
    #[arg(long)]
    no_delta: bool,

    /// Number of partial frames between full (non-delta) keyframes.
    /// Lower values send more keyframes (safer on lossy links at the cost of
    /// slightly higher bandwidth); higher values maximise delta savings.
    #[arg(long, default_value_t = DEFAULT_KEYFRAME_INTERVAL)]
    keyframe_interval: u16,
}

fn main() {
    let args = Args::parse();

    let (tx, rx) = mpsc::channel::<()>();
    ctrlc::set_handler(move || {
        println!("\nShutting down...");
        let _ = tx.send(());
    })
    .expect("failed to install Ctrl-C handler");

    let mode = if args.unicast { "unicast" } else { "multicast" };
    println!("source → {} ({})", args.target, mode);

    if let Err(e) = source::run(&args.bind, &args.target, args.unicast, args.busy_wait, args.pin_core, args.high_priority, args.reconnect_timeout, args.datagram_size, args.no_delta, args.keyframe_interval, rx) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
