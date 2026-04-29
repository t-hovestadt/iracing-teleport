use lz4_flex::block::{compress_into, get_maximum_output_size};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::platform::{boost_thread_priority, pin_thread_to_core, HighResTimer};
use crate::protocol::Sender;
use crate::stats::Stats;
use crate::telemetry::{IRSDK_HEADER_SIZE, MAX_TELEMETRY_SIZE, Telemetry, TelemetryError, TelemetryProvider};

const RECONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL_MS: u32 = 200;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
// Fallback interval for sending a session-info frame when the bidirectional
// resync-request mechanism can't reach the source (e.g. firewall blocking
// inbound UDP to source's ephemeral port). Normal resync is driven by target
// requests — see pending_resync. 10 s is fast enough to feel instant on first
// connect while still being negligible overhead during normal racing.
const FULL_FRAME_INTERVAL: Duration = Duration::from_secs(10);


/// Returns the byte offset where the variable data region begins —
/// `min(varBuf[i].bufOffset)` for all active buffers. This is the end of the
/// static prefix (irsdk header + var descriptors + session YAML).
/// Falls back to `data.len()` if the header is missing or malformed.
fn session_info_end(data: &[u8]) -> usize {
    if data.len() < IRSDK_HEADER_SIZE { return data.len(); }
    let num_buf = (i32::from_le_bytes(
        data[32..36].try_into().unwrap_or([0; 4])
    ) as usize).min(4);
    if num_buf == 0 { return data.len(); }
    let mut min_off = data.len();
    for i in 0..num_buf {
        let b = 48 + i * 16;
        if b + 8 > data.len() { return data.len(); }
        let off = i32::from_le_bytes(
            data[b + 4..b + 8].try_into().unwrap_or([0; 4])
        ) as usize;
        if off > IRSDK_HEADER_SIZE && off < data.len() {
            min_off = min_off.min(off);
        }
    }
    min_off
}

