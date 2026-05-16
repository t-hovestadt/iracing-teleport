use lz4_flex::block::{decompress_into, get_maximum_output_size};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::protocol::{Receiver as ProtoReceiver, MAX_DATAGRAM_SIZE};
use crate::stats::Stats;
use crate::telemetry::{Telemetry, TelemetryProvider, MAX_TELEMETRY_SIZE};

/// Zero the shared-memory map after this much silence so SimHub dashboards
/// immediately show stopped state (Fix 3).
const STALE_TIMEOUT: Duration = Duration::from_secs(30);
/// Close and release the map entirely after this much silence (Fix 3).
const CLOSE_TIMEOUT: Duration = Duration::from_secs(60);

#[allow(clippy::too_many_arguments)]
pub fn run(
    bind: &str,
    unicast: bool,
    multicast_group: &str,
    busy_wait: bool,
    zero_on_exit: bool,
    on_first_data: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    on_stale: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    shutdown: mpsc::Receiver<()>,
) -> std::io::Result<()> {
    // Build the socket manually so we can set the receive buffer before binding.
    // A single frame arrives as a burst of ~23 × 9KB fragments (~207KB). The OS
    // default (64KB on Windows) drops everything beyond the 7th fragment,
    // losing the whole frame. 2MB holds ~9 full frames with room to spare.
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_recv_buffer_size(2 * 1024 * 1024)?;
    sock.set_reuse_address(true)?;
    let bind_addr: SocketAddr = bind
        .parse()
        .map_err(|e| std::io::Error::other(format!("invalid bind address: {e}")))?;
    sock.bind(&bind_addr.into())?;
    let socket: UdpSocket = sock.into();
    if busy_wait {
        socket.set_nonblocking(true)?;
    } else {
        socket.set_read_timeout(Some(Duration::from_secs(1)))?;
    }
    println!("Listening on {bind}");

    if !unicast {
        let group: Ipv4Addr = multicast_group
            .parse()
            .map_err(|e| std::io::Error::other(format!("bad multicast address: {e}")))?;
        socket.join_multicast_v4(&group, &Ipv4Addr::UNSPECIFIED)?;
        println!("Joined multicast group {group}");
    }

    let mut recv_buf = [0u8; MAX_DATAGRAM_SIZE];
    let mut proto = ProtoReceiver::new(get_maximum_output_size(MAX_TELEMETRY_SIZE));
    let mut telemetry: Option<Telemetry> = None;
    let mut last_update = Instant::now();
    // Fix 3: track whether we've already zeroed the map during the current
    // stale window so we don't repeatedly fill(0) every poll tick.
    let mut zeroed_at: Option<Instant> = None;
    // Tracks whether on_first_data has been fired for the current active
    // window. Reset when on_stale fires so on_first_data re-fires on resume.
    let mut data_announced = false;
    let mut stats = Stats::new("target");
    let mut seq_start: Option<Instant> = None;

    loop {
        if shutdown.try_recv().is_ok() {
            if zero_on_exit {
                if let Some(ref mut t) = telemetry {
                    t.as_slice_mut().fill(0);
                    let _ = t.signal_data_ready();
                }
            }
            return Ok(());
        }

        match socket.recv_from(&mut recv_buf) {
            Ok((len, _src)) => {
                let (assembled, new_seq) = proto.ingest(&recv_buf[..len]);

                if new_seq {
                    seq_start = Some(Instant::now());
                }

                if let Some(compressed) = assembled {
                    // Lazily create the local telemetry object the first time data arrives.
                    if telemetry.is_none() {
                        match Telemetry::create(MAX_TELEMETRY_SIZE) {
                            Ok(t) => {
                                println!("Created local telemetry memory map.");
                                telemetry = Some(t);
                                data_announced = false; // will fire below
                            }
                            Err(e) => {
                                return Err(std::io::Error::other(format!(
                                    "failed to create telemetry: {e}"
                                )));
                            }
                        }
                    }

                    // Decompress directly into the mapped memory — zero extra allocation.
                    let t = telemetry.as_mut().unwrap();
                    if let Err(e) = decompress_into(compressed, t.as_slice_mut()) {
                        eprintln!("decompression failed: {e}");
                        continue;
                    }

                    if let Err(e) = t.signal_data_ready() {
                        eprintln!("signal_data_ready failed: {e}");
                    }

                    // Compute end-to-end latency: source processing + network transit.
                    if let Some(start) = seq_start.take() {
                        let transit_us = start.elapsed().as_micros() as u64;
                        stats.record(
                            compressed.len(),
                            proto.last_fragment_count,
                            proto.last_source_us + transit_us,
                        );
                    }

                    last_update = Instant::now();
                    zeroed_at = None; // Fix 3: reset zeroed flag on live data
                    if !data_announced {
                        data_announced = true;
                        if let Some(ref cb) = on_first_data {
                            cb();
                        }
                    }
                    stats.maybe_print();
                }
            }

            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if busy_wait {
                    std::hint::spin_loop();
                }
                // Fix 3: Two-stage stale handling.
                //  • 30 s — zero the map so SimHub dashboards show stopped state,
                //            but keep the handle open for fast resume.
                //  • 60 s — close and release the map entirely.
                let mut close_map = false;
                if let Some(ref mut t) = telemetry {
                    let stale = last_update.elapsed();
                    if stale >= CLOSE_TIMEOUT {
                        close_map = true;
                    } else if stale >= STALE_TIMEOUT && zeroed_at.is_none() {
                        println!(
                            "No data for {}s — zeroing telemetry map (closes at {}s).",
                            STALE_TIMEOUT.as_secs(),
                            CLOSE_TIMEOUT.as_secs()
                        );
                        t.as_slice_mut().fill(0);
                        let _ = t.signal_data_ready();
                        zeroed_at = Some(Instant::now());
                        data_announced = false;
                        if let Some(ref cb) = on_stale {
                            cb();
                        }
                    }
                }
                if close_map {
                    println!(
                        "No data for {}s — closing telemetry map.",
                        CLOSE_TIMEOUT.as_secs()
                    );
                    telemetry = None;
                    zeroed_at = None;
                }
            }

            Err(e) => return Err(e),
        }
    }
}
