use lz4_flex::block::{compress_into, get_maximum_output_size};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::protocol::Sender;
use crate::stats::Stats;
use crate::telemetry::{MAX_TELEMETRY_SIZE, Telemetry, TelemetryError, TelemetryProvider};

const RECONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL_MS: u32 = 200;

pub fn run(
    bind: &str,
    target: &str,
    unicast: bool,
    shutdown: mpsc::Receiver<()>,
) -> std::io::Result<()> {
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
    let mut last_data = Instant::now();
    let mut compress_buf = vec![0u8; get_maximum_output_size(MAX_TELEMETRY_SIZE)];

    loop {
        if shutdown.try_recv().is_ok() {
            return Ok(());
        }

        if !telemetry.wait_for_data(POLL_INTERVAL_MS) {
            if last_data.elapsed() >= RECONNECT_TIMEOUT {
                println!("iRacing stopped responding — waiting to reconnect...");
                drop(telemetry);
                telemetry = loop {
                    match try_open(&shutdown)? {
                        OpenResult::Connected(t) => break t,
                        OpenResult::Shutdown => return Ok(()),
                        OpenResult::Retry => continue,
                    }
                };
                last_data = Instant::now();
                println!("Reconnected.");
            }
            continue;
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
            sender.send(payload, source_us, |d| socket.send_to(d, target_addr).map(|_| ()))
        };

        match result {
            Ok(frags) => stats.record(compressed_len, frags, source_us),
            Err(e) => eprintln!("send failed: {e}"),
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

