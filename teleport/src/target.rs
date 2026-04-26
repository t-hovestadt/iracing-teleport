use lz4_flex::block::{decompress_into, get_maximum_output_size};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::platform::{boost_thread_priority, pin_thread_to_core, HighResTimer};
use crate::protocol::{MAX_DATAGRAM_SIZE, Receiver as ProtoReceiver};
use crate::stats::Stats;
use crate::telemetry::{MAX_TELEMETRY_SIZE, Telemetry, TelemetryProvider};

const STALE_TIMEOUT: Duration = Duration::from_secs(10);

pub fn run(
    bind: &str,
    unicast: bool,
    multicast_group: &str,
    busy_wait: bool,
    pin_core: Option<usize>,
    shutdown: mpsc::Receiver<()>,
) -> std::io::Result<()> {
    let _timer = HighResTimer::acquire();
    boost_thread_priority();
    if let Some(core) = pin_core {
        pin_thread_to_core(core);
    }

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
    if busy_wait {
        // Spin on recv_from. Burns one core but cuts ~500 µs of OS scheduler
        // wake-up jitter off every frame.
        socket.set_nonblocking(true)?;
        println!("Busy-wait mode: target thread will burn one CPU core for lower latency.");
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
    // Staging buffer for partial-frame decompression: the payload is
    // irsdk_header (112 bytes) || varBuf data, written to two disjoint
    // positions in the map, so it cannot be decompressed in-place.
    let mut partial_staging = vec![0u8; MAX_TELEMETRY_SIZE];
    let mut telemetry: Option<Telemetry> = None;
    let mut last_update = Instant::now();
    let mut stats = Stats::new("target");
    let mut seq_start: Option<Instant> = None;
    // Guard: only write partial frames once we have established a full-map
    // baseline. If we start up mid-session the source will send a full frame
    // on the next sessionInfoUpdate tick; until then we discard partial frames.
    let mut has_full_frame = false;

    loop {
        if shutdown.try_recv().is_ok() {
            stats.print_summary();
            return Ok(());
        }

        match socket.recv_from(&mut recv_buf) {
            Ok((len, _src)) => {
                let res = proto.ingest(&recv_buf[..len]);

                if res.heartbeat {
                    last_update = Instant::now();
                    continue;
                }

                if res.new_seq {
                    seq_start = Some(Instant::now());
                }

                if let Some(compressed) = res.assembled {
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

                    let t = telemetry.as_mut().unwrap();

                    if res.buf_offset == u32::MAX {
                        // Full-map frame — decompress the entire payload at offset 0.
                        if let Err(e) = decompress_into(compressed, t.as_slice_mut()) {
                            eprintln!("decompression failed (full frame): {e}");
                            continue;
                        }
                        has_full_frame = true;
                    } else if has_full_frame {
                        // Partial frame — payload is irsdk_header (112 bytes) || varBuf data.
                        // Source prepends the header so tickCounts stay current, preventing
                        // SimHub from reading the wrong varBuf slot after a ring rotation.
                        const HDR: usize = 112;
                        let off = res.buf_offset as usize;
                        let dec_len = match decompress_into(compressed, &mut partial_staging) {
                            Ok(n) => n,
                            Err(e) => {
                                eprintln!("decompression failed (partial frame): {e}");
                                continue;
                            }
                        };
                        if dec_len < HDR {
                            eprintln!("partial frame decompressed to {dec_len} bytes, expected >{HDR}");
                            continue;
                        }
                        let var_len = dec_len - HDR;
                        let map = t.as_slice_mut();
                        if map.len() < HDR || off + var_len > map.len() {
                            eprintln!("partial frame out of range (off={off} var_len={var_len}), discarding");
                            continue;
                        }
                        map[..HDR].copy_from_slice(&partial_staging[..HDR]);
                        map[off..off + var_len].copy_from_slice(&partial_staging[HDR..dec_len]);
                    } else {
                        // Partial frame arrived before any full frame — discard.
                        // Source will send a full frame on the next session-info tick.
                        continue;
                    }

                    if let Err(e) = t.signal_data_ready() {
                        eprintln!("signal_data_ready failed: {e}");
                    }

                    // Compute end-to-end latency: source processing + network transit.
                    if let Some(start) = seq_start.take() {
                        let transit_us = start.elapsed().as_micros() as u64;
                        stats.record(compressed.len(), proto.last_fragment_count, proto.last_source_us + transit_us);
                        stats.record_dropped(proto.dropped_sequences);
                        proto.dropped_sequences = 0;
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
                    // Clear IRSDK_ST_CONNECTED before unmapping so SimHub sees
                    // a clean disconnect rather than a stale status flag.
                    if let Some(t) = telemetry.as_mut() {
                        t.clear_status();
                    }
                    telemetry = None;
                    has_full_frame = false;
                }
                // In busy-wait mode the loop spins immediately; in blocking mode
                // recv_from already slept up to its 1s timeout.
            }

            Err(e) => return Err(e),
        }
    }
}
