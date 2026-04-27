# iRacing Teleport

Stream iRacing telemetry over your local network so SimHub (or any iRacing-compatible app) runs on a separate machine from your iRacing installation. Two small Windows executables, no installers, no dependencies.

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

Pre-built Windows x64 binaries are on the [Releases](../../releases/latest) page.

| File | Machine |
|------|---------|
| `source.exe` | iRacing PC |
| `target.exe` | SimHub PC |
| `teleport.exe` | Either, combined CLI (`teleport source` / `teleport target`) |

---

## Quick start

Default (multicast, works on most home networks):

1. Run `target.exe` on your SimHub PC
2. Run `source.exe` on your iRacing PC

Start them in any order. source waits for iRacing to launch; target waits for data.

Unicast (if multicast doesn't work on your network):

```
# SimHub PC
target.exe --unicast

# iRacing PC (replace with your SimHub machine's IP)
source.exe --unicast --target 192.168.1.50:5000
```

---

## Options

| Flag | source | target | Default | Description |
|------|:------:|:------:|---------|-------------|
| `--bind <ADDR>` | ✓ | ✓ | `0.0.0.0:0` / `0.0.0.0:5000` | Local address to bind the UDP socket to |
| `--target <ADDR>` | ✓ | | `239.255.0.1:5000` | Destination (multicast group:port or unicast IP:port) |
| `--unicast` | ✓ | ✓ | off | Send/receive directly host-to-host instead of multicast |
| `--group <ADDR>` | | ✓ | `239.255.0.1` | Multicast group to join |
| `--busy-wait` | | ✓ | off | Spin on recv instead of sleeping; lower jitter, costs one CPU core |
| `--pin-core <N>` | ✓ | ✓ | off | Pin the worker thread to CPU core N (0-based) |
| `--fanalab` | | ✓ | off | Spawn a dummy iRacingSim64DX11.exe so FanaLab detects iRacing and auto-loads per-car profiles |

---

## How it works

- **source** maps the iRacing shared memory region, compresses each frame with LZ4, and sends it over UDP. It waits indefinitely for iRacing to start and reconnects automatically if iRacing closes.
- Each frame sends only the ~5–15 KB slice of memory that actually changed. A full sync (~150–250 KB) is sent when the session changes, when target reconnects, or every 10 s as a fallback.
- **target** receives, reassembles, and decompresses the data into a matching shared memory region on the SimHub PC, so SimHub sees iRacing as running locally. The map is created on first data arrival and closed cleanly if no data is received for 10 s.
- Heartbeat packets keep the connection alive across loading screens and menus so SimHub doesn't disconnect mid-session.

Both tools print a stats line every 5 s (`60 msg/s  0.48 Mbps  200/280/340 µs p50/p99/max`) and a summary on Ctrl-C.

---

## Compatible apps

Any app that reads iRacing shared memory works automatically on the target machine — the memory map is identical to what iRacing produces locally. No extra configuration needed in the app.

**Dashboards and overlays**
- [SimHub](https://www.simhubdash.com) — dashboards, overlays, haptics, LED control
- [RaceLab](https://racelab.app) — modern overlay suite
- [iOverlay](https://ioverlay.app) — standings and timing overlays
- [Z1 Dashboard](https://www.z1racetech.com) — live telemetry display and lap analysis
- [SDK Gaming](https://www.sdk-gaming.co.uk) — HUD and live timing overlays

**Haptics and bass shakers**
- [Track Impulse](https://track-impulse.com) — dedicated haptic engine, reads iRacing's 360 Hz sub-samples for higher resolution shaker output
- [ButtKicker HaptiConnect](https://thebuttkicker.com) — haptic feedback using suspension, engine, and track surface data
- [irFFB](https://github.com/nlp80/irFFB) — FFB enhancement using 360 Hz telemetry; also supports seat shakers

**Wheel hardware**
- [FanaLab](https://fanatec.com/fanalab) — per-car profiles for Fanatec wheels (use `--fanalab` flag)
- [FanaBridge](https://github.com/kelchm/FanaBridge) — SimHub plugin for Fanatec LED and display control

**Spotter and coaching**
- [Crew Chief](https://thecrewchief.org) — AI spotter and engineer with voice feedback
- [VRS](https://virtualracingschool.com) — professional coaching overlays with reference lap comparison
- [Trophi.ai](https://trophi.ai) — AI real-time voice coaching

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

> If your working directory path contains spaces, set `CARGO_TARGET_DIR` to a path without spaces (the `mingw-w64` linker doesn't handle quoted paths).

---

<details>
<summary>Technical details</summary>

### Protocol

Each telemetry frame is compressed with LZ4 and split into 9,000-byte UDP datagrams. Every datagram carries a 24-byte header:

| Field | Type | Description |
|-------|------|-------------|
| `source_us` | u64 | Microseconds spent on source side |
| `sequence` | u32 | Monotonically increasing per message |
| `payload_size` | u32 | Total compressed bytes across all fragments |
| `buf_offset` | u32 | Byte offset to write decompressed data in the target map; `u32::MAX` = session-info frame (write at offset 0) |
| `fragment` | u16 | 0-based index of this fragment |
| `fragments` | u16 | Total fragment count for this sequence; `0` = heartbeat |

The receiver reassembles fragments out-of-order and discards duplicates. A new sequence discards any in-progress assembly from the previous one.

### Performance design

iRacing's header exposes a ring of variable buffers (~5–15 KB each). source reads the highest-tick slot each frame and sends only that slice, cutting per-frame data from ~1.1 MB to ~5–15 KB and fragment count from ~23 to 1. Session-info frames (sent on session changes and reconnects) carry only the static map prefix (header + variable headers + session YAML, ~150–250 KB) rather than the full map. Session-info frames write to shared memory but withhold the SimHub `SetEvent` signal; the next partial frame fills the varBuf region and then fires the signal so SimHub always reads a complete map on first notification.

Socket buffers on both sides are set to 2 MB. The OS default (64 KB) is smaller than one full frame, which would silently drop fragments. Compression writes into a pre-allocated buffer; decompression writes directly into the mapped memory region.

Scheduling: both binaries call `timeBeginPeriod(1)` for 1 ms timer resolution (default is 15.6 ms) and run at above-normal thread priority. target also registers with MMCSS under the "Games" task for reserved CPU time, not applied to source to avoid competing with iRacing's own registrations. `--pin-core N` pins the thread to a single core; `--busy-wait` (target only) spins on recv instead of sleeping, trading one CPU core for ~500 µs less wakeup jitter.

Release profile uses LTO and a single codegen unit.

</details>

<details>
<summary>Improvements over sklose/iracing-teleport</summary>

Rewritten from scratch based on [sklose/iracing-teleport](https://github.com/sklose/iracing-teleport). Main differences:

- **Partial frames**: sends only the active variable buffer (~5–15 KB) per frame instead of the full 1.1 MB map; latency drops from ~1.4 ms to ~200–500 µs on a typical LAN.
- Each partial frame includes the current 112-byte irsdk header so target always has up-to-date `tickCount` values. Without this, SimHub can silently read a stale varBuf slot when iRacing rotates its ring buffer.
- Session-info frames write the static map prefix but withhold `SetEvent`; the next partial frame fills varBuf and fires the signal so SimHub reads a complete map on first notification.
- **Bidirectional resync**: target sends a 1-byte UDP packet to source when it needs a session-info frame; source responds on the next tick instead of waiting for a fixed timer.
- **MMCSS on target**: registers under the Windows "Games" multimedia task for reserved CPU time; skipped on source to avoid competing with iRacing's own registrations.
- Heartbeat packets during menus and loading screens keep the SimHub connection alive between sessions.
- `IRSDK_ST_CONNECTED` is zeroed before closing the target map so SimHub sees a clean disconnect.
- Stats show p50/p99/max latency per window with end-to-end measurement (source processing + network transit).
- Socket buffers set to 2 MB on both sides; the original used the OS default, which is smaller than one full frame.
- `repr(C, packed)` wire header with a compile-time size assertion; the original's `repr(C)` added 4 bytes of trailing padding.
- Receive path uses `ptr::read_unaligned`; reading a packed struct through a reference is undefined behaviour when unaligned.
- Pre-allocated compression buffer; the original allocated a new `Vec` per frame.
- source waits indefinitely for iRacing to start; the original exited after 5 seconds.
- Shared memory region size read via `VirtualQuery` rather than a hardcoded constant.
- `Drop` guards with null and `INVALID_HANDLE_VALUE` checks on all handles.

</details>

---

## License

MIT
