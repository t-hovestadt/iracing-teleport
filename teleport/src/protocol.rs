use std::io;

/// Default UDP datagram payload size (bytes). Chosen to fit inside a 9000-byte
/// jumbo-frame MTU (9000 - 20 IP - 8 UDP = 8972 usable, minus 24-byte protocol
/// header leaves 8948 payload bytes, but we round up to 9000 to match the
/// historical behaviour where the OS handles any minor IP fragmentation on
/// non-jumbo links). On standard 1500-byte MTU networks use `--datagram-size 1472`
/// to avoid IP fragmentation.
pub const MAX_DATAGRAM_SIZE: usize = 9_000;

const HEADER_SIZE: usize = std::mem::size_of::<Header>();
pub const MAX_PAYLOAD_PER_DATAGRAM: usize = MAX_DATAGRAM_SIZE - HEADER_SIZE;

/// Bit flag set in `buf_offset` to signal an XOR-delta encoded partial frame.
/// The real write offset is `buf_offset & !DELTA_BIT`.
/// Never set on session-info frames (`buf_offset == u32::MAX`).
/// Old receivers see a large `buf_offset`, fail the bounds check, discard the
/// frame, and send a resync — the source responds with a full keyframe (safe).
pub const DELTA_BIT: u32 = 1 << 31;

/// XOR `current` against `previous` into `output` (all slices same length).
/// Uses 8-byte chunks so LLVM auto-vectorises to SSE2/AVX2.
pub fn xor_delta(current: &[u8], previous: &[u8], output: &mut [u8]) {
    debug_assert_eq!(current.len(), previous.len(), "xor_delta: current/previous length mismatch");
    debug_assert_eq!(current.len(), output.len(),   "xor_delta: current/output length mismatch");
    let len = current.len();
    let chunks = len / 8;
    let (cur_chunks, cur_tail) = current.split_at(chunks * 8);
    let (prev_chunks, prev_tail) = previous.split_at(chunks * 8);
    let (out_chunks, out_tail) = output.split_at_mut(chunks * 8);
    for i in 0..chunks {
        let c = u64::from_ne_bytes(cur_chunks[i * 8..i * 8 + 8].try_into().unwrap());
        let p = u64::from_ne_bytes(prev_chunks[i * 8..i * 8 + 8].try_into().unwrap());
        out_chunks[i * 8..i * 8 + 8].copy_from_slice(&(c ^ p).to_ne_bytes());
    }
    for i in 0..cur_tail.len() {
        out_tail[i] = cur_tail[i] ^ prev_tail[i];
    }
}

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
    /// Application-level payload bytes per datagram (= datagram_size - HEADER_SIZE).
    payload_per_datagram: usize,
}

impl Default for Sender {
    fn default() -> Self {
        Self::new()
    }
}

impl Sender {
    pub fn new() -> Self {
        Self::with_datagram_size(MAX_DATAGRAM_SIZE)
    }

    /// Create a sender that fits each fragment into `datagram_size` UDP payload bytes.
    /// Use `MAX_DATAGRAM_SIZE` (9000) for jumbo-frame links; use 1472 for standard
    /// 1500-byte MTU networks to avoid IP fragmentation.
    pub fn with_datagram_size(datagram_size: usize) -> Self {
        let datagram_size = datagram_size.max(HEADER_SIZE + 1); // at least 1 payload byte
        Self {
            sequence: 0,
            buf: vec![0u8; datagram_size],
            payload_per_datagram: datagram_size - HEADER_SIZE,
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
        let n_fragments = total.div_ceil(self.payload_per_datagram);
        if n_fragments == 0 || n_fragments > u16::MAX as usize {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "payload too large or empty"));
        }