pub fn run(
    bind: &str,
    target: &str,
    unicast: bool,
    pin_core: Option<usize>,
    shutdown: mpsc::Receiver<()>,
) -> std::io::Result<()> {
    let _timer = HighResTimer::acquire();
    boost_thread_priority();
    if let Some(core) = pin_core {
        pin_thread_to_core(core);
    }

    // Build the socket manually so we can set the send buffer before binding.
    // A single compressed frame is ~200KB on the wire. The OS default (64KB on
    // Windows) is smaller than one frame, so send_to stalls mid-burst and adds
    // latency. 2MB holds ~9 full frames with no backpressure.
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_send_buffer_size(2 * 1024 * 1024)?;
    let bind_addr: SocketAddr = bind.parse()
        .map_err(|e| std::io::Error::other(format!("invalid bind address: {e}")))?;
    sock.bind(&bind_addr.into())?;
    let socket: UdpSocket = sock.into();
    // Non-blocking so we can poll for inbound resync requests from targets
    // without a separate thread. UDP sends with a 2 MB send buffer never block.
    socket.set_nonblocking(true)?;
    let target_addr: SocketAddr = target
        .parse()
        .map_err(|e| std::io::Error::other(format!("invalid target address: {e}")))?;
    if unicast {
        socket.connect(target_addr)?;
    }

    let send_one = |sender: &mut Sender, payload: &[u8], source_us: u64, buf_offset: u32| -> std::io::Result<u16> {
        if unicast {
            sender.send(payload, source_us, buf_offset, |d| socket.send(d).map(|_| ()))
        } else {
            sender.send(payload, source_us, buf_offset, |d| socket.send_to(d, target_addr).map(|_| ()))
        }
    };
    let send_heartbeat = |sender: &mut Sender| -> std::io::Result<()> {
        if unicast {
            sender.send_heartbeat(|d| socket.send(d).map(|_| ()))
        } else {
            sender.send_heartbeat(|d| socket.send_to(d, target_addr).map(|_| ()))
        }
    };

    println!("Waiting for iRacing to start...");
    let mut telemetry = loop {
        match try_open(&shutdown)? {
            OpenResult::Connected(t) => {
                println!("Connected to iRacing telemetry ({} bytes)", t.size());
                break t;
            }
            OpenResult::Shutdown => return Ok(()),
            OpenResult::Retry => continue,
        }
    };

    let mut sender = Sender::new();
    let mut stats = Stats::new("source");
    let mut last_data = Instant::now();
    let mut last_heartbeat = Instant::now();
    let mut compress_buf = vec![0u8; get_maximum_output_size(MAX_TELEMETRY_SIZE)];
    // Staging buffer for partial-frame payloads: irsdk header (112 bytes) prepended
    // to the active varBuf data so the target always gets current tickCounts.
    let mut partial_staging = vec![0u8; MAX_TELEMETRY_SIZE];
    // True once at least one telemetry frame has been received in the current connection.
    // Used to suppress log spam when iRacing is open but between sessions.
    let mut got_data = false;
    // Tracks the last-seen sessionInfoUpdate counter. When it changes we send a
    // full-map frame so the target's header + session YAML stay current.
    let mut last_session_update: i32 = -1;
    // Tracks when we last sent a session-info frame. Used as a fallback in case
    // the target's resync request can't reach the source (see pending_resync).
    let mut last_full_frame = Instant::now();
    // Set to true when a resync request arrives from a target; causes the next
    // data tick to send a session-info frame immediately.
    let mut pending_resync = false;

    loop {
        if shutdown.try_recv().is_ok() {
            stats.print_summary();
            return Ok(());
        }

        if !telemetry.wait_for_data(POLL_INTERVAL_MS) {
            // Check for resync requests from targets even while iRacing is idle.
            let mut tmp = [0u8; 1];
            if socket.recv_from(&mut tmp).is_ok() {
                pending_resync = true;
            }

            // Send a tiny "still here" packet so the target keeps its memory map
            // alive across menus / loading screens.
            if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
                if let Err(e) = send_heartbeat(&mut sender) {
                    eprintln!("heartbeat failed: {e}");
                }
                last_heartbeat = Instant::now();
            }

            if last_data.elapsed() >= RECONNECT_TIMEOUT {
                if got_data {
                    println!("iRacing stopped responding — waiting to reconnect...");
                }
                drop(telemetry);
                got_data = false;
                last_session_update = -1;
                last_full_frame = Instant::now() - FULL_FRAME_INTERVAL;
                telemetry = loop {
                    match try_open(&shutdown)? {
                        OpenResult::Connected(t) => break t,
                        OpenResult::Shutdown => {
                            stats.print_summary();
                            return Ok(());
                        }
                        OpenResult::Retry => continue,
                    }
                };
                last_data = Instant::now();
                stats = Stats::new("source");
            }
            continue;
        }

        if !got_data {
            println!("Session started.");
            got_data = true;
            // Force a session-info frame on the very first tick so any target
            // that was already waiting syncs immediately without needing the
            // fallback timer or a successful resync request.
            last_full_frame = Instant::now() - FULL_FRAME_INTERVAL;
        }

        last_data = Instant::now();

        // Send a full session-info frame when sessionInfoUpdate changes (new session,
        // track, or car), when target requests a resync, or every FULL_FRAME_INTERVAL
        // as a fallback. For every other tick send only the active variable buffer
        // (~5–15 KB). See the "SimHub activation invariant" block below for why
        // session-info frames must always send the complete map.
        let force_full = last_full_frame.elapsed() >= FULL_FRAME_INTERVAL || pending_resync;
        let (buf_offset, payload_slice) = {
            let data = telemetry.as_slice();
            let session_update = data
                .get(12..16)
                .and_then(|s| s.try_into().ok())
                .map(i32::from_le_bytes)
                .unwrap_or(0);

            // ── SimHub activation invariant ──────────────────────────────────────
            // SimHub detects iRacing via two independent mechanisms:
            //   1. WaitForSingleObject on Local\IRSDKDataValidEvent
            //   2. Direct polling of irsdk_header.status on its own timer
            //
            // The invariant: status=1 must only become visible AFTER varBuf data
            // is already written to the shared map.
            //
            // How this is enforced (write-ordering approach):
            //   Session-info frames: bytes [4..8] (status) are zeroed before
            //     compressing. Target copies to map skipping [4..8], so status
            //     stays 0 (or preserved at 1 for session updates — varBuf from
            //     the previous tick is still valid).
            //   Partial frames: target writes varBuf FIRST, then irsdk header
            //     LAST. The header contains status=1 from iRacing's live data;
            //     writing it after varBuf means status=1 is visible only once
            //     varBuf is already in place.
            //
            // History of failed optimisations (both wrote status=1 before varBuf):
            //   4e1a197  prefix-only session-info, no status zeroing → bc2bd98 reverted
            //   ed7af31  prefix-only + withhold SetEvent workaround  → 48a4714 reverted
            // ─────────────────────────────────────────────────────────────────
            if session_update != last_session_update || force_full {
                last_session_update = session_update;
                last_full_frame = Instant::now();
                pending_resync = false;
                (u32::MAX, 0..data.len())
            } else if let Some((off, len)) = telemetry.active_var_buf() {
                (off as u32, off..off + len)
            } else {
                last_full_frame = Instant::now();
                pending_resync = false;
                (u32::MAX, 0..data.len())
            }
        };

        let data = telemetry.as_slice();

        // Session-info frames send only the static prefix (header + var descriptors
        // + session YAML) with the status field zeroed. The target copies this to
        // the map skipping [4..8] so status stays 0 until the first partial frame
        // writes varBuf and then the header (status=1) — see invariant comment above.
        //
        // Partial frames prepend the irsdk header so the target always writes
        // current tickCounts. Without this, SimHub could read the wrong varBuf slot
        // when iRacing rotates to a new ring position.
        let payload: &[u8] = if buf_offset == u32::MAX {
            let prefix_end = session_info_end(data);
            partial_staging[..prefix_end].copy_from_slice(&data[..prefix_end]);
            partial_staging[4..8].copy_from_slice(&[0u8; 4]); // zero status — target skips [4..8]
            &partial_staging[..prefix_end]
        } else {
            let var_slice = &data[payload_slice];
            partial_staging[..IRSDK_HEADER_SIZE].copy_from_slice(&data[..IRSDK_HEADER_SIZE]);
            partial_staging[IRSDK_HEADER_SIZE..IRSDK_HEADER_SIZE + var_slice.len()].copy_from_slice(var_slice);
            &partial_staging[..IRSDK_HEADER_SIZE + var_slice.len()]
        };

        let compressed_len = match compress_into(payload, &mut compress_buf) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("compression failed: {e}");
                continue;
            }
        };
        let source_us = last_data.elapsed().as_micros() as u64;

        let is_full = buf_offset == u32::MAX;
        match send_one(&mut sender, &compress_buf[..compressed_len], source_us, buf_offset) {
            Ok(_) => stats.record(compressed_len, source_us, 0, is_full),
            Err(e) => eprintln!("send failed: {e}"),
        }
        last_heartbeat = Instant::now();

        // Poll for resync requests from targets (non-blocking).
        let mut tmp = [0u8; 1];
        if socket.recv_from(&mut tmp).is_ok() {
            pending_resync = true;
        }

        stats.maybe_print();
    }
}

enum OpenResult {
    Connected(Telemetry),
    /// iRacing not running yet — caller should retry.
    Retry,
    /// Shutdown signal received — caller should exit.
    Shutdown,
}

fn try_open(shutdown: &mpsc::Receiver<()>) -> std::io::Result<OpenResult> {
    match Telemetry::open() {
        Ok(t) => return Ok(OpenResult::Connected(t)),
        Err(TelemetryError::Unavailable) => {}
        Err(TelemetryError::Other(e)) => {
            return Err(std::io::Error::other(e.to_string()));
        }
    }

    // iRacing not running yet — wait up to 5s before retrying, but wake
    // immediately if shutdown is requested.
    match shutdown.recv_timeout(Duration::from_secs(5)) {
        Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => Ok(OpenResult::Shutdown),
        Err(mpsc::RecvTimeoutError::Timeout) => Ok(OpenResult::Retry),
    }
}
