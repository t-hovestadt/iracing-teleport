use lz4_flex::block::{decompress_into, get_maximum_output_size};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::platform::{
    boost_thread_priority, pin_thread_to_core, set_high_priority, HighResTimer, MmcssGuard,
};
use crate::protocol::{xor_delta, Receiver as ProtoReceiver, DELTA_BIT, MAX_DATAGRAM_SIZE};
use crate::stats::Stats;
use crate::telemetry::{Telemetry, TelemetryProvider, IRSDK_HEADER_SIZE, MAX_TELEMETRY_SIZE};

pub const DEFAULT_STALE_TIMEOUT_SECS: u64 = 10;
// How often target retries a resync request to source when has_full_frame is false.
const RESYNC_REQUEST_INTERVAL: Duration = Duration::from_secs(1);

/// Spawns a sleeping copy of this executable named `iRacingSim64DX11.exe` in the
/// system temp directory so FanaLab finds the expected process and auto-loads car
/// profiles. Killed automatically when dropped.
struct FanalabStub(std::process::Child);

impl FanalabStub {
    fn spawn() -> Option<Self> {
        let own_path = std::env::current_exe().ok()?;
        let stub_path = std::env::temp_dir().join("iRacingSim64DX11.exe");
        // Write to a temp name first, then rename — atomic on NTFS within a single
        // volume. Avoids leaving a half-written exe if we crash mid-copy, which
        // would prevent the stub from spawning on the next run.
        let tmp_path = std::env::temp_dir().join("iRacingSim64DX11_new.exe");
        if std::fs::copy(&own_path, &tmp_path).is_ok() {
            if std::fs::rename(&tmp_path, &stub_path).is_err() {
                // Rename failed (destination locked by a prior crash). Clean up the
                // temp file and fall back to whatever is already at stub_path.
                let _ = std::fs::remove_file(&tmp_path);
                if !stub_path.exists() {
                    eprintln!("fanalab: could not create stub");
                    return None;
                }
            }
        } else if !stub_path.exists() {
            eprintln!("fanalab: could not create stub");
            return None;
        }
        match std::process::Command::new(&stub_path)
            .arg("--fanalab-stub")
            .spawn()
        {
            Ok(child) => {
                println!(
                    "FanaLab compat: iRacingSim64DX11.exe running (pid {})",
                    child.id()
                );
                Some(Self(child))
            }
            Err(e) => {
                eprintln!("fanalab: failed to spawn stub: {e}");
                None
            }
        }
    }
}

