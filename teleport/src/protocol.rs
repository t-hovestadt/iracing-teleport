use std::io;

/// Max UDP datagram size — stays under typical jumbo-frame MTU with headroom for IP/UDP headers.
pub const MAX_DATAGRAM_SIZE: usize = 9_000;

const HEADER_SIZE: usize = std::mem::size_of::<Header>();
pub const MAX_PAYLOAD_PER_DATAGRAM: usize = MAX_DATAGRAM_SIZE - HEADER_SIZE;

/// Wire header prepended to every UDP datagram — 24 bytes, no padding.
///
/// Fields are ordered largest-to-smallest alignment so `repr(C, packed)`
/// produces exactly the same layout as `repr(C)` would with natural alignment.
///
/// Layout (all little-endian):
///   source_us    u64   microseconds spent on the source side (for latency stats)
///   sequence     u32   monotonically increasing per full message
///   payload_size u32   total compressed payload bytes across all fragments
///   buf_offset   u32   byte offset to write decompressed data into the target map;
///                      u32::MAX signals a full-map frame (write at offset 0)
///   fragment     u16   0-based index of this fragment
///   fragments    u16   total fragment count for this sequence; 0 = heartbeat
#[repr(C, packed)]
struct Header {
    source_us: u64,
    sequence: u32,
    payload_size: u32,
    buf_offset: u32,
    fragment: u16,
    fragments: u16,
}

const _: () = assert!(std::mem::size_of::<Header>() == 24);
const _: () = assert!(MAX_PAYLOAD_PER_DATAGRAM > 0);

// Maximum fragments per sequence. A 1.1 MB session-info frame compresses to
// ~300 KB, requiring at most ~34 × 8976-byte fragments. 256 is a generous cap
// that still prevents OOM from a malformed packet claiming thousands of fragments.
const MAX_FRAGMENTS: u16 = 256;

// ── Sender ────────────────────────────────────────────────────────────────────

pub struct Sender {
    sequence: u32,
    buf: Vec<u8>,
}

impl Default for Sender {
    fn default() -> Self {
        Self::new()
    }
}

impl Sender {
    pub fn new() -> Self {
        Self {
            sequence: 0,
            buf: vec![0u8; MAX_DATAGRAM_SIZE],
        }
    }

    /// Fragment `data` and call `send_fn` once per datagram.
    /// `buf_offset` is written verbatim into every fragment header so the
    /// receiver knows where to write the decompressed bytes (`u32::MAX` = full
    /// frame, any other value = partial varBuf frame).
    /// Returns the number of fragments sent.
    pub fn send<F>(&mut self, data: &[u8], source_us: u64, buf_offset: u32, mut send_fn: F) -> io::Result<u16>
    where
        F: FnMut(&[u8]) -> io::Result<()>,
    {
        let total = data.len();
        let n_fragments = total.div_ceil(MAX_PAYLOAD_PER_DATAGRAM);
        if n_fragments == 0 || n_fragments > u16::MAX as usize {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "payload too large or empty"));
        }

        for i in 0..n_fragments {
            let offset = i * MAX_PAYLOAD_PER_DATAGRAM;
            let chunk = &data[offset..(offset + MAX_PAYLOAD_PER_DATAGRAM).min(total)];

            let hdr = Header {
                sequence: self.sequence,
                fragment: i as u16,
                fragments: n_fragments as u16,
                payload_size: total as u32,
                buf_offset,
                source_us,
            };

            // Safety: `hdr` is a local stack variable (properly aligned). Viewing it as
            // bytes is always valid; we read every byte before `hdr` is dropped.
            let hdr_bytes =
                unsafe { std::slice::from_raw_parts(&hdr as *const _ as *const u8, HEADER_SIZE) };

            self.buf[..HEADER_SIZE].copy_from_slice(hdr_bytes);
            self.buf[HEADER_SIZE..HEADER_SIZE + chunk.len()].copy_from_slice(chunk);
            send_fn(&self.buf[..HEADER_SIZE + chunk.len()])?;
        }

        self.sequence = self.sequence.wrapping_add(1);
        Ok(n_fragments as u16)
    }

    /// Send a single header-only "still here" datagram so the receiver can keep
    /// its telemetry map alive while iRacing is open but between sessions.
    /// Distinguished on the wire by `fragments == 0`.
    pub fn send_heartbeat<F>(&mut self, mut send_fn: F) -> io::Result<()>
    where
        F: FnMut(&[u8]) -> io::Result<()>,
    {
        let hdr = Header {
            sequence: 0,
            fragment: 0,
            fragments: 0,
            payload_size: 0,
            buf_offset: 0,
            source_us: 0,
        };
        let hdr_bytes =
            unsafe { std::slice::from_raw_parts(&hdr as *const _ as *const u8, HEADER_SIZE) };
        self.buf[..HEADER_SIZE].copy_from_slice(hdr_bytes);
        send_fn(&self.buf[..HEADER_SIZE])
    }
}

