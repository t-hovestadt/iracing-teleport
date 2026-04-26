# iRacing Teleport

Stream iRacing telemetry over your local network so SimHub (or any iRacing-compatible app) runs on a separate machine from your iRacing installation.

Two small Windows executables, no installers, no dependencies.

---

## How it works

iRacing exposes its telemetry through a Windows shared-memory region and a Win32 event. **source.exe** maps that memory, compresses each frame with LZ4, fragments it into UDP datagrams, and sends them over the network. **target.exe** receives, reassembles, decompresses, and writes the data into an identical shared-memory region on the destination machine — making it look to SimHub (or any other tool) as if iRacing is running locally.

```
┌─────────────────────────┐         UDP (multicast or unicast)        ┌─────────────────────────┐
│     iRacing PC          │  ────────────────────────────────────►   │     SimHub PC           │
│                         │                                           │                         │
│  iRacing                │                                           │  SimHub / overlays      │
│    └─ shared memory     │                                           │    └─ shared memory     │
│         └─ source.exe   │                                           │         └─ target.exe   │
└─────────────────────────┘                                           └─────────────────────────┘
```

---

## Download

Pre-built Windows x64 binaries are available on the [Releases](../../releases) page.

| File | Machine |
|------|---------|
| `source.exe` | iRacing PC |
| `target.exe` | SimHub PC |

---

## Quick start

**Default setup (multicast — works on most home networks):**

1. Copy `target.exe` to your SimHub PC and run it:
   ```
   target.exe
   ```

2. Run `source.exe` on your iRacing PC:
   ```
   source.exe
   ```

Both tools connect automatically. Start them in any order — source will wait for iRacing to launch, and target will wait for data to arrive.

