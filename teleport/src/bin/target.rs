use clap::Parser;
use std::sync::mpsc;

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

    let dest = if args.unicast {
        "unicast"
    } else {
        args.group.as_str()
    };
    let mode = if args.unicast { "unicast" } else { "multicast" };
    println!("target ← {dest} ({mode})");

    let result = teleport::run_target(
        teleport::TargetConfig {
            bind: args.bind,
            unicast: args.unicast,
            multicast_group: args.group,
            busy_wait: args.busy_wait,
            pin_core: args.pin_core,
            high_priority: args.high_priority,
            fanalab: args.fanalab,
            zero_on_exit: args.zero_on_exit,
            ..Default::default()
        },
        rx,
    );

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