// ── Receiver ──────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct Ingested<'a> {
    /// Full reassembled compressed payload, present once a sequence completes.
    pub assembled: Option<&'a [u8]>,
    /// True when the datagram was the first fragment of a new sequence.
    /// Used by the target to start the network-transit timer.
    pub new_seq: bool,
    /// True when the datagram is a sender heartbeat (no payload).
    pub heartbeat: bool,
    /// Where to write the decompressed payload in the target telemetry map.
    /// `u32::MAX` means write at offset 0 (full frame). Meaningless if
    /// `assembled` is `None` or `heartbeat` is `true`.
    pub buf_offset: u32,
}

pub struct Receiver {
    buf: Vec<u8>,
    received: Vec<bool>,
    max_payload: usize,
    current_seq: Option<u32>,
    total_frags: u16,
    got_frags: u16,
    payload_size: usize,
    pub last_source_us: u64,
    pub last_fragment_count: u16,
    /// Number of sequences abandoned mid-reassembly due to a new sequence arriving.
    /// Each increment means at least one UDP packet was lost.
    pub dropped_sequences: u64,
    last_buf_offset: u32,
}

impl Receiver {
    pub fn new(max_payload: usize) -> Self {
        Self {
            buf: Vec::with_capacity(max_payload),
            received: Vec::new(),
            max_payload,
            current_seq: None,
            total_frags: 0,
            got_frags: 0,
            payload_size: 0,
            last_source_us: 0,
            last_fragment_count: 0,
            dropped_sequences: 0,
            last_buf_offset: u32::MAX,
        }
    }

    /// Feed a raw UDP datagram. Returns flags describing what happened plus
    /// the reassembled payload when a sequence completes.
    pub fn ingest(&mut self, datagram: &[u8]) -> Ingested<'_> {
        if datagram.len() < HEADER_SIZE {
            return Ingested::default();
        }

        // Safety: length checked above. `read_unaligned` copies the 20 bytes into a
        // properly-aligned local, avoiding UB from creating a reference to a packed
        // struct field (the `u64` may not be 8-byte aligned in the receive buffer).
        let hdr = unsafe { std::ptr::read_unaligned(datagram.as_ptr() as *const Header) };

        // Heartbeats are header-only, distinguished by fragments == 0.
        if hdr.fragments == 0 {
            return Ingested {
                heartbeat: true,
                ..Ingested::default()
            };
        }

        // Fragment 0 carries the source-side timestamp.
        if hdr.fragment == 0 {
            self.last_source_us = hdr.source_us;
        }

        let new_seq = match self.current_seq {
            Some(s) => s != hdr.sequence,
            None => true,
        };

        if new_seq {
            self.reset(&hdr);
        }

        let first_frag = hdr.fragment == 0;

        // Ignore out-of-range or duplicate fragments.
        let idx = hdr.fragment as usize;
        if idx >= self.total_frags as usize || self.received[idx] {
            return Ingested { new_seq: first_frag, ..Ingested::default() };
        }

        let data = &datagram[HEADER_SIZE..];
        if data.len() > MAX_PAYLOAD_PER_DATAGRAM {
            return Ingested { new_seq: first_frag, ..Ingested::default() };
        }
        let dest_offset = idx * MAX_PAYLOAD_PER_DATAGRAM;
        if dest_offset + data.len() > self.buf.len() {
            return Ingested { new_seq: first_frag, ..Ingested::default() };
        }
        self.buf[dest_offset..dest_offset + data.len()].copy_from_slice(data);
        self.received[idx] = true;
        self.got_frags += 1;

