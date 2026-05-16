use lz4_flex::block::{compress_into, get_maximum_output_size};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::protocol::Sender;
use crate::stats::Stats;
use crate::telemetry::{Telemetry, TelemetryError, TelemetryProvider, MAX_TELEMETRY_SIZE};

const RECONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Hybrid poll interval: wait up to 250 ms for the iRacing event, then fall
/// back to checking the tick counter (Fix 1).
const POLL_INTERVAL_MS: u32 = 250;
/// Re-send the last frame as a keepalive if no new data arrives for this long.
/// Prevents the target from closing its shared-memory map during an event gap
/// (Fix 2).
const KEEPALIVE_AFTER: Duration = Duration::from_secs(1);
/// Log a warning when a gap of this duration is detected or resolved (Fix 4).
const GAP_LOG_THRESHOLD: Duration = Duration::from_secs(1);

pub fn run(
    bind: &str,
    target: &str,
    unicast: bool,
    busy_wait: bool,
    shutdown: mpsc::Receiver<()>,
) -> std::io::Result<()> {
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
    let target_addr: SocketAddr = target
        .parse()
        .map_err(|e| std::io::Error::other(format!("invalid target address: {e}")))?;
    if unicast {
        socket.connect(target_addr)?;
    }

    println!("Waiting for iRacing to start...");
    let mut telemetry = loop {
        match try_open(&shutdown)? {
            OpenResult::Connected(t) => break t,
            OpenResult::Shutdown => return Ok(()),
            OpenResult::Retry => continue,
        }
    };

    let mut sender = Sender::new();
    let mut stats = Stats::new("source");
    let mut compress_buf = vec![0u8; get_maximum_output_size(MAX_TELEMETRY_SIZE)];

    // Track when we last received a real iRacing frame (for reconnect + gap
    // logging).
    let mut last_data = Instant::now();

    // Fix 1: seed the tick counter so the first hybrid check has a baseline.
    let mut last_tick: i32 = telemetry.read_tick_counter();

    // Fix 2: keepalive state — last time we sent anything, plus the payload
    // to re-send during a gap.
    let mut last_sent = Instant::now();
    let mut keepalive_payload: Option<Vec<u8>> = None;

    loop {
        if shutdown.try_recv().is_ok() {
            return Ok(());
        }

        // Wait for iRacing to signal the data-ready event.
        let event_fired = if busy_wait {
            telemetry.wait_for_data(0)
        } else {
            telemetry.wait_for_data(POLL_INTERVAL_MS)
        };

        // Fix 1: Hybrid fallback — if the event didn't fire, check whether
        // the tick counter advanced.  iRacing increments it every frame even
        // when the event handle is stuck (e.g. held by the Fanatec App).
        let current_tick = telemetry.read_tick_counter();
        let tick_advanced = current_tick != last_tick;
        last_tick = current_tick;

        let got_data = event_fired || tick_advanced;

        if !got_data {
            // Fix 2: Keepalive — re-send the last frame so the target doesn't
            // close its map during an event gap.
            if let Some(ref payload) = keepalive_payload {
                if last_sent.elapsed() >= KEEPALIVE_AFTER {
                    let gap_ms = last_data.elapsed().as_millis();
                    eprintln!("[source] no new data for {gap_ms}ms — sending keepalive");
                    let result = if unicast {
                        sender.send(payload, 0, |d| socket.send(d).map(|_| ()))
                    } else {
                        sender.send(payload, 0, |d| socket.send_to(d, target_addr).map(|_| ()))
                    };
                    if let Err(e) = result {
                        eprintln!("keepalive send failed: {e}");
                    }
                    last_sent = Instant::now();
                }
            }

            if last_data.elapsed() >= RECONNECT_TIMEOUT {
                println!("iRacing stopped responding — waiting to reconnect...");
                drop(telemetry);
                keepalive_payload = None;
                telemetry = loop {
                    match try_open(&shutdown)? {
                        OpenResult::Connected(t) => break t,
                        OpenResult::Shutdown => return Ok(()),
                        OpenResult::Retry => continue,
                    }
                };
                last_data = Instant::now();
                last_tick = telemetry.read_tick_counter();
                println!("Reconnected.");
            }
            continue;
        }

        // Fix 4: Log when a significant gap is resolved so operators can
        // correlate disconnects with external events.
        let gap = last_data.elapsed();
        if gap >= GAP_LOG_THRESHOLD {
            eprintln!(
                "[source] gap {}ms resolved (event={event_fired}, tick_fallback={})",
                gap.as_millis(),
                !event_fired && tick_advanced,
            );
        }

        last_data = Instant::now();

        let compressed_len = match compress_into(telemetry.as_slice(), &mut compress_buf) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("compression failed: {e}");
                continue;
            }
        };
        let source_us = last_data.elapsed().as_micros() as u64;

        let payload = &compress_buf[..compressed_len];
        let result = if unicast {
            sender.send(payload, source_us, |d| socket.send(d).map(|_| ()))
        } else {
            sender.send(payload, source_us, |d| {
                socket.send_to(d, target_addr).map(|_| ())
            })
        };

        match result {
            Ok(frags) => stats.record(compressed_len, frags, source_us),
            Err(e) => eprintln!("send failed: {e}"),
        }

        // Fix 2: Save payload for keepalive re-sends during future gaps.
        keepalive_payload = Some(payload.to_vec());
        last_sent = Instant::now();

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
        Ok(t) => {
            println!("Connected to iRacing telemetry ({} bytes)", t.size());
            return Ok(OpenResult::Connected(t));
        }
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
