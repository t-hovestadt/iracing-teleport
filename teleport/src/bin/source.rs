use clap::Parser;
use std::sync::mpsc;
use teleport::source;

const DEFAULT_MULTICAST: &str = "239.255.0.1";
const DEFAULT_PORT: u16 = 5000;

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

    if let Err(e) = source::run(&args.bind, &args.target, args.unicast, args.pin_core, rx) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