**Unicast (if multicast doesn't work on your network):**

```
# SimHub PC — listen on any interface
target.exe --unicast

# iRacing PC — send directly to the SimHub machine's IP
source.exe --unicast --target 192.168.1.50:5000
```

---

## All options

### source.exe

```
Options:
  --bind <ADDR>       Local address to bind to          [default: 0.0.0.0:0]
  --target <ADDR>     Destination (multicast group:port or unicast IP:port)
                                                        [default: 239.255.0.1:5000]
  --unicast           Send directly to one host instead of multicast
  --pin-core <N>      Pin the source thread to CPU core N for lower jitter
  --help              Print help
  --version           Print version
```

### target.exe

```
Options:
  --bind <ADDR>       Address and port to listen on     [default: 0.0.0.0:5000]
  --group <ADDR>      Multicast group to join           [default: 239.255.0.1]
  --unicast           Expect a direct unicast stream instead of multicast
  --busy-wait         Spin on recv instead of sleeping — burns one CPU core
                      but removes ~500 µs of OS scheduler wake-up jitter
  --pin-core <N>      Pin the target thread to CPU core N for lower jitter
  --help              Print help
  --version           Print version
```

---

## Status output

Both tools print a stats line every 5 seconds and a summary on shutdown:

```
[source] 60.0 msg/s  0.48 Mbps  1.0 frags/msg  8/12/18 µs p50/p99/max  0 dropped
[target] 60.0 msg/s  0.48 Mbps  1.0 frags/msg  210/280/340 µs p50/p99/max  0 dropped
```

The target latency figure is end-to-end: source processing time plus network transit.
Latency spikes to ~150 µs on session-info change frames (full 1.1 MB map); normal
frames send only the active variable buffer (~5–15 KB, 1 fragment).

---

## Behaviour

- **source** waits indefinitely for iRacing to start. Once connected it prints the telemetry map size and starts streaming.
- Each frame, source sends only the **active variable buffer** (~5–15 KB) rather than the full 1.1 MB map. Every partial frame includes the current 112-byte irsdk header so that `tickCount` values stay accurate — iRacing rotates through a ring of 2–3 varBuf slots, and SimHub uses tickCounts to pick the right slot; stale counts would cause reads from the previous slot. A full frame is sent when the session changes (new track, car swap, etc.) or every 30 seconds, so a restarted target resyncs quickly without needing a session change.
- **Heartbeats**: when iRacing is open but between sessions (menus, loading screens), source sends a small keep-alive packet every second so target keeps its shared-memory region alive for SimHub.
- **target** creates its local shared-memory region the first time a complete frame arrives. If no data is received for 10 seconds the `IRSDK_ST_CONNECTED` status flag is cleared (so SimHub sees a clean disconnect) and the map is closed; it is recreated when data resumes.
- If iRacing stops responding for 10 seconds, source drops the connection and waits to reconnect.
- Press **Ctrl-C** on either machine to shut down cleanly; both tools print a lifetime summary.

---

## Building from source

Requires [Rust](https://rustup.rs) (stable).

```
git clone https://github.com/t-hovestadt/iracing-teleport
cd iracing-teleport/teleport
cargo build --release
```

Binaries are written to `target/release/`.

Cross-compiling for Windows from macOS or Linux requires `mingw-w64` and the `x86_64-pc-windows-gnu` Rust target:

```
rustup target add x86_64-pc-windows-gnu
brew install mingw-w64          # macOS
CARGO_TARGET_DIR=/tmp/iracing-build cargo build --release --target x86_64-pc-windows-gnu
```

> **Note:** If your working directory path contains spaces, set `CARGO_TARGET_DIR` to a path without spaces (the `mingw-w64` linker doesn't handle quoted paths).

---

## Technical details

### Protocol

Each telemetry frame is compressed with LZ4 and split into 9,000-byte UDP datagrams. Every datagram carries a 24-byte header:

| Field | Type | Description |
|-------|------|-------------|
| `source_us` | u64 | Microseconds spent on source side |
| `sequence` | u32 | Monotonically increasing per message |
| `payload_size` | u32 | Total compressed bytes across all fragments |
| `buf_offset` | u32 | Byte offset to write decompressed data in the target map; `u32::MAX` = full frame (write at offset 0) |
| `fragment` | u16 | 0-based index of this fragment |
| `fragments` | u16 | Total fragment count for this sequence; `0` = heartbeat (no payload) |

The receiver reassembles fragments out-of-order and discards duplicates. A new sequence discards any in-progress assembly from the previous one.

### Performance design

- **Partial telemetry**: iRacing's header exposes a ring of up to 4 variable buffers (~5–15 KB each). Source reads the highest-tick slot each frame and sends only that slice, cutting per-frame data from ~1.1 MB to ~5–15 KB and fragment count from ~23 to 1. Full frames are sent on session changes and every 30 s for target resync.
- **2 MB socket buffers** on both sides (via `socket2`) — the OS default of 64 KB would drop all but the first 7 fragments of a full frame.
- **Zero-allocation hot path** — compression writes into a pre-allocated buffer; decompression writes directly into the mapped memory region.
- **1 ms timer resolution** — source and target call `timeBeginPeriod(1)` on startup so Windows sleep and event waits resolve at 1 ms granularity rather than the default 15.6 ms.
- **Above-normal thread priority** — both binaries raise their main thread priority to reduce OS scheduling latency.
- **CPU affinity** (`--pin-core N`) — optionally pins the hot thread to a single core to eliminate cross-core migration jitter.
- **Busy-wait mode** (`--busy-wait` on target) — spins on `recv_from` instead of sleeping, trading one full CPU core for ~500 µs less OS scheduler wake-up jitter per frame.
- **LTO + single codegen unit** in the release profile for maximum inlining across crate boundaries.

---

## Improvements over sklose/iracing-teleport

This project started as a from-scratch reimplementation of [iracing-teleport](https://github.com/sklose/iracing-teleport). Key differences:

- **Partial telemetry** — sends only the active iRacing variable buffer (~5–15 KB) per frame instead of the full 1.1 MB map, cutting latency from ~1.4 ms to ~200–500 µs on typical LAN.
- **Accurate varBuf slot selection** — each partial frame includes the current 112-byte irsdk header so the target always has up-to-date `tickCount` values; without this, SimHub could silently read a one-frame-stale slot when iRacing rotates its ring buffer.
- **Heartbeats** — keep-alive packets during menus and loading screens prevent SimHub from losing its telemetry connection between sessions.
- **STATUS flag clear on disconnect** — zeros `IRSDK_ST_CONNECTED` before closing the target memory map so SimHub sees a clean disconnect rather than a stale "still connected" flag.
- **Latency percentiles** — stats show p50/p99/max per window and lifetime min/avg/max on shutdown, instead of a simple average.
- **Busy-wait and CPU affinity** — optional flags for lowest possible latency on dedicated hardware.
- **Correct 24-byte wire header** — `repr(C, packed)` with a compile-time size assertion; the original used `repr(C)` which adds 4 bytes of trailing padding, silently making the header 24 bytes with undefined layout.
- **No undefined behaviour on receive** — reading a packed struct through a reference is UB when unaligned; replaced with `ptr::read_unaligned`.
- **OS socket buffers set to 2 MB** — the original used the OS default, which is smaller than a single full frame on Windows.
- **Target address parsed once** — the original parsed a `&str` address inside the fragment loop.
- **Pre-allocated compression buffer** — the original allocated a new `Vec` per frame.
- **Zero-copy decompression for full frames** — full frames decompress directly into shared memory; partial frames decompress to a small staging buffer then write header and varBuf separately.
- **Separate `source.exe` and `target.exe`** — simpler to distribute; users only need the one relevant to their machine.
- **Proper reconnect logic** — the original exited if iRacing hadn't started within 5 seconds; source now waits indefinitely using a typed `OpenResult` enum.
- **Actual region size via `VirtualQuery`** — instead of a hardcoded constant.
- **`Drop` guards** — null and `INVALID_HANDLE_VALUE` checks before each handle close.
- **End-to-end latency stats** — combines source processing time (carried in the header) with network transit time measured at the target.

---

## License

MIT
