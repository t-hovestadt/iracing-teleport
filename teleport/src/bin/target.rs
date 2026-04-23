use clap::Parser;
use std::sync::mpsc;
use teleport::target;

const DEFAULT_MULTICAST: &str = "239.255.0.1";
const DEFAULT_PORT: u16 = 5000;

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
}

fn main() {
    let args = Args::parse();

    let (tx, rx) = mpsc::channel::<()>();
    ctrlc::set_handler(move || {
        println!("\nShutting down...");
        let _ = tx.send(());
    })
    .expect("failed to install Ctrl-C handler");

    let dest = if args.unicast { "unicast" } else { args.group.as_str() };
    let mode = if args.unicast { "unicast" } else { "multicast" };
    println!("target ← {dest} ({mode})");

    if let Err(e) = target::run(&args.bind, args.unicast, &args.group, rx) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
