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
- **CPU 0 exclusion** — source.exe automatically avoids CPU 0 to prevent competing with iRacing's sim thread, eliminating Type B stutters (15–27 ms frame time spikes). Disable with `--no-cpu-exclude` if not running alongside iRacing.
- **2 MB socket buffers** — OS defaults drop frames; buffers are large enough for the entire 1.1 MB telemetry frame
- **Zero-allocation hot path** — no heap allocations during the send/receive loop
- **Zero-copy decompression** — decompresses directly into shared memory, skipping the intermediate buffer
- **LTO + single codegen unit** — maximum cross-crate inlining

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
