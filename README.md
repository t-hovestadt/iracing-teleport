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

**Default (multicast — works on most home networks):**

1. Run `target.exe` on your SimHub PC
2. Run `source.exe` on your iRacing PC

Start them in any order. source waits for iRacing to launch; target waits for data.

**Unicast (if multicast doesn't work on your network):**

```
# SimHub PC
target.exe --unicast

# iRacing PC (replace with your SimHub machine's IP)
source.exe --unicast --target 192.168.1.50:5000
```

**Direct Ethernet (point-to-point cable between the two PCs):**

See the [Direct Ethernet setup](#direct-ethernet-setup) section below.

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
- **Each tick** sends only the ~5–15 KB variable buffer slice that actually changed, keeping per-frame bandwidth to ~0.5–1 Mbps at 60 Hz.
- **Session-info frames** (sent on session changes, on target resync, and every 10 s as a fallback) send the static prefix — irsdk header + variable descriptors + session YAML — compressed to ~60–150 KB. The status field is zeroed in the prefix; target writes varBuf first then the irsdk header on every partial frame, so `status=1` is never visible before varBuf is populated.
- **target** receives, reassembles, and decompresses the data into a matching shared memory region on the SimHub PC, so SimHub sees iRacing as running locally. The map is created on first data arrival and closed cleanly if no data is received for 10 s.
- Heartbeat packets keep the connection alive across loading screens and menus so SimHub doesn't disconnect mid-session.

Both tools print a stats line every 5 s (`60 msg/s  0.48 Mbps  200/280/340 µs p50/p99/max`) and a summary on Ctrl-C.

---

## Compatible apps

Any app that reads iRacing shared memory works automatically on the target machine — the memory map is identical to what iRacing produces locally.

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

## Direct Ethernet setup

A direct Ethernet cable between the two PCs (no router, no switch) gives the lowest possible latency — typically **~9 µs p50** vs ~100–200 µs over a LAN switch. You need:

- A network adapter on each PC (PCIe/M.2 cards work well; USB adapters also work)
- A Cat 5e or better Ethernet cable
- Static IP addresses (Windows won't auto-assign usable IPs on a direct link)

**1. Assign static IPs**

On each PC, set a static IP on the direct-link adapter:

| PC | IP | Subnet |
|----|-----|--------|
| iRacing PC | `192.168.50.1` | `255.255.255.0` |
| SimHub PC | `192.168.50.2` | `255.255.255.0` |

In Windows: *Network & Internet → Change adapter options → right-click adapter → Properties → IPv4 → Use the following IP address*. Leave gateway and DNS blank.

**2. Firewall rules (on both PCs)**

Add an inbound UDP rule for port 5000 on **both** machines:

```
New-NetFirewallRule -DisplayName "iRacing Teleport" -Direction Inbound -Protocol UDP -LocalPort 5000 -Action Allow
```

Or via *Windows Defender Firewall → Advanced Settings → Inbound Rules → New Rule → Port → UDP → 5000 → Allow*.

**3. Bat files**

`start-source.bat` on the **iRacing PC** — bind source to port 5000 so resync requests from target are covered by the firewall rule above:

```batch
@echo off
cd /d "D:\Simracing"
source.exe --unicast --target 192.168.50.2:5000 --bind 192.168.50.1:5000
pause
```

`start-target.bat` on the **SimHub PC**:

```batch
@echo off
cd /d "D:\Simracing"
target.exe --unicast --bind 192.168.50.2:5000
pause
```

> **Why `--bind 192.168.50.1:5000` on source?** Source needs to receive 1-byte resync requests from target so it can send a fresh session-info frame immediately on first connect. If source binds to an ephemeral port (`:0`), Windows assigns a random port number that isn't covered by the port 5000 firewall rule, so resync is silently blocked and SimHub takes up to 10 seconds to activate instead of ~1 second.

---

<details>
<summary id="technical-details">Technical details</summary>

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

- **Partial frames**: iRacing's header exposes a ring of up to 4 variable buffers (~5–15 KB each). source reads the highest-tick slot each frame and sends only that slice, cutting per-frame data from ~1.1 MB to ~5–15 KB and fragment count from ~23 to 1. Each partial frame includes the 112-byte irsdk header so target has current `tickCount` values and picks the right varBuf slot after a ring rotation.
- **Session-info frames**: sent on session changes, on target resync request, and every 10 s as a fallback. These send only the **static prefix** — irsdk header + variable descriptors + session YAML — compressed to ~60–150 KB (~7–17 fragments, down from ~20+ for the full map). The `status` field (bytes [4..8]) is zeroed before compressing. On the target, the prefix is written to the map skipping bytes [4..8], so the map's status stays at 0 (fresh) or its previous value (session update). `status=1` is written exclusively by the **partial frame handler**, after varBuf data is already in place.
- **Write ordering on partial frames**: target writes varBuf data first, then the irsdk header last. The irsdk header contains `status=1` from iRacing's live data; writing it after varBuf means `status=1` is visible only once the variable data is already in place.
- **Bidirectional resync**: target sends a 1-byte UDP packet to source when it needs a session-info frame; source responds on the next tick instead of waiting for the 10 s fallback. Requires source to bind to a known port (not ephemeral `:0`) so the request passes through the firewall — see [Direct Ethernet setup](#direct-ethernet-setup).
- **2 MB socket buffers** on both sides (via `socket2`) — the OS default of 64 KB drops all but the first 7 fragments of a session-info frame.
- **Receiver bounds validation** — datagram headers are checked before any buffer allocation: `fragments` is capped at 256 and `payload_size` at the pre-allocated maximum. A malformed or spoofed packet on the LAN is silently discarded.
- **Zero-allocation hot path** — compression and decompression write into pre-allocated buffers; no heap allocation per frame.
- **1 ms timer resolution** — source and target call `timeBeginPeriod(1)` so Windows sleep and event waits resolve at 1 ms granularity rather than the default 15.6 ms.
- **MMCSS on target** — registers under the Windows "Games" multimedia task for reserved CPU time and lower jitter. Not applied to source to avoid competing with iRacing's own registrations.
- **Shared memory security** — the target map and data-ready event are created with a NULL DACL (explicit "allow all access"), matching iRacing's own shared memory, so any process can open them regardless of elevation or user account.

Release profile uses LTO and a single codegen unit.

</details>

<details>
<summary>Improvements over sklose/iracing-teleport</summary>

Rewritten from scratch based on [sklose/iracing-teleport](https://github.com/sklose/iracing-teleport). Main differences:

- **Partial frames**: sends only the active variable buffer (~5–15 KB) per frame instead of the full 1.1 MB map; latency drops from ~1.4 ms to ~200–500 µs on a typical LAN, ~9 µs on a direct Ethernet link.
- Each partial frame includes the 112-byte irsdk header so target has current `tickCount` values and picks the right varBuf slot after a ring rotation.
- Session-info frames send only the static prefix (~60–150 KB compressed) with `status` zeroed; partial frames write varBuf first then the irsdk header last. `status=1` only becomes visible after varBuf is populated, so SimHub's independent `irsdk_header.status` poll never sees `status=1` with empty telemetry values.
- **Bidirectional resync**: target sends a 1-byte UDP packet to source when it needs a session-info frame; source responds on the next tick instead of waiting for a fixed timer.
- **Direct Ethernet support**: documented static IP setup, firewall rules, and bat files for point-to-point cable connections achieving ~9 µs p50 latency.
- **MMCSS on target**: registers under the Windows "Games" multimedia task for reserved CPU time; skipped on source to avoid competing with iRacing's own registrations.
- **NULL DACL shared memory**: target map and event created with explicit "allow all" security descriptor, matching iRacing's own setup, so SimHub and other apps can open the map regardless of elevation.
- Heartbeat packets during menus and loading screens prevent SimHub from disconnecting between sessions.
- `IRSDK_ST_CONNECTED` is zeroed before closing the target map so SimHub sees a clean disconnect.
- Stats show p50/p99/max latency per window with end-to-end measurement (source processing + network transit).
- Socket buffers set to 2 MB on both sides; the original used the OS default, which is smaller than one full frame.
- `repr(C, packed)` wire header with compile-time size and layout assertions; the original's `repr(C)` added 4 bytes of trailing padding.
- Receive path uses `ptr::read_unaligned`; reading a packed struct through a reference is undefined behaviour when unaligned.
- Receiver validates datagram header bounds before allocating — `fragments` and `payload_size` are capped so a malformed packet cannot cause an out-of-memory crash.
- Pre-allocated compression buffer; the original allocated a new `Vec` per frame.
- source waits indefinitely for iRacing to start; the original exited after 5 seconds.
- Shared memory region size read via `VirtualQuery` rather than a hardcoded constant.
- `Drop` guards with null and `INVALID_HANDLE_VALUE` checks on all handles.

</details>

---

## License

MIT
