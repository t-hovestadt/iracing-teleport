<!-- Suggested tag: v1.1 -->

## What's new in v1.1

### XOR-delta compression

Partial frames are now XOR-encoded against the previous frame before LZ4 compression. iRacing telemetry changes only ~5% of bytes per tick, so delta frames compress **4–8× smaller** than raw partial frames — bandwidth drops well under 0.5 Mbps at 60 Hz. A full keyframe is sent every 60 ticks (configurable with `--keyframe-interval`) to prevent divergence if a delta is lost. Both sides reset to zeros on each session-info frame to stay in sync. Use `--no-delta` to force full frames on every tick.

### Capability negotiation

Target's resync packet is now 2 bytes: byte 0 is the resync flag; byte 1 is a capability bitfield where bit 0 signals delta support. Source enables delta encoding automatically when the target confirms support. Old targets that send a 1-byte packet are treated as delta-incapable and continue to receive full frames — no configuration required on either side.

### Duplicate-tick detection

Before compressing a frame, source now compares the active varBuf slot's `tickCount` against the last value sent. If iRacing signals the data-ready event without advancing the counter — common during loading screens and sub-60 Hz sessions — the tick is silently skipped, saving a full LZ4 compression pass and a socket send.

### Zero-allocation hot path improvements

- Fragment reassembly switched from `Vec<bool>` to an inline `[bool; 256]` array — no heap allocation per sequence reset; clearing uses a single `fill(false)` over only the slots actually used.
- The reassembly payload buffer is now pre-allocated once at startup to its maximum size; per-sequence `resize`/`truncate` calls eliminated.
- Stats percentile calculation replaced with three O(n) `select_nth_unstable` passes (called in decreasing-index order) instead of one O(n log n) sort.
- One redundant `Instant::elapsed()` call removed from the source hot path (one QPC syscall saved per tick).

### New flags

| Flag | Side | Description |
|------|------|-------------|
| `--no-delta` | source | Disable XOR-delta; send full frames every tick |
| `--keyframe-interval <N>` | source | Partial frames between keyframes (default: 60) |
| `--datagram-size <BYTES>` | source | UDP payload per fragment; use `1472` for 1500-byte MTU networks. Target auto-detects the sender's size |
| `--busy-wait` | source + target | Spin instead of sleeping — eliminates OS scheduler wake-up jitter at the cost of one CPU core |
| `--high-priority` | source + target | Raise to `HIGH_PRIORITY_CLASS` for lower scheduling jitter |
| `--fanalab` | target | Spawn a dummy `iRacingSim64DX11.exe` so FanaLab detects iRacing and auto-loads per-car profiles |
| `--reconnect-timeout <SECS>` | source | Seconds without telemetry before closing and reconnecting to iRacing (default: 10) |
| `--stale-timeout <SECS>` | target | Seconds without data before closing the telemetry map (default: 10) |

### Stats output

Target now reports source-side compression latency and delta percentage alongside the existing throughput and latency fields:

```
[source] 60.0 msg/s  0.47 Mbps  2.3x  12/18/45 µs p50/p99/max  0 dropped
[target] 60.0 msg/s  0.47 Mbps  2.3x  14/22/48 µs p50/p99/max  src: 5/9 µs p50/p99  98% delta  0 dropped
```

The `src:` fields show the source-side processing time as measured on the target (carried in each datagram header). The `N% delta` field shows what fraction of partial frames used XOR-delta encoding.

### Bug fixes

- Fixed "1-byte resync" documentation inconsistency — resync packets are 2 bytes.
- `--busy-wait` help text now explicitly notes it is safe to use on the SimHub PC; the iRacing PC caveat applies to source only.

---

**Full changelog:** see commit history from `582f5bd` to `520755d`.
