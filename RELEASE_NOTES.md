Stream iRacing telemetry to a SimHub PC over your local network.
Two small executables, no installers, no dependencies.

## Downloads

| File | Machine | Description |
|------|---------|-------------|
| `source.exe` | iRacing PC | Reads shared memory, compresses, sends over UDP |
| `target.exe` | SimHub PC | Receives, decompresses, writes to local shared memory |
| `teleport.exe` | Either | Combined source+target in one binary (subcommands) |

## Quick start

**SimHub PC:**
```
target.exe
```

**iRacing PC:**
```
source.exe
```

For unicast (direct connection):
```
target.exe --unicast
source.exe --unicast --target 192.168.1.50:5000
```

## What's included

### Performance
- **CPU 0 exclusion** — both source and target automatically avoid CPU 0 to prevent competing with iRacing's sim thread, eliminating Type B stutters (15–27 ms frame time spikes). Disable with `--no-cpu-exclude`.
- **2 MB socket buffers** — OS defaults drop frames; buffers are large enough for the entire 1.1 MB telemetry frame
- **Zero-allocation hot path** — no heap allocations during the send/receive loop
- **Zero-copy decompression** — decompresses directly into shared memory, skipping the intermediate buffer
- **LTO + single codegen unit** — maximum cross-crate inlining

### Low-latency options (identical on both source and target)
- **`--busy-wait`** — spin instead of sleeping, minimising OS scheduler jitter at the cost of one CPU core. On source: spins on the iRacing data-ready event. On target: spins on recv.
- **`--high-priority`** — set `HIGH_PRIORITY_CLASS` so the OS scheduler prefers the telemetry thread
- **`--pin-core <N>`** — pin to a specific CPU core for cache-friendly execution

### FanaLab / fanatec-tuner support
- **`--fanalab`** (target recommended) — spawns a dummy `iRacingSim64DX11.exe` process so FanaLab and fanatec-tuner detect iRacing on the SimHub PC. Automatically killed on Ctrl-C.
- **`--zero-on-exit`** (target) — zeros the shared-memory map on shutdown so SimHub dashboards immediately show 0 RPM / stopped state instead of stale data. When using fanatec-tuner, wheel LEDs clear on its own stale timeout (≤10 s after iracing-teleport stops).

### Reliability
- **Proper reconnect** — waits indefinitely for iRacing to start (original exited after 5 seconds)
- **No undefined behavior** — safe unaligned reads replace packed struct references
- **Drop guards** — null and invalid handle checks before every handle close
- **VirtualQuery** — discovers actual shared memory region size instead of hardcoding

### Protocol improvements vs upstream
- Wire header reduced from 24 to 20 bytes (no padding)
- Target address parsed once, not 23 times per frame
- Pre-allocated compression buffer (no per-frame allocation)
- Out-of-order fragment reassembly with duplicate detection

## Full documentation

See the [README](https://github.com/t-hovestadt/iracing-teleport/blob/rewrite2/README.md) for all options and technical details.

## Credits

Originally forked from [sklose/iracing-teleport](https://github.com/sklose/iracing-teleport).
Reimplemented from scratch with all improvements listed above.