        if self.got_frags == self.total_frags {
            self.last_fragment_count = self.total_frags;
            self.current_seq = None;
            Ingested {
                assembled: Some(&self.buf[..self.payload_size]),
                new_seq: first_frag,
                heartbeat: false,
                buf_offset: self.last_buf_offset,
            }
        } else {
            Ingested { new_seq: first_frag, ..Ingested::default() }
        }
    }

    fn reset(&mut self, hdr: &Header) {
        if self.got_frags > 0 && self.got_frags < self.total_frags {
            self.dropped_sequences += 1;
        }

        // Reject malformed packets before allocating. Legitimate session-info
        // frames are at most ~34 fragments and ~300 KB compressed; anything
        // beyond MAX_FRAGMENTS or max_payload is corrupt or a spoofed packet.
        // Set total_frags = 0 so the idx >= total_frags check in ingest() silently
        // discards all subsequent fragments for this sequence.
        if hdr.fragments > MAX_FRAGMENTS || hdr.payload_size as usize > self.max_payload {
            self.current_seq = Some(hdr.sequence);
            self.total_frags = 0;
            self.got_frags = 0;
            return;
        }

        self.current_seq = Some(hdr.sequence);
        self.total_frags = hdr.fragments;
        self.got_frags = 0;
        self.payload_size = hdr.payload_size as usize;
        self.last_buf_offset = hdr.buf_offset;

        self.received.clear();
        self.received.resize(hdr.fragments as usize, false);

        self.buf.clear();
        self.buf.resize(hdr.payload_size as usize, 0);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(payload: &[u8]) -> Vec<u8> {
        let mut sender = Sender::new();
        let mut datagrams = Vec::new();
        sender
            .send(payload, 42, u32::MAX, |d| {
                datagrams.push(d.to_vec());
                Ok(())
            })
            .unwrap();

        let mut receiver = Receiver::new(payload.len() + MAX_PAYLOAD_PER_DATAGRAM);
        let mut result = None;
        for dg in &datagrams {
            if let Some(out) = receiver.ingest(dg).assembled {
                result = Some(out.to_vec());
            }
        }
        result.expect("never assembled")
    }

    #[test]
    fn single_fragment() {
        let data: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        assert_eq!(round_trip(&data), data);
    }

    #[test]
    fn multi_fragment() {
        let data: Vec<u8> = (0..MAX_PAYLOAD_PER_DATAGRAM * 3 + 500)
            .map(|i| (i % 251) as u8)
            .collect();
        assert_eq!(round_trip(&data), data);
    }

    #[test]
    fn out_of_order() {
        let payload: Vec<u8> = (0..MAX_PAYLOAD_PER_DATAGRAM * 2 + 1)
            .map(|i| (i % 199) as u8)
            .collect();

        let mut sender = Sender::new();
        let mut datagrams = Vec::new();
        sender
            .send(&payload, 0, u32::MAX, |d| {
                datagrams.push(d.to_vec());
                Ok(())
            })
            .unwrap();

        // Deliver last fragment first, then the rest.
        let mut receiver = Receiver::new(payload.len() + MAX_PAYLOAD_PER_DATAGRAM);
        let mut result = None;
        let last = datagrams.pop().unwrap();
        if let Some(out) = receiver.ingest(&last).assembled {
            result = Some(out.to_vec());
        }
        for dg in &datagrams {
            if let Some(out) = receiver.ingest(dg).assembled {
                result = Some(out.to_vec());
            }
        }
        assert_eq!(result.unwrap(), payload);
    }

    #[test]
    fn source_timestamp_preserved() {
        let data = vec![0u8; 100];
        let mut sender = Sender::new();
        let mut datagrams = Vec::new();
        sender
            .send(&data, 9999, u32::MAX, |d| {
                datagrams.push(d.to_vec());
                Ok(())
            })
            .unwrap();

        let mut receiver = Receiver::new(1024);
        receiver.ingest(&datagrams[0]);
        assert_eq!(receiver.last_source_us, 9999);
    }
}
