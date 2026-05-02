use lz4_flex::block::{compress_into, get_maximum_output_size};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::platform::{boost_thread_priority, pin_thread_to_core, set_high_priority, HighResTimer};
use crate::protocol::{xor_delta, Sender, DELTA_BIT};
use crate::stats::Stats;
use crate::telemetry::{
    Telemetry, TelemetryError, TelemetryProvider, IRSDK_HEADER_SIZE, MAX_TELEMETRY_SIZE,
};

pub const DEFAULT_RECONNECT_TIMEOUT_SECS: u64 = 10;
/// Default UDP datagram size for source — matches `protocol::MAX_DATAGRAM_SIZE`.
/// Expose so CLI binaries can use it as the default value for `--datagram-size`.
pub use crate::protocol::MAX_DATAGRAM_SIZE as DEFAULT_DATAGRAM_SIZE;
/// Default number of partial frames between full (non-delta) keyframes.
pub const DEFAULT_KEYFRAME_INTERVAL: u16 = 60;
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
    if data.len() < IRSDK_HEADER_SIZE {
        return data.len();
    }
    let num_buf = (i32::from_le_bytes(data[32..36].try_into().unwrap_or([0; 4])) as usize).min(4);
    if num_buf == 0 {
        return data.len();
    }
    let mut min_off = data.len();
    for i in 0..num_buf {
        let b = 48 + i * 16;
        if b + 8 > data.len() {
            return data.len();
        }
        let off = i32::from_le_bytes(data[b + 4..b + 8].try_into().unwrap_or([0; 4])) as usize;
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
    busy_wait: bool,
    pin_core: Option<usize>,
    high_priority: bool,
    reconnect_timeout_secs: u64,
    datagram_size: usize,
    no_delta: bool,
    keyframe_interval: u16,
    shutdown: mpsc::Receiver<()>,
) -> std::io::Result<()> {
    let reconnect_timeout = Duration::from_secs(reconnect_timeout_secs);
    let _timer = HighResTimer::acquire();
    boost_thread_priority();
    if high_priority {
        set_high_priority();
    }
    if let Some(core) = pin_core {
        pin_thread_to_core(core);
    }
    // In busy-wait mode the main loop spins on WaitForSingleObject(0) — no OS
    // scheduler wake-up jitter, but burns one CPU core.  On the iRacing PC this
    // competes with iRacing; only use if you have spare cores.
    let poll_ms = if busy_wait { 0 } else { POLL_INTERVAL_MS };
    if busy_wait {
        println!("Busy-wait mode: source thread will burn one CPU core for lower latency.");
    }
    // Clamp to a sensible range: at least 64 bytes (header + minimal payload)
    // and at most 65507 bytes (max UDP payload on IPv4).
    let datagram_size = datagram_size.clamp(64, 65_507);
    if datagram_size != crate::protocol::MAX_DATAGRAM_SIZE {
        println!("Datagram size: {datagram_size} bytes per fragment.");
    }

    // Build the socket manually so we can set the send buffer before binding.
    // A single compressed frame is ~200KB on the wire. The OS default (64KB on
    // Windows) is smaller than one frame, so send_to stalls mid-burst and adds
    // latency. 2MB holds ~9 full frames with no backpressure.
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_send_buffer_size(2 * 1024 * 1024)?;
    let bind_addr: SocketAddr = bind
        .parse()
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

    let send_one = |sender: &mut Sender,
                    payload: &[u8],
                    source_us: u64,
                    buf_offset: u32|
     -> std::io::Result<u16> {
        if unicast {
            sender.send(payload, source_us, buf_offset, |d| {
                socket.send(d).map(|_| ())
            })
        } else {
            sender.send(payload, source_us, buf_offset, |d| {
                socket.send_to(d, target_addr).map(|_| ())
            })
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

    let mut sender = Sender::with_datagram_size(datagram_size);
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
    // XOR-delta state. prev_varbuf holds the last partial frame sent (irsdk_header
    // prepended); delta_buf is the XOR workspace. Both are zeroed initially — a
    // zeroed prev means the delta == current, which the target reconstructs
    // correctly even before it receives its first keyframe.
    let mut prev_varbuf = vec![0u8; MAX_TELEMETRY_SIZE];
    let mut delta_buf = vec![0u8; MAX_TELEMETRY_SIZE];
    // True once a capability packet from the target confirms it speaks the delta
    // protocol. Stays false until the first resync packet with byte[1] & 0x01 set.
    let mut target_supports_delta = false;
    // Counts partial frames sent; resets to 0 after each session-info frame so
    // the first partial frame after a session-info is always a keyframe.
    let mut tick_counter: u32 = 0;
    // Last varBuf tickCount successfully sent. -1 = none yet. Duplicate ticks
    // (iRacing signalled but didn't advance the counter, e.g. during loading
    // screens or sub-60 Hz operation) are skipped to avoid redundant
    // compress+send. Reset alongside tick_counter on reconnect and session change.
    let mut last_tick: i32 = -1;

    loop {
        if shutdown.try_recv().is_ok() {
            stats.print_summary();
            return Ok(());
        }

        if !telemetry.wait_for_data(poll_ms) {
            // In busy-wait mode this branch fires on every spin iteration (poll_ms=0
            // returns immediately when no new data). Housekeeping (resync recv,
            // heartbeat) is self-throttled by the elapsed-time guards below, so it
            // only runs at the normal rate regardless of how fast the loop spins.
            // Check for resync/capability packets from targets even while iRacing is idle.
            let mut cap_buf = [0u8; 2];
            if socket.recv_from(&mut cap_buf).is_ok() {
                pending_resync = true;
                if !no_delta {
                    // Byte 0: resync flag (0x01). Byte 1: capability bitfield, bit 0 = delta.
                    // Old 1-byte targets leave byte 1 as 0 — falls through to false.
                    target_supports_delta = cap_buf.get(1).map(|&b| b & 0x01 != 0).unwrap_or(false);
                }
            }

            // Send a tiny "still here" packet so the target keeps its memory map
            // alive across menus / loading screens.
            if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
                if let Err(e) = send_heartbeat(&mut sender) {
                    eprintln!("heartbeat failed: {e}");
                }
                last_heartbeat = Instant::now();
            }

            if last_data.elapsed() >= reconnect_timeout {
                if got_data {
                    println!("iRacing stopped responding — waiting to reconnect...");
                }
                drop(telemetry);
                got_data = false;
                last_session_update = -1;
                last_full_frame = Instant::now() - FULL_FRAME_INTERVAL;
                // Reset delta state: target_supports_delta until we hear a new
                // capability packet; tick_counter so the first partial frame after
                // reconnect is a keyframe; last_tick so the first varBuf is always sent.
                target_supports_delta = false;
                tick_counter = 0;
                last_tick = -1;
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
            if busy_wait {
                // Emit a spin-loop hint (PAUSE instruction) — reduces power
                // consumption and prevents the CPU from mis-predicting branch
                // patterns in the spin loop, without introducing any sleep.
                std::hint::spin_loop();
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
        // `data` is a live pointer into iRacing's memory-mapped region — NOT a
        // snapshot. iRacing writes concurrently. For partial frames we re-check
        // tickCount after copying to detect torn reads (TOCTOU guard below).
        let data = telemetry.as_slice();
        // tick_field_off: byte offset in the header of the active slot's tickCount.
        // Set to 0 for session-info frames (static prefix, no ring-buffer race).
        let (buf_offset, payload_slice, tick_before, tick_field_off) = {
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
                // tick_field_off = 0: session-info uses a static prefix; no TOCTOU check.
                (u32::MAX, 0..data.len(), 0i32, 0usize)
            } else if let Some((off, len, tick, tick_off)) = telemetry.active_var_buf() {
                (off as u32, off..off + len, tick, tick_off)
            } else {
                last_full_frame = Instant::now();
                pending_resync = false;
                (u32::MAX, 0..data.len(), 0i32, 0usize)
            }
        };

        // Session-info frames send only the static prefix (header + var descriptors
        // + session YAML) with the status field zeroed. The target copies this to
        // the map skipping [4..8] so status stays 0 until the first partial frame
        // writes varBuf and then the header (status=1) — see invariant comment above.
        //
        // Partial frames: if delta is enabled and the target supports it, XOR the
        // current payload against the previous one and set DELTA_BIT in buf_offset.
        // Every keyframe_interval ticks (and whenever buf_offset==u32::MAX) a full
        // keyframe is sent to prevent divergence from cumulative delta errors.
        let (payload, is_delta, wire_buf_offset): (&[u8], bool, u32) = if buf_offset == u32::MAX {
            let prefix_end = session_info_end(data);
            partial_staging[..prefix_end].copy_from_slice(&data[..prefix_end]);
            partial_staging[4..8].copy_from_slice(&[0u8; 4]); // zero status — target skips [4..8]
                                                              // Reset counters: next partial frame after this session-info is a keyframe,
                                                              // and last_tick is invalid because the new session's tickCount sequence restarts.
            tick_counter = 0;
            last_tick = -1;
            (&partial_staging[..prefix_end], false, buf_offset)
        } else {
            let var_slice = &data[payload_slice];
            let full_len = IRSDK_HEADER_SIZE + var_slice.len();
            // Bounds guard: partial_staging is MAX_TELEMETRY_SIZE bytes. A malformed
            // or unexpectedly large iRacing varBuf must not cause a panic on copy.
            if full_len > partial_staging.len() {
                eprintln!(
                    "partial frame too large ({} + {} = {} bytes, max {}), skipping",
                    IRSDK_HEADER_SIZE,
                    var_slice.len(),
                    full_len,
                    partial_staging.len()
                );
                continue;
            }
            partial_staging[..IRSDK_HEADER_SIZE].copy_from_slice(&data[..IRSDK_HEADER_SIZE]);
            partial_staging[IRSDK_HEADER_SIZE..full_len].copy_from_slice(var_slice);

            // TOCTOU guard: `data` is a live pointer into iRacing's shared memory.
            // If iRacing advanced tickCount while we were copying, the frame is torn.
            // Re-read the tick field of the slot we selected; if it changed, skip.
            let tick_after = data
                .get(tick_field_off..tick_field_off + 4)
                .and_then(|s| s.try_into().ok())
                .map(i32::from_le_bytes)
                .unwrap_or(tick_before);
            if tick_after != tick_before {
                stats.record_dropped(1);
                continue;
            }

            // Duplicate-tick guard: iRacing signalled the data-ready event but the
            // varBuf slot's tickCount hasn't advanced since the last frame we sent.
            // Happens during loading screens and sub-60 Hz operation. Skip compress+
            // send to avoid redundant network traffic.
            if tick_before == last_tick {
                continue;
            }
            last_tick = tick_before;

            let use_delta = !no_delta
                && target_supports_delta
                && keyframe_interval > 0
                && tick_counter % keyframe_interval as u32 != 0;

            if use_delta {
                xor_delta(
                    &partial_staging[..full_len],
                    &prev_varbuf[..full_len],
                    &mut delta_buf[..full_len],
                );
                prev_varbuf[..full_len].copy_from_slice(&partial_staging[..full_len]);
                tick_counter = tick_counter.wrapping_add(1);
                (&delta_buf[..full_len], true, buf_offset | DELTA_BIT)
            } else {
                prev_varbuf[..full_len].copy_from_slice(&partial_staging[..full_len]);
                tick_counter = tick_counter.wrapping_add(1);
                (&partial_staging[..full_len], false, buf_offset)
            }
        };

        let compressed_len = match compress_into(payload, &mut compress_buf) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("compression failed: {e}");
                continue;
            }
        };
        // Measured pre-send: this is what goes in the wire header so the target can
        // compute end-to-end latency (source compress time + network transit).
        let source_us = last_data.elapsed().as_micros() as u64;

        let is_full = buf_offset == u32::MAX;
        match send_one(
            &mut sender,
            &compress_buf[..compressed_len],
            source_us,
            wire_buf_offset,
        ) {
            Ok(_) => {
                // Reuse the pre-send timestamp for stats: send itself is sub-µs
                // with the 2 MB socket buffer, so the difference from actual
                // post-send time is negligible. Saves one QPC syscall per tick.
                stats.record(
                    compressed_len,
                    payload.len(),
                    source_us,
                    0,
                    is_full,
                    is_delta,
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Send buffer full — silently count as dropped; shown in the 5-s stats
                // window. Very rare with the 2 MB send buffer but better than printing
                // every occurrence or losing the drop silently.
                stats.record_dropped(1);
            }
            Err(e) => eprintln!("send failed: {e}"),
        }
        // last_data was just set — no need for a fresh Instant::now() syscall here.
        last_heartbeat = last_data;

        // Poll for resync/capability packets from targets (non-blocking).
        let mut cap_buf = [0u8; 2];
        if socket.recv_from(&mut cap_buf).is_ok() {
            pending_resync = true;
            if !no_delta {
                target_supports_delta = cap_buf.get(1).map(|&b| b & 0x01 != 0).unwrap_or(false);
            }
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