        for i in 0..n_fragments {
            let offset = i * self.payload_per_datagram;
            let chunk = &data[offset..(offset + self.payload_per_datagram).min(total)];

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
    /// Fragment payload size auto-detected from the first non-final datagram.
    /// All non-final fragments carry exactly this many bytes; the final fragment
    /// carries the remainder. Detected rather than hardcoded so the receiver
    /// works transparently with any sender `--datagram-size` setting.
    detected_frag_size: usize,
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
            detected_frag_size: 0,
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

        // Auto-detect the sender's fragment payload size from any non-final fragment.
        // All non-final fragments carry the same number of bytes (the sender's
        // payload_per_datagram). The final fragment carries the remainder and may be
        // shorter. Single-fragment messages always write at dest_offset=0 regardless.
        if idx < self.total_frags as usize - 1 && self.detected_frag_size == 0 {
            self.detected_frag_size = data.len();
        }
        let frag_size = if self.detected_frag_size > 0 {
            self.detected_frag_size
        } else {
            MAX_PAYLOAD_PER_DATAGRAM // fallback before first non-final fragment seen
        };
        let dest_offset = idx * frag_size;
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
        self.detected_frag_size = 0; // re-detect on each new sequence

        self.received.clear();
        self.received.resize(hdr.fragments as usize, false);

        // Avoid O(n) zero-fill when shrinking: truncate leaves existing bytes in
        // place, and they will be overwritten by ingest() before being read back.
        // Only grow (with zeroing) when the new sequence needs more space.
        let new_size = hdr.payload_size as usize;
        if new_size > self.buf.len() {
            self.buf.resize(new_size, 0);
        } else {
            self.buf.truncate(new_size);
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a raw datagram with arbitrary header fields, bypassing Sender
    /// validation. Used to test receiver behaviour on malformed input.
    fn make_datagram(sequence: u32, fragments: u16, payload_size: u32, fragment_idx: u16, data: &[u8]) -> Vec<u8> {
        let hdr = Header {
            source_us: 0,
            sequence,
            payload_size,
            buf_offset: u32::MAX,
            fragment: fragment_idx,
            fragments,
        };
        let mut buf = vec![0u8; HEADER_SIZE + data.len()];
        let hdr_bytes = unsafe {
            std::slice::from_raw_parts(&hdr as *const _ as *const u8, HEADER_SIZE)
        };
        buf[..HEADER_SIZE].copy_from_slice(hdr_bytes);
        buf[HEADER_SIZE..].copy_from_slice(data);
        buf
    }

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

    // ── Bounds-check tests ────────────────────────────────────────────────────

    #[test]
    fn rejects_oversized_fragment_count() {
        let mut receiver = Receiver::new(1024);
        // fragments = MAX_FRAGMENTS + 1 should be rejected without allocating.
        let dg = make_datagram(1, MAX_FRAGMENTS + 1, 100, 0, &[0u8; 100]);
        assert!(receiver.ingest(&dg).assembled.is_none());
        // Subsequent fragments for the same sequence are also discarded.
        let dg2 = make_datagram(1, MAX_FRAGMENTS + 1, 100, 1, &[0u8; 100]);
        assert!(receiver.ingest(&dg2).assembled.is_none());
    }

    #[test]
    fn rejects_oversized_payload_size() {
        let max_payload = 1024usize;
        let mut receiver = Receiver::new(max_payload);
        // payload_size = max_payload + 1 should be rejected.
        let dg = make_datagram(1, 1, (max_payload + 1) as u32, 0, &[0u8; 10]);
        assert!(receiver.ingest(&dg).assembled.is_none());
    }

    #[test]
    fn valid_frame_after_rejected_malformed() {
        let payload: Vec<u8> = (0..200).map(|i| (i % 251) as u8).collect();
        let max_payload = payload.len() + MAX_PAYLOAD_PER_DATAGRAM;
        let mut receiver = Receiver::new(max_payload);

        // Malformed packet on sequence 99 — distinct from Sender's starting sequence 0.
        let bad = make_datagram(99, MAX_FRAGMENTS + 1, 100, 0, &[0u8; 10]);
        assert!(receiver.ingest(&bad).assembled.is_none());

        // Valid frame on sequence 0 should assemble cleanly despite the prior rejection.
        let mut sender = Sender::new();
        let mut datagrams = Vec::new();
        sender
            .send(&payload, 0, u32::MAX, |d| {
                datagrams.push(d.to_vec());
                Ok(())
            })
            .unwrap();

        let mut assembled = None;
        for dg in &datagrams {
            if let Some(out) = receiver.ingest(dg).assembled {
                assembled = Some(out.to_vec());
            }
        }
        assert_eq!(assembled.unwrap(), payload);
    }

    // ── Custom datagram-size tests ────────────────────────────────────────────

    /// Sender with a small datagram size (e.g. 1472 bytes for 1500-MTU networks)
    /// should fragment and reassemble correctly. Receiver auto-detects the fragment
    /// size from non-final fragments.
    #[test]
    fn small_datagram_size_round_trip() {
        // Slightly larger than one 1448-byte payload to force multi-fragment.
        let payload: Vec<u8> = (0..3000).map(|i| (i % 197) as u8).collect();
        let datagram_size = 1472; // standard 1500-MTU UDP payload limit
        let mut sender = Sender::with_datagram_size(datagram_size);
        let mut datagrams = Vec::new();
        let frags = sender
            .send(&payload, 0, u32::MAX, |d| {
                // Every datagram must fit within the configured datagram_size.
                assert!(d.len() <= datagram_size, "datagram too large: {}", d.len());
                datagrams.push(d.to_vec());
                Ok(())
            })
            .unwrap();
        assert!(frags >= 2, "expected multiple fragments, got {frags}");

        let mut receiver = Receiver::new(payload.len() + datagram_size);
        let mut result = None;
        for dg in &datagrams {
            if let Some(out) = receiver.ingest(dg).assembled {
                result = Some(out.to_vec());
            }
        }
        assert_eq!(result.expect("never assembled"), payload);
    }

    /// Same data should round-trip correctly regardless of which fragment
    /// size the sender chose, even when the receiver starts with no prior
    /// knowledge of the sender's datagram size.
    #[test]
    fn varying_datagram_sizes() {
        for &dsize in &[256usize, 512, 1472, 4096, MAX_DATAGRAM_SIZE] {
            let payload: Vec<u8> = (0..dsize * 3 / 2).map(|i| (i % 211) as u8).collect();
            let mut sender = Sender::with_datagram_size(dsize);
            let mut datagrams = Vec::new();
            sender
                .send(&payload, 0, u32::MAX, |d| {
                    datagrams.push(d.to_vec());
                    Ok(())
                })
                .unwrap();
            let mut receiver = Receiver::new(payload.len() + dsize);
            let mut result = None;
            for dg in &datagrams {
                if let Some(out) = receiver.ingest(dg).assembled {
                    result = Some(out.to_vec());
                }
            }
            assert_eq!(
                result.expect("never assembled"),
                payload,
                "failed for datagram_size={dsize}"
            );
        }
    }

    /// Receiver correctly handles LZ4-compressed end-to-end pipeline:
    /// compress → fragment → reassemble → decompress → original data.
    #[test]
    fn lz4_round_trip_through_protocol() {
        use lz4_flex::block::{compress_into, decompress_into, get_maximum_output_size};

        let original: Vec<u8> = (0..12_000).map(|i| (i % 13) as u8).collect();
        let mut compressed = vec![0u8; get_maximum_output_size(original.len())];
        let compressed_len = compress_into(&original, &mut compressed).unwrap();
        let compressed = &compressed[..compressed_len];

        let mut sender = Sender::new();
        let mut datagrams = Vec::new();
        sender
            .send(compressed, 99, 0x4000u32, |d| {
                datagrams.push(d.to_vec());
                Ok(())
            })
            .unwrap();

        let mut receiver = Receiver::new(compressed.len() + MAX_PAYLOAD_PER_DATAGRAM);
        let mut assembled_compressed: Option<Vec<u8>> = None;
        for dg in &datagrams {
            let res = receiver.ingest(dg);
            if let Some(data) = res.assembled {
                assembled_compressed = Some(data.to_vec());
            }
        }
        let assembled = assembled_compressed.expect("never assembled");
        assert_eq!(assembled, compressed, "protocol round-trip corrupted data");

        let mut decompressed = vec![0u8; original.len()];
        decompress_into(&assembled, &mut decompressed).expect("decompression failed");
        assert_eq!(decompressed, original);
        assert_eq!(receiver.last_source_us, 99);
        assert_eq!(receiver.last_buf_offset, 0x4000u32);
    }

    // ── XOR-delta tests ───────────────────────────────────────────────────────

    /// Round-trip XOR-delta: modify 5% of a 16 KB buffer, encode the delta,
    /// then decode it and verify the reconstructed bytes match.
    #[test]
    fn xor_delta_round_trip() {
        use lz4_flex::block::{compress_into, decompress_into, get_maximum_output_size};

        let size = 16_000usize;
        let prev: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let mut current = prev.clone();
        // Modify roughly 5% of the bytes.
        for i in (0..size).step_by(20) {
            current[i] = current[i].wrapping_add(1);
        }

        // Compute delta and compress it.
        let mut delta = vec![0u8; size];
        xor_delta(&current, &prev, &mut delta);

        let mut compressed = vec![0u8; get_maximum_output_size(size)];
        let compressed_len = compress_into(&delta, &mut compressed).unwrap();
        let compressed = &compressed[..compressed_len];

        // Delta of a mostly-unchanged buffer should compress significantly better
        // than the raw current buffer.
        let mut raw_compressed = vec![0u8; get_maximum_output_size(size)];
        let raw_len = compress_into(&current, &mut raw_compressed).unwrap();
        assert!(
            compressed.len() < raw_len,
            "delta ({} bytes) should be smaller than raw ({raw_len} bytes)",
            compressed.len(),
        );

        // Decompress and reconstruct: current = delta XOR prev.
        let mut decompressed_delta = vec![0u8; size];
        decompress_into(compressed, &mut decompressed_delta).unwrap();

        let mut reconstructed = vec![0u8; size];
        xor_delta(&decompressed_delta, &prev, &mut reconstructed);
        assert_eq!(reconstructed, current, "XOR-delta round-trip failed");
    }

    /// When prev_varbuf is all zeros (fresh target), the delta equals the
    /// current buffer. The target XOR-reconstructs back to the original.
    #[test]
    fn xor_delta_zeroed_prev() {
        let size = 4_000usize;
        let current: Vec<u8> = (0..size).map(|i| (i % 199) as u8).collect();
        let prev = vec![0u8; size];

        let mut delta = vec![0u8; size];
        xor_delta(&current, &prev, &mut delta);
        // delta == current when prev is all zeros.
        assert_eq!(delta, current);

        // Reconstruct: XOR delta against zeros gives back current.
        let mut reconstructed = vec![0u8; size];
        xor_delta(&delta, &prev, &mut reconstructed);
        assert_eq!(reconstructed, current);
    }

    /// Session-sequence counter increments across successive sends; the
    /// receiver discards any new sequence that starts before the previous one
    /// completes (dropped_sequences tracks this).
    #[test]
    fn dropped_sequence_counted() {
        let large: Vec<u8> = vec![0u8; MAX_PAYLOAD_PER_DATAGRAM * 3];
        let mut sender = Sender::new();
        let mut datagrams_a = Vec::new();
        sender.send(&large, 0, u32::MAX, |d| { datagrams_a.push(d.to_vec()); Ok(()) }).unwrap();
        let mut datagrams_b = Vec::new();
        sender.send(&large, 0, u32::MAX, |d| { datagrams_b.push(d.to_vec()); Ok(()) }).unwrap();

        let mut receiver = Receiver::new(large.len() + MAX_PAYLOAD_PER_DATAGRAM);
        // Deliver only the first fragment of sequence A, then all of sequence B.
        receiver.ingest(&datagrams_a[0]);
        let mut assembled = false;
        for dg in &datagrams_b {
            if receiver.ingest(dg).assembled.is_some() {
                assembled = true;
            }
        }
        assert!(assembled, "sequence B should assemble");
        assert_eq!(receiver.dropped_sequences, 1, "sequence A should be counted as dropped");
    }
}
