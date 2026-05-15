use clap::Parser;
use std::sync::mpsc;

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
}

fn main() {
    // Hidden stub mode: sleep forever so FanaLab / fanatec-tuner see a process
    // named iRacingSim64DX11.exe without this binary doing any real work.
    if std::env::args().any(|a| a == "--fanalab-stub") {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }

    let args = Args::parse();

    if args.no_cpu_exclude {
        eprintln!("[cpu] CPU 0 exclusion disabled by --no-cpu-exclude flag");
    } else {
        teleport::avoid_cpu0();
    }

    let (tx, rx) = mpsc::channel::<()>();
    ctrlc::set_handler(move || {
        println!("\nShutting down...");
        let _ = tx.send(());
    })
    .expect("failed to install Ctrl-C handler");

    let mode = if args.unicast { "unicast" } else { "multicast" };
    println!("source → {} ({})", args.target, mode);

    let result = teleport::run_source(
        teleport::SourceConfig {
            bind: args.bind,
            target: args.target,
            unicast: args.unicast,
            busy_wait: args.busy_wait,
            pin_core: args.pin_core,
            high_priority: args.high_priority,
            fanalab: args.fanalab,
            zero_on_exit: args.zero_on_exit,
        },
        rx,
    );

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
