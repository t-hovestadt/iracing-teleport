use lz4_flex::block::{decompress_into, get_maximum_output_size};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::protocol::{MAX_DATAGRAM_SIZE, Receiver as ProtoReceiver};
use crate::stats::Stats;
use crate::telemetry::{MAX_TELEMETRY_SIZE, Telemetry, TelemetryProvider};

const STALE_TIMEOUT: Duration = Duration::from_secs(10);

pub fn run(
    bind: &str,
    unicast: bool,
    multicast_group: &str,
    shutdown: mpsc::Receiver<()>,
) -> std::io::Result<()> {
    // Build the socket manually so we can set the receive buffer before binding.
    // A single frame arrives as a burst of ~23 × 9KB fragments (~207KB). The OS
    // default (64KB on Windows) drops everything beyond the 7th fragment,
    // losing the whole frame. 2MB holds ~9 full frames with room to spare.
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_recv_buffer_size(2 * 1024 * 1024)?;
    sock.set_reuse_address(true)?;
    let bind_addr: SocketAddr = bind.parse()
        .map_err(|e| std::io::Error::other(format!("invalid bind address: {e}")))?;
    sock.bind(&bind_addr.into())?;
    let socket: UdpSocket = sock.into();
    socket.set_read_timeout(Some(Duration::from_secs(1)))?;
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
    let mut stats = Stats::new("target");
    let mut seq_start: Option<Instant> = None;

    loop {
        if shutdown.try_recv().is_ok() {
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
                        stats.record(compressed.len(), proto.last_fragment_count, proto.last_source_us + transit_us);
                    }

                    last_update = Instant::now();
                    stats.maybe_print();
                }
            }

            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Drop the telemetry map when we haven't heard from the source for a while.
                if telemetry.is_some() && last_update.elapsed() >= STALE_TIMEOUT {
                    println!("No data for {}s — closing telemetry map.", STALE_TIMEOUT.as_secs());
                    telemetry = None;
                }
            }

            Err(e) => return Err(e),
        }
    }
}