impl Drop for FanalabStub {
    fn drop(&mut self) {
        let _ = self.0.kill();
        println!("FanaLab compat: iRacingSim64DX11.exe stopped");
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    bind: &str,
    unicast: bool,
    multicast_group: &str,
    busy_wait: bool,
    pin_core: Option<usize>,
    fanalab: bool,
    stale_timeout_secs: u64,
    high_priority: bool,
    shutdown: mpsc::Receiver<()>,
) -> std::io::Result<()> {
    let stale_timeout = Duration::from_secs(stale_timeout_secs);
    let _timer = HighResTimer::acquire();
    boost_thread_priority();
    if high_priority {
        set_high_priority();
    }
    let _mmcss = MmcssGuard::acquire();
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
    let bind_addr: SocketAddr = bind
        .parse()
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
    // XOR-delta state: prev_varbuf holds the last partial frame written
    // (irsdk_header prepended); reconstruct_buf is the XOR workspace.
    // Zeroed on session-info frames and stale timeout to stay in sync with source.
    let mut prev_varbuf = vec![0u8; MAX_TELEMETRY_SIZE];
    let mut reconstruct_buf = vec![0u8; MAX_TELEMETRY_SIZE];
    let mut telemetry: Option<Telemetry> = None;
    let mut last_update = Instant::now();
    let mut stats = Stats::new("target");
    let mut seq_start: Option<Instant> = None;
    // Guard: only write partial frames once we have received a session-info frame.
    let mut has_full_frame = false;
    // Source address learned from recv_from; used to send resync requests.
    // Updated on every recv so resync requests always go to the current source port.
    #[allow(unused_assignments)] // initial None is intentional; first recv_from overwrites it
    let mut source_addr: Option<std::net::SocketAddr> = None;
    let mut last_resync_request = Instant::now() - RESYNC_REQUEST_INTERVAL;
    // FanaLab compat: dummy process that makes FanaLab think iRacing is running.
    let mut fanalab_stub: Option<FanalabStub> = None;

    loop {
        if shutdown.try_recv().is_ok() {
            drop(fanalab_stub.take());
            stats.print_summary();
            return Ok(());
        }

        match socket.recv_from(&mut recv_buf) {
            Ok((len, src)) => {
                // Always update so resync requests go to the current source port.
                // If source restarts on a different ephemeral port, keeping the old
                // address would delay resync by up to stale_timeout seconds.
                source_addr = Some(src);
                let res = proto.ingest(&recv_buf[..len]);

                if res.heartbeat {
                    last_update = Instant::now();
                } else {
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

                        let Some(t) = telemetry.as_mut() else {
                            continue;
                        };
                        let mut wrote = false;
                        let mut dec_len_out = 0usize;

                        let is_delta =
                            res.buf_offset != u32::MAX && (res.buf_offset & DELTA_BIT != 0);

                        if res.buf_offset == u32::MAX {
                            // Session-info frame: decompress prefix into staging, then copy to
                            // map SKIPPING bytes [4..8] (the status field). Preserving status
                            // at 0 (fresh map) or 1 (ongoing session) means SimHub can never
                            // see status=1 from a session-info frame before varBuf is populated.
                            // status=1 is written by the partial frame handler below, after varBuf.
                            // lz4_flex with the `checked-decode` feature returns
                            // Err (never panics or writes past the buffer end) if
                            // the output would exceed partial_staging.len().
                            let prefix_len = match decompress_into(compressed, &mut partial_staging)
                            {
                                Ok(n) => n,
                                Err(e) => {
                                    eprintln!("decompression failed (session-info frame): {e}");
                                    continue;
                                }
                            };
                            dec_len_out = prefix_len;
                            if prefix_len < 8 {
                                eprintln!(
                                    "session-info frame too short ({prefix_len} bytes), discarding"
                                );
                                continue;
                            }
                            let map = t.as_slice_mut();
                            let n = prefix_len.min(map.len());
                            map[..4].copy_from_slice(&partial_staging[..4]); // pre-status bytes
                                                                             // [4..8] intentionally skipped — preserve existing status value
                            if n > 8 {
                                map[8..n].copy_from_slice(&partial_staging[8..n]);
                            }
                            has_full_frame = true;
                            // Reset delta state: source will send a keyframe next.
                            prev_varbuf.fill(0);
                            wrote = true;
                            // Spawn FanaLab compat stub on the first session-info frame so
                            // FanaLab sees iRacingSim64DX11.exe and loads car profiles.
                            if fanalab && fanalab_stub.is_none() {
                                fanalab_stub = FanalabStub::spawn();
                            }
                        } else if has_full_frame {
                            // Partial frame — payload is irsdk_header || varBuf data.
                            // Write order: varBuf FIRST, then irsdk header LAST.
                            // The header contains status=1 from iRacing's live data; writing it
                            // after varBuf means status=1 becomes visible only once varBuf is
                            // already in place — see source.rs "SimHub activation invariant".
                            let real_off = (res.buf_offset & !DELTA_BIT) as usize;
                            // lz4_flex with the `checked-decode` feature returns
                            // Err (never panics or writes past the buffer end) if
                            // the output would exceed partial_staging.len().
                            let dec_len = match decompress_into(compressed, &mut partial_staging) {
                                Ok(n) => n,
                                Err(e) => {
                                    eprintln!(
                                        "decompression failed ({} frame): {e}",
                                        if is_delta { "delta" } else { "partial" }
                                    );
                                    continue;
                                }
                            };
                            dec_len_out = dec_len;
                            if dec_len < IRSDK_HEADER_SIZE {
                                eprintln!("partial frame decompressed to {dec_len} bytes, expected >{IRSDK_HEADER_SIZE}");
                                continue;
                            }

                            // For delta frames: reconstruct current = delta XOR prev_varbuf.
                            let frame_data: &[u8] = if is_delta {
                                if dec_len > prev_varbuf.len() {
                                    eprintln!(
                                        "delta frame too large ({dec_len} bytes), discarding"
                                    );
                                    continue;
                                }
                                xor_delta(
                                    &partial_staging[..dec_len],
                                    &prev_varbuf[..dec_len],
                                    &mut reconstruct_buf[..dec_len],
                                );
                                &reconstruct_buf[..dec_len]
                            } else {
                                &partial_staging[..dec_len]
                            };

                            let var_len = dec_len - IRSDK_HEADER_SIZE;
                            let map = t.as_slice_mut();
                            if map.len() < IRSDK_HEADER_SIZE || real_off + var_len > map.len() {
                                eprintln!("partial frame out of range (off={real_off} var_len={var_len}), discarding");
                                continue;
                            }
                            // Update prev_varbuf only after bounds check passes, so it stays
                            // aligned with what we actually wrote to the map. If we updated it
                            // before the check and then discarded the frame, prev_varbuf and the
                            // map would diverge — correct bytes but wrong frame in the map.
                            prev_varbuf[..dec_len].copy_from_slice(frame_data);
                            // varBuf written first — status still 0 (fresh) or 1 (ongoing).
                            map[real_off..real_off + var_len]
                                .copy_from_slice(&frame_data[IRSDK_HEADER_SIZE..dec_len]);
                            // Header written last — sets status=1 only after varBuf is in place.
                            map[..IRSDK_HEADER_SIZE]
                                .copy_from_slice(&frame_data[..IRSDK_HEADER_SIZE]);
                            wrote = true;
                        }
                        // else: partial before session-info — fall through to resync request below.

                        if wrote {
                            // Signal on every write. For partial frames this fires after
                            // status=1 is written (header last), so SimHub's WaitForSingleObject
                            // path also sees a fully-populated map.
                            // See source.rs "SimHub activation invariant" for full analysis.
                            if let Err(e) = t.signal_data_ready() {
                                eprintln!("signal_data_ready failed: {e}");
                            }

                            // Compute end-to-end latency: source processing + network transit.
                            if let Some(start) = seq_start.take() {
                                let transit_us = start.elapsed().as_micros() as u64;
                                let is_full = res.buf_offset == u32::MAX;
                                stats.record(
                                    compressed.len(),
                                    dec_len_out,
                                    proto.last_source_us,
                                    transit_us,
                                    is_full,
                                    is_delta,
                                );
                                stats.record_dropped(proto.dropped_sequences);
                                proto.dropped_sequences = 0;
                            }

                            last_update = Instant::now();
                            stats.maybe_print();
                        }
                    }
                }

                // When we don't have a session-info frame yet, ask the source for one.
                // Rate-limited to once per second; retries ensure delivery even if a
                // request is lost.
                if !has_full_frame {
                    if let Some(addr) = source_addr {
                        if last_resync_request.elapsed() >= RESYNC_REQUEST_INTERVAL {
                            // Byte 0: resync request (0x01).
                            // Byte 1: capability bitfield — bit 0 = delta-capable.
                            // Old sources ignore the second byte and send a session-info frame.
                            let _ = socket.send_to(&[0x01, 0x01], addr);
                            last_resync_request = Instant::now();
                        }
                    }
                }
            }

            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Drop the telemetry map when we haven't heard from the source for a while.
                if telemetry.is_some() && last_update.elapsed() >= stale_timeout {
                    println!(
                        "No data for {}s, closing telemetry map.",
                        stale_timeout.as_secs()
                    );
                    // Clear IRSDK_ST_CONNECTED before unmapping so SimHub sees
                    // a clean disconnect rather than a stale status flag.
                    if let Some(t) = telemetry.as_mut() {
                        t.clear_status();
                    }
                    telemetry = None;
                    has_full_frame = false;
                    // Reset delta state: source will have reset its own state; zero
                    // our prev_varbuf so we accept the next keyframe cleanly.
                    prev_varbuf.fill(0);
                    // source_addr is updated on every recv_from, so no need to clear it
                    // here — the next incoming packet will set it correctly regardless.
                    fanalab_stub = None;
                }
                // In busy-wait mode the loop spins immediately; in blocking mode
                // recv_from already slept up to its 1s timeout.
            }

            Err(e) => return Err(e),
        }
    }
}
