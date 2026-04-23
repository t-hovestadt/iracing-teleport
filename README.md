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
  --bind <ADDR>     Local address to bind to          [default: 0.0.0.0:0]
  --target <ADDR>   Destination (multicast group:port or unicast IP:port)
                                                      [default: 239.255.0.1:5000]
  --unicast         Send directly to one host instead of multicast
  --help            Print help
  --version         Print version
```

### target.exe

```
Options:
  --bind <ADDR>     Address and port to listen on     [default: 0.0.0.0:5000]
  --group <ADDR>    Multicast group to join           [default: 239.255.0.1]
  --unicast         Expect a direct unicast stream instead of multicast
  --help            Print help
  --version         Print version
```

---

## Status output

Both tools print a stats line every 5 seconds:

```
[source] 60.0 msg/s  48.20 Mbps  23.0 frags/msg  312 µs avg latency
[target] 60.0 msg/s  48.20 Mbps  23.0 frags/msg  891 µs avg latency
```

The target latency figure is end-to-end: source processing time plus network transit.

---

## Behaviour

- **source** waits indefinitely for iRacing to start. Once connected it prints the telemetry map size. If iRacing stops responding for 10 seconds it drops the connection and waits again.
- **target** creates its local shared-memory region the first time a complete frame arrives. If no data is received for 10 seconds the map is closed; it is recreated when data resumes.
- Press **Ctrl-C** on either machine to shut down cleanly.

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
cargo build --release --target x86_64-pc-windows-gnu
```

---

## Technical details

### Protocol

Each telemetry frame (~1.1 MB uncompressed) is compressed with LZ4 and split into 9,000-byte UDP datagrams. Every datagram carries a 20-byte header:

| Field | Type | Description |
|-------|------|-------------|
| `source_us` | u64 | Microseconds spent on source side |
| `sequence` | u32 | Monotonically increasing per message |
| `payload_size` | u32 | Total compressed bytes across all fragments |
| `fragment` | u16 | 0-based index of this fragment |
| `fragments` | u16 | Total fragment count for this sequence |

The receiver reassembles fragments out-of-order and discards duplicates. A new sequence discards any in-progress assembly from the previous one.

### Performance design

- **2 MB socket buffers** on both sides (via `socket2`) — the OS default of 64 KB drops all but the first 7 of ~23 fragments per frame, losing the whole frame.
- **Zero-allocation hot path** — compression writes into a pre-allocated buffer; decompression writes directly into the mapped memory region.
- **LTO + single codegen unit** in the release profile for maximum inlining across crate boundaries.
- **Target address pre-parsed** to `SocketAddr` before the send loop — avoids re-parsing the string 23 times per frame.

---

## Improvements over sklose/iracing-teleport

This project started as a from-scratch reimplementation of [iracing-teleport](https://github.com/sklose/iracing-teleport). Key differences:

- **Wire header is 20 bytes, not 24** — `repr(C)` adds 4 bytes of trailing padding; `repr(C, packed)` with a compile-time size assertion removes it.
- **No undefined behaviour on receive** — reading a packed struct through a reference is UB when unaligned; replaced with `ptr::read_unaligned`.
- **OS socket buffers set to 2 MB** — the original used the OS default, which is smaller than a single frame on Windows.
- **Target address parsed once** — the original parsed a `&str` address inside the fragment loop, re-doing the work ~23 times per frame.
- **Pre-allocated compression buffer** — the original allocated a new `Vec` per frame.
- **Zero-copy decompression** — the original decompressed into a temporary `Vec` then copied; we decompress directly into shared memory.
- **Separate `source.exe` and `target.exe`** — simpler to distribute; users only need the one relevant to their machine.
- **Proper reconnect logic** — the original exited if iRacing hadn't started within 5 seconds; source now waits indefinitely using a typed `OpenResult` enum.
- **Actual region size via `VirtualQuery`** — instead of a hardcoded constant.
- **`Drop` guards** — null and `INVALID_HANDLE_VALUE` checks before each handle close.
- **End-to-end latency stats** — combines source processing time (carried in the header) with network transit time measured at the target.

---

## License

MIT
