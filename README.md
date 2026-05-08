# iRacing Teleport

Stream iRacing telemetry over your local network so SimHub (or any iRacing-compatible
app) runs on a separate machine from your iRacing installation. Two small Windows
executables, no installers, no dependencies.

```
┌─────────────────────────┐         UDP (multicast or unicast)        ┌─────────────────────────┐
│     iRacing PC          │  ────────────────────────────────────►   │     SimHub PC           │
│                         │                                           │                         │
│  iRacing                │                                           │  SimHub / overlays      │
│    └─ shared memory     │                                           │    └─ shared memory     │
│         └─ source.exe   │                                           │         └─ target.exe   │
└─────────────────────────┘                                           └─────────────────────────┘
```

**For all games in one app:** [sim-bridge](https://github.com/t-hovestadt/sim-bridge)
bundles iRacing Teleport with AC Teleport and Sim Relay. One binary,
automatic game detection, no manual switching.

**Companion projects:**
- [ac-teleport](https://github.com/t-hovestadt/ac-teleport) — Assetto Corsa / ACE (shared memory)
- [sim-relay](https://github.com/t-hovestadt/sim-relay) — games that broadcast UDP natively
- [sim-bridge](https://github.com/t-hovestadt/sim-bridge) — unified single-binary launcher for all three

---

## Download

Pre-built Windows x64 binaries are on the [Releases](../../releases/latest) page.

| File | Machine |
|------|---------|
| `source.exe` | iRacing PC |
| `target.exe` | SimHub PC |
| `teleport.exe` | Either — combined CLI (`teleport source` / `teleport target`) |

---

## Windows SmartScreen

On first run, Windows may show "Windows protected your PC." This is normal for unsigned open-source software.

To unblock: right-click the `.exe` → **Properties** → check **Unblock** at the bottom of the General tab → **OK**.

Or click **More info** on the SmartScreen dialog, then **Run anyway**.

---

## Quick start

**Default (multicast — works on most home networks):**

1. Run `target.exe` on your SimHub PC
2. Run `source.exe` on your iRacing PC

Start them in any order. Source waits for iRacing to launch; target waits for data.

**Unicast (if multicast doesn't work on your network):**

```
# SimHub PC
target.exe --unicast

# iRacing PC (replace with your SimHub machine's IP)
source.exe --unicast --target 192.168.1.50:5000
```

**Direct Ethernet (point-to-point cable between the two PCs):**

See [Direct Ethernet setup](#direct-ethernet-setup) below.

---

## Options

| Flag | source | target | Default | Description |
|------|:------:|:------:|---------|-------------|
| `--bind <ADDR>` | ✓ | ✓ | `0.0.0.0:0` / `0.0.0.0:5000` | Local UDP socket address |
| `--target <ADDR>` | ✓ | | `239.255.0.1:5000` | Destination (multicast group:port or unicast IP:port) |
| `--unicast` | ✓ | ✓ | off | Direct host-to-host instead of multicast |
| `--group <ADDR>` | | ✓ | `239.255.0.1` | Multicast group to join |
| `--busy-wait` | ✓ | ✓ | off | Spin instead of sleeping (~0–2 ms less jitter, one core). Safe on SimHub PC; avoid on iRacing PC. |
| `--datagram-size <BYTES>` | ✓ | | `9000` | UDP payload bytes per fragment. Use `1472` on standard 1500-byte MTU links to avoid IP fragmentation. Target auto-detects sender's size. |
| `--no-delta` | ✓ | | off | Disable XOR-delta compression; send full frames every tick. Higher bandwidth, zero reconstruction risk on very lossy links. |
| `--keyframe-interval <N>` | ✓ | | `60` | Partial frames between full keyframes when delta is enabled. Lower values are safer on lossy links. |
| `--pin-core <N>` | ✓ | ✓ | off | Pin worker thread to CPU core N (0-based) |
| `--fanalab` | | ✓ | off | Spawn a dummy `iRacingSim64DX11.exe` so FanaLab detects iRacing and auto-loads per-car LED profiles |
| `--reconnect-timeout <SECS>` | ✓ | | `10` | Seconds without telemetry before closing and reconnecting to iRacing. Increase for simulators with long session reload times. |
| `--stale-timeout <SECS>` | | ✓ | `10` | Seconds without data before closing the telemetry map. Increase for long loading screens. |
| `--high-priority` | ✓ | ✓ | off | `HIGH_PRIORITY_CLASS` for lower scheduling jitter. Safe on SimHub PC; avoid on iRacing PC (competes with the game). |

---

## How it works

### Source

Source maps iRacing's shared memory region (`Local\IRSDKMemMapFileName`, ~1.1 MB)
and waits for the data-ready event (`Local\IRSDKDataValidEvent`). On each tick:

1. Check which varBuf slot has the highest `tickCount` — that's the active slot.
2. Check if `tickCount` advanced since the last send. If not (common during
   loading screens and sub-60 Hz operation), skip the frame.
3. Copy the ~5–15 KB variable buffer slice into a staging buffer.
4. Re-read `tickCount` after copying. If it changed, iRacing overwrote the buffer
   mid-copy (TOCTOU); drop the frame rather than forward corrupted data.
5. If target confirmed delta support: XOR the varBuf against the previous one.
   iRacing telemetry changes only ~5% of bytes per tick — delta frames compress
   4–8× smaller than raw partial frames.
6. LZ4-compress, split into datagrams, send.

A full keyframe (no delta) is sent every 60 ticks (configurable) to prevent
divergence if a delta frame is lost in transit. Both sides reset delta state to
zeros on each session-info frame.

**Session-info frames** (irsdk header + variable descriptors + session YAML,
~60–150 KB) are sent on session changes, on target resync request, and every
10 seconds as a fallback. The `status` field (bytes [4..8]) is zeroed before
sending — `status=1` is set only by the partial-frame handler after varBuf is
written, ensuring SimHub never sees `status=1` with empty telemetry.

**FanaLab LED cleanup**: on clean shutdown (Ctrl-C or shutdown signal), source
opens the iRacing map with write access and zeros the entire 1.1 MB region, then
sleeps 200 ms for FanaLab to read RPM=0 before closing. FanaLab reads the zeroed
RPM and sends the LED-off command to Fanatec wheel firmware. This prevents LEDs
from staying lit after iRacing exits. It fires only on shutdown signal, not on
session transitions.

**FanaLab handle note**: FanaLab holds `Local\IRSDKDataValidEvent` open after
iRacing exits. Early versions of sim-bridge probed this event to detect iRacing;
FanaLab's persistent handle made iRacing appear alive long after it quit. Detection
was reverted to process-name scanning (`iRacingSim64DX11.exe`) which is immune to
this. The event is still used by source to wait for data — it's just not used for
the source-side alive check.

Source waits indefinitely for iRacing to start and reconnects automatically after
exits.

### Target

Target receives, reassembles (out-of-order tolerant), and decompresses data into
a matching shared-memory region created on the SimHub PC. The map and data-ready
event are created with NULL DACL (all access) so any process can open them
regardless of elevation.

**Write ordering guarantee**: target writes varBuf data first, then writes the
irsdk header last. The header contains `status=1` from iRacing's live data. By
writing it last, `status=1` is only visible after varBuf data is already in place.
SimHub polls `irsdk_header.status` independently — it must never see `status=1`
with empty telemetry values.

On stale timeout, target zeros `IRSDK_ST_CONNECTED` before closing the map so
SimHub sees a clean disconnect.

### Capability negotiation

Target sends a 2-byte UDP packet to source when it needs a session-info frame:
- Byte 0: resync flag (`0x01`)
- Byte 1: capability bitfield (bit 0 = delta-capable)

Source responds on the next tick with a session-info frame and enables delta
encoding when bit 0 is set. Old 1-byte targets (no second byte) are treated as
delta-incapable — they receive full frames only, no configuration needed.

Delta capability persists across iRacing reconnects — source retains the
negotiated state for the lifetime of both processes.

For this to work, source must bind to a known port (not ephemeral `:0`) so the
resync packet passes through any firewall rule. See [Direct Ethernet setup](#direct-ethernet-setup)
for the `--bind 192.168.50.1:5000` pattern.

---

## Wire protocol

Each UDP datagram carries a 24-byte header (`repr(C, packed)`, little-endian):

| Field | Type | Description |
|-------|------|-------------|
| `source_us` | u64 | Source-side processing time in microseconds |
| `sequence` | u32 | Monotonically increasing per message |
| `payload_size` | u32 | Total LZ4-compressed bytes across all fragments |
| `buf_offset` | u32 | Byte offset to write decompressed data; `u32::MAX` = session-info frame (write at offset 0); bit 31 set = XOR-delta frame, real offset = `buf_offset & !(1 << 31)` |
| `fragment` | u16 | 0-based fragment index |
| `fragments` | u16 | Total fragment count; `0` = heartbeat (no payload) |

The receiver reassembles fragments out-of-order and discards duplicates. A new
sequence clears any in-progress assembly from the previous one. `fragments` is
capped at 256 and `payload_size` at the pre-allocated maximum — malformed or
spoofed packets are silently discarded.

---

## Stats output

Both tools print a stats line every 5 seconds:

```
[source] 60.0 msg/s  0.47 Mbps  2.3x  12/18/45 µs p50/p99/max  0 dropped
[target] 60.0 msg/s  0.47 Mbps  2.3x  14/22/48 µs p50/p99/max  src: 5/9 µs p50/p99  98% delta  0 dropped
```

The `2.3x` figure is the compression ratio (uncompressed ÷ compressed). Delta
frames typically reach 4–8× when only ~5% of telemetry changes per tick.
The `98% delta` figure is the fraction of frames sent as delta (vs full keyframes).

---

## Compatible apps

Any app that reads iRacing shared memory works automatically on the target machine.

**Dashboards and overlays**
- [SimHub](https://www.simhubdash.com) — dashboards, overlays, haptics, LED control
- [RaceLab](https://racelab.app) — modern overlay suite
- [iOverlay](https://ioverlay.app) — standings and timing overlays
- [Z1 Dashboard](https://www.z1racetech.com) — live telemetry and lap analysis

**Haptics and bass shakers**
- [Track Impulse](https://track-impulse.com) — dedicated haptic engine, reads iRacing's 360 Hz sub-samples
- [ButtKicker HaptiConnect](https://thebuttkicker.com) — haptic feedback
- [irFFB](https://github.com/nlp80/irFFB) — FFB enhancement using 360 Hz telemetry

**Wheel hardware**
- [FanaLab](https://fanatec.com/fanalab) — per-car profiles for Fanatec wheels (use `--fanalab` flag)
- [FanaBridge](https://github.com/kelchm/FanaBridge) — SimHub plugin for Fanatec LED/display

**Spotter and coaching**
- [Crew Chief](https://thecrewchief.org) — AI spotter and engineer
- [VRS](https://virtualracingschool.com) — coaching overlays with reference lap comparison
- [Trophi.ai](https://trophi.ai) — AI real-time voice coaching

---

## Direct Ethernet setup

A direct Ethernet cable between the two PCs gives the lowest possible latency —
typically **~11 µs end-to-end p50** (~7 µs on the wire, ~4 µs source-side).

**1. Assign static IPs**

| PC | IP | Subnet |
|----|-----|--------|
| iRacing PC | `192.168.50.1` | `255.255.255.0` |
| SimHub PC | `192.168.50.2` | `255.255.255.0` |

In Windows: *Network & Internet → Change adapter options → right-click adapter →
Properties → IPv4 → Use the following IP address*. Leave gateway and DNS blank.

**2. Firewall rules** (run as Administrator)

On the **iRacing PC** (receives resync packets from SimHub PC):
```powershell
New-NetFirewallRule -DisplayName "iRacing Teleport source" `
    -Direction Inbound -Protocol UDP -LocalPort 5000 -Action Allow
```

On the **SimHub PC** (receives telemetry from iRacing PC):
```powershell
New-NetFirewallRule -DisplayName "iRacing Teleport target" `
    -Direction Inbound -Protocol UDP -LocalPort 5000 -Action Allow
```

**3. NIC settings (both PCs)**

Device Manager → Network Adapters → right-click the direct-link adapter →
Properties:

| Setting | Value |
|---------|-------|
| Energy Efficient Ethernet | Disabled |
| Interrupt Moderation / Interrupt Throttle Rate | Disabled |
| Wake on Magic Packet | Disabled |
| Wake on Pattern Match | Disabled |
| Auto MDI/MDIX | Auto |
| Speed & Duplex | 1.0 Gbps Full Duplex |

Power Management: uncheck "Allow the computer to turn off this device" and
"Allow this device to wake the computer."

**4. Bat files**

`start-source.bat` on the **iRacing PC** — bind source to port 5000 so the
firewall rule above covers resync packets from target:

```batch
@echo off
cd /d "D:\Simracing"
source.exe --unicast --target 192.168.50.2:5000 --bind 192.168.50.1:5000
pause
```

> **Why `--bind 192.168.50.1:5000` on source?** Source receives 2-byte resync
> packets from target so it can send a fresh session-info frame immediately on
> first connect. Without `--bind`, Windows assigns a random ephemeral port that
> isn't covered by the port 5000 firewall rule — resync is silently blocked and
> SimHub takes up to 10 seconds to activate instead of ~1 second.

`start-target.bat` on the **SimHub PC**:

```batch
@echo off
cd /d "D:\Simracing"
target.exe --unicast --bind 192.168.50.2:5000
pause
```

**Troubleshooting**

*Adapter shows Disconnected despite cable plugged in:* Do a full Shut down (not
Restart), wait 30–60 seconds, then power on. Disable Wake-on-LAN in NIC settings
and BIOS (look for "Wake on LAN" or "PCIe ASPM").

*Link won't establish between two NICs:* The Speed & Duplex setting above (1.0 Gbps
Full Duplex) fixes this. Confirm Auto MDI/MDIX is Auto — if disabled, a
straight-through cable won't link without a crossover cable.

*Can't set static IP via PowerShell (`element not found`):* Plug cable in first
(adapter needs a link), then set IP. To reset:
`Remove-NetIPAddress -InterfaceIndex <N> -Confirm:$false`.

---

## Library API

iRacing Teleport is a library crate (`teleport = { path = "…/teleport" }`).
Public API surface used by sim-bridge:

```rust
pub struct SourceConfig {
    pub bind: String,                  // "0.0.0.0:0"
    pub target: String,                // required
    pub unicast: bool,
    pub busy_wait: bool,
    pub pin_core: Option<usize>,
    pub high_priority: bool,
    pub reconnect_timeout_secs: u64,   // 10
    pub datagram_size: usize,          // 9000
    pub no_delta: bool,
    pub keyframe_interval: u16,        // 60
    pub fanalab: bool,
    pub stale_timeout_secs: u64,       // 10
    pub on_first_data: Option<Arc<dyn Fn() + Send + Sync>>,
    pub on_stale: Option<Arc<dyn Fn() + Send + Sync>>,
}

pub fn run_source(config: SourceConfig, shutdown: Receiver<()>) -> io::Result<()>;

pub struct TargetConfig {
    pub bind: String,                  // "0.0.0.0:5000"
    pub unicast: bool,
    pub multicast_group: String,       // "239.255.0.1"
    pub busy_wait: bool,
    pub pin_core: Option<usize>,
    pub fanalab: bool,
    pub stale_timeout_secs: u64,       // 10
    pub high_priority: bool,
    pub on_first_data: Option<Arc<dyn Fn() + Send + Sync>>,
    pub on_stale: Option<Arc<dyn Fn() + Send + Sync>>,
}

pub fn run_target(config: TargetConfig, shutdown: Receiver<()>) -> io::Result<()>;

pub const DEFAULT_PORT: u16 = 5000;
pub const DEFAULT_MULTICAST: &str = "239.255.0.1";
```

`on_first_data` fires when the first complete session-info frame is received.
`on_stale` fires when the stale timeout expires and the map is closed.
sim-bridge uses these to call `SimHubWPF.exe -switchgame` and kill stubs.

---

## Building from source

Requires [Rust](https://rustup.rs) (stable).

```
git clone https://github.com/t-hovestadt/iracing-teleport
cd iracing-teleport
cargo build --release
```

Cross-compile for Windows from macOS or Linux:

```
rustup target add x86_64-pc-windows-gnu
brew install mingw-w64   # macOS
CARGO_TARGET_DIR=/tmp/iracing-build cargo build --release --target x86_64-pc-windows-gnu
```

If your working directory path contains spaces, set `CARGO_TARGET_DIR` to a
path without spaces — the `mingw-w64` linker doesn't handle quoted paths.

---

<details>
<summary>Technical details</summary>

### Shared memory layout

iRacing exposes a single 1.1 MB shared-memory region at `Local\IRSDKMemMapFileName`.
The region contains:
- 112-byte irsdk header at offset 0 (contains `status`, `tickCount`, varBuf ring)
- Up to 4 variable buffers (varBufs) of ~5–15 KB each, at offsets from the header
- Variable descriptors (names, types, units for each telemetry value)
- Session YAML string (car setup, session info, driver info)

Only the **active varBuf** (highest `tickCount`) is sent per frame — ~5–15 KB
vs the full 1.1 MB map. Fragment count drops from ~23 to ~1.

### XOR-delta encoding

When the target confirms delta support, source XORs the current varBuf payload
against the previous one before LZ4 compression:

```
delta[i] = current[i] ^ previous[i]
```

Processed in 8-byte chunks so LLVM auto-vectorizes to SSE2/AVX2. The result
is stored in `buf_offset` bit 31. Reconstruction on target:

```
current[i] = delta[i] ^ previous[i]
```

Both sides keep a `prev_varbuf` buffer. Delta state resets to zeros on each
session-info frame so sender and receiver stay in sync across session changes.

### TOCTOU guard

`as_slice()` is a live pointer into iRacing's shared memory. Source copies a
varBuf slot into a staging buffer, then re-reads that slot's `tickCount`. If it
changed, iRacing overwrote the buffer during the copy — the frame is dropped
and counted as lost rather than forwarding corrupt data.

### Performance design

- **2 MB socket buffers** (via `socket2`) — OS default (64 KB on Windows) is
  smaller than one full session-info frame.
- **Zero-allocation hot path** — compression into pre-allocated buffers. Fragment
  reassembly uses an inline `[bool; 256]` received-map with no per-sequence heap
  allocation.
- **Duplicate-tick detection** — source skips compress+send when `tickCount`
  hasn't advanced (loading screens, sub-60 Hz operation). Saves a full LZ4 pass
  and socket send.
- **1 ms timer resolution** — `timeBeginPeriod(1)` on both source and target.
- **MMCSS on target** — "Games" multimedia task for reserved CPU time and lower
  jitter. Not applied to source (would compete with iRacing's own registration).
- **NULL DACL shared memory** — created with explicit "all access" security so
  any process can open regardless of elevation.

Release profile uses LTO, single codegen unit, symbol stripping, and
`panic = "abort"`.

### Improvements over sklose/iracing-teleport

This project was rewritten from scratch based on
[sklose/iracing-teleport](https://github.com/sklose/iracing-teleport). Key differences:

- Partial frames: ~5–15 KB per tick vs full 1.1 MB map (latency drops from ~1.4 ms to ~11 µs on direct Ethernet)
- XOR-delta encoding: 4–8× additional compression on top of LZ4
- Capability negotiation: target advertises delta support; old 1-byte targets receive full frames
- Torn-frame detection (TOCTOU guard): drops mid-copy corrupted frames
- Bidirectional resync: target requests session-info immediately, not on a fixed timer
- `repr(C, packed)` wire header with compile-time size assertions (original used `repr(C)` with trailing padding)
- 2 MB socket buffers (original used OS default, too small for session-info frames)
- Receiver bounds validation before allocation: `fragments` capped at 256, `payload_size` capped at pre-allocated max
- Zero-allocation hot path: pre-allocated compression + reassembly buffers
- Duplicate-tick detection: skips redundant sends during loading screens
- `IRSDK_ST_CONNECTED` zeroed before closing target map (clean disconnect signal)
- `status=1` written only after varBuf is populated (write-ordering guarantee)
- Heartbeats during menus/loading prevent SimHub from disconnecting mid-session
- Source waits indefinitely for iRacing; original exited after 5 seconds
- Stats show p50/p99/max latency with end-to-end measurement

</details>

---

## License

MIT
