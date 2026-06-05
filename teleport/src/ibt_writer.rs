//! iRacing `.ibt` telemetry disk-file writer.
//!
//! Opt-in via the target's `--write-ibt` flag. The target already reconstructs a
//! byte-for-byte copy of iRacing's live shared-memory map (header + variable
//! descriptors + session YAML + the live variable buffer). Disk-based telemetry
//! tools such as Garage 61 do not read that shared memory — they read the `.ibt`
//! files iRacing writes to `Documents\iRacing\telemetry\`. On the target PC there
//! is no iRacing process to write them, so those tools see nothing.
//!
//! This module closes that gap: it serialises the reconstructed map to a real
//! `.ibt` file on the target so disk-based tools can read teleported telemetry
//! locally.
//!
//! The on-disk format is taken from the iRacing SDK (`irsdk_defines.h` and the
//! reference writer `irsdk_csv2ibt/csv2ibt.cpp`) and validated byte-for-byte
//! against real `.ibt` files. Layout:
//!
//! ```text
//! [0   : 112]  irsdk_header         ver, status, tickRate, offsets, numBuf=1, bufLen, varBuf[4]
//! [112 : 144]  irsdk_diskSubHeader  sessionStartDate, start/end SessionTime, lap + record counts
//! [144 : S  ]  irsdk_varHeader[N]   144 bytes each            (copied verbatim from the live map)
//! [S   : B  ]  sessionInfo YAML     sessionInfoLen bytes      (copied verbatim from the live map)
//! [B   : EOF]  varBuffer            sessionRecordCount rows of bufLen bytes each
//! ```
//! where `S = 144 + N*144` and `B = S + sessionInfoLen`.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

// ── irsdk_header field offsets (all i32 little-endian unless noted) ────────────
const OFF_VER: usize = 0;
const OFF_STATUS: usize = 4;
const OFF_TICKRATE: usize = 8;
const OFF_SESSIONINFOUPDATE: usize = 12;
const OFF_SESSIONINFOLEN: usize = 16;
const OFF_SESSIONINFOOFFSET: usize = 20;
const OFF_NUMVARS: usize = 24;
const OFF_VARHEADEROFFSET: usize = 28;
const OFF_NUMBUF: usize = 32;
const OFF_BUFLEN: usize = 36;
// pad1[2] occupies [40..48]
const OFF_VARBUF: usize = 48; // varBuf[4], each 16 bytes: tickCount(i32) bufOffset(i32) pad[2]

const HEADER_SIZE: usize = 112;
const DISK_SUBHEADER_SIZE: usize = 32;
const VARHEADER_SIZE: usize = 144;
// within one irsdk_varHeader: type(4) offset(4) count(4) countAsTime(1)+pad(3) then name[32]
const VH_TYPE_OFF: usize = 0;
const VH_OFFSET_OFF: usize = 4;
const VH_NAME_OFF: usize = 16;
const VH_NAME_LEN: usize = 32;

const STATUS_CONNECTED: i32 = 1;
const IRSDK_VER: i32 = 2;

#[inline]
fn rd_i32(b: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Locate the active varBuf slot (the one with the highest tickCount) in a live
/// irsdk map header. Returns `(tick_count, buf_offset)`.
fn active_varbuf(map: &[u8]) -> Option<(i32, usize)> {
    if map.len() < HEADER_SIZE {
        return None;
    }
    let num_buf = rd_i32(map, OFF_NUMBUF).clamp(1, 4) as usize;
    let mut best: Option<(i32, usize)> = None;
    for i in 0..num_buf {
        let base = OFF_VARBUF + i * 16;
        let tc = rd_i32(map, base);
        let bo = rd_i32(map, base + 4) as usize;
        if best.is_none_or(|(btc, _)| tc > btc) {
            best = Some((tc, bo));
        }
    }
    best
}

/// Find a variable's byte offset within a sample row by name, scanning the
/// varHeader array. Used to locate `SessionTime` (double) and `Lap` (int) for the
/// disk subheader. Returns `(row_offset, type_index)`.
fn find_var(map: &[u8], vh_off: usize, num_vars: usize, name: &str) -> Option<(usize, i32)> {
    for i in 0..num_vars {
        let base = vh_off + i * VARHEADER_SIZE;
        if base + VARHEADER_SIZE > map.len() {
            break;
        }
        let nb = &map[base + VH_NAME_OFF..base + VH_NAME_OFF + VH_NAME_LEN];
        let end = nb.iter().position(|&c| c == 0).unwrap_or(nb.len());
        if nb[..end] == *name.as_bytes() {
            let row_off = rd_i32(map, base + VH_OFFSET_OFF) as usize;
            let vtype = rd_i32(map, base + VH_TYPE_OFF);
            return Some((row_off, vtype));
        }
    }
    None
}

/// Static description of a live map's layout, extracted once when a file opens.
struct MapLayout {
    ver: i32,
    tick_rate: i32,
    session_info_update: i32,
    num_vars: usize,
    buf_len: usize,
    vh_off: usize,                       // varHeader array offset in the LIVE map
    si_off: usize,                       // sessionInfo (YAML) offset in the LIVE map
    si_len: usize,                       // sessionInfo length
    session_time_row_off: Option<usize>, // byte offset of SessionTime (double) in a row
    lap_row_off: Option<usize>,          // byte offset of Lap (int) in a row
}

impl MapLayout {
    /// Parse a live irsdk map. Returns `None` if it is not a populated, connected
    /// session (no point writing a file with no variables or no YAML).
    fn parse(map: &[u8]) -> Option<MapLayout> {
        if map.len() < HEADER_SIZE {
            return None;
        }
        if rd_i32(map, OFF_STATUS) & STATUS_CONNECTED == 0 {
            return None;
        }
        let num_vars = rd_i32(map, OFF_NUMVARS) as usize;
        let buf_len = rd_i32(map, OFF_BUFLEN) as usize;
        let vh_off = rd_i32(map, OFF_VARHEADEROFFSET) as usize;
        let si_off = rd_i32(map, OFF_SESSIONINFOOFFSET) as usize;
        let si_len = rd_i32(map, OFF_SESSIONINFOLEN) as usize;
        if num_vars == 0 || buf_len == 0 || si_len == 0 {
            return None;
        }
        // Bounds-check that the static regions actually fit in the map.
        if vh_off + num_vars * VARHEADER_SIZE > map.len() || si_off + si_len > map.len() {
            return None;
        }
        let session_time_row_off = find_var(map, vh_off, num_vars, "SessionTime").map(|(o, _)| o);
        let lap_row_off = find_var(map, vh_off, num_vars, "Lap").map(|(o, _)| o);
        Some(MapLayout {
            ver: rd_i32(map, OFF_VER),
            tick_rate: rd_i32(map, OFF_TICKRATE),
            session_info_update: rd_i32(map, OFF_SESSIONINFOUPDATE),
            num_vars,
            buf_len,
            vh_off,
            si_off,
            si_len,
            session_time_row_off,
            lap_row_off,
        })
    }
}

/// Build the disk-file static prefix: header (112) + subheader placeholder (32) +
/// varHeaders + YAML. The subheader is written as a placeholder here and rewritten
/// with final counts on close. `start_date` is the wall-clock session start
/// (unix seconds); injectable for deterministic tests.
fn build_prefix(map: &[u8], layout: &MapLayout, start_date: i64) -> Vec<u8> {
    let si_disk_off = HEADER_SIZE + DISK_SUBHEADER_SIZE + layout.num_vars * VARHEADER_SIZE;
    let buf_off = si_disk_off + layout.si_len;

    let mut out = Vec::with_capacity(buf_off);

    // ── irsdk_header (112 bytes) ──
    let mut hdr = [0u8; HEADER_SIZE];
    let ver = if layout.ver != 0 {
        layout.ver
    } else {
        IRSDK_VER
    };
    let tick_rate = if layout.tick_rate != 0 {
        layout.tick_rate
    } else {
        60
    };
    hdr[OFF_VER..OFF_VER + 4].copy_from_slice(&ver.to_le_bytes());
    hdr[OFF_STATUS..OFF_STATUS + 4].copy_from_slice(&STATUS_CONNECTED.to_le_bytes());
    hdr[OFF_TICKRATE..OFF_TICKRATE + 4].copy_from_slice(&tick_rate.to_le_bytes());
    hdr[OFF_SESSIONINFOUPDATE..OFF_SESSIONINFOUPDATE + 4]
        .copy_from_slice(&layout.session_info_update.to_le_bytes());
    hdr[OFF_SESSIONINFOLEN..OFF_SESSIONINFOLEN + 4]
        .copy_from_slice(&(layout.si_len as i32).to_le_bytes());
    hdr[OFF_SESSIONINFOOFFSET..OFF_SESSIONINFOOFFSET + 4]
        .copy_from_slice(&(si_disk_off as i32).to_le_bytes());
    hdr[OFF_NUMVARS..OFF_NUMVARS + 4].copy_from_slice(&(layout.num_vars as i32).to_le_bytes());
    hdr[OFF_VARHEADEROFFSET..OFF_VARHEADEROFFSET + 4]
        .copy_from_slice(&((HEADER_SIZE + DISK_SUBHEADER_SIZE) as i32).to_le_bytes());
    hdr[OFF_NUMBUF..OFF_NUMBUF + 4].copy_from_slice(&1i32.to_le_bytes());
    hdr[OFF_BUFLEN..OFF_BUFLEN + 4].copy_from_slice(&(layout.buf_len as i32).to_le_bytes());
    // varBuf[0]: tickCount=0, bufOffset=buf_off; varBuf[1..4] left zeroed.
    hdr[OFF_VARBUF..OFF_VARBUF + 4].copy_from_slice(&0i32.to_le_bytes());
    hdr[OFF_VARBUF + 4..OFF_VARBUF + 8].copy_from_slice(&(buf_off as i32).to_le_bytes());
    out.extend_from_slice(&hdr);

    // ── irsdk_diskSubHeader (32 bytes) placeholder ──
    out.extend_from_slice(&start_date.to_le_bytes()); // time_t (8 bytes on Win64)
    out.extend_from_slice(&0f64.to_le_bytes()); // sessionStartTime
    out.extend_from_slice(&0f64.to_le_bytes()); // sessionEndTime
    out.extend_from_slice(&0i32.to_le_bytes()); // sessionLapCount
    out.extend_from_slice(&0i32.to_le_bytes()); // sessionRecordCount

    // ── varHeaders (copied verbatim from the live map) ──
    out.extend_from_slice(&map[layout.vh_off..layout.vh_off + layout.num_vars * VARHEADER_SIZE]);
    // ── sessionInfo YAML (copied verbatim) ──
    out.extend_from_slice(&map[layout.si_off..layout.si_off + layout.si_len]);

    debug_assert_eq!(out.len(), buf_off);
    out
}

/// Serialise the 32-byte disk subheader from running session statistics.
fn subheader_bytes(start_date: i64, start_t: f64, end_t: f64, laps: i32, records: i32) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[0..8].copy_from_slice(&start_date.to_le_bytes());
    b[8..16].copy_from_slice(&start_t.to_le_bytes());
    b[16..24].copy_from_slice(&end_t.to_le_bytes());
    b[24..28].copy_from_slice(&laps.to_le_bytes());
    b[28..32].copy_from_slice(&records.to_le_bytes());
    b
}

/// An open `.ibt` file plus the running subheader statistics it will be finalised with.
struct OpenIbt {
    file: File,
    layout: MapLayout,
    start_date: i64,
    record_count: i32,
    start_time: f64,
    end_time: f64,
    lap_count: i32,
    last_lap: i32,
    last_tick: i32,
}

/// Writes `.ibt` files to `dir`, one per connected session.
pub struct IbtWriter {
    dir: PathBuf,
    open: Option<OpenIbt>,
}

impl IbtWriter {
    pub fn new(dir: PathBuf) -> Self {
        IbtWriter { dir, open: None }
    }

    fn now_unix() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Pick an unused `telemetry_<unixtime>.ibt`, falling back to `_NN` suffixes,
    /// mirroring the SDK's `openUniqueFile` behaviour so we never clobber a file
    /// another tool may still be reading.
    fn open_unique(&self, stamp: i64) -> std::io::Result<(File, PathBuf)> {
        for i in 0..100 {
            let name = if i == 0 {
                format!("teleport_{stamp}.ibt")
            } else {
                format!("teleport_{stamp}_{i:02}.ibt")
            };
            let path = self.dir.join(name);
            match File::options().write(true).create_new(true).open(&path) {
                Ok(f) => return Ok((f, path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::other("no unused .ibt filename available"))
    }

    /// Open a new file and write its static prefix. Test seam: `start_date` fixed.
    fn start_session(&mut self, map: &[u8], layout: MapLayout, start_date: i64) {
        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            eprintln!("[ibt] cannot create telemetry dir {:?}: {e}", self.dir);
            return;
        }
        let (mut file, path) = match self.open_unique(start_date) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[ibt] cannot open .ibt file: {e}");
                return;
            }
        };
        let prefix = build_prefix(map, &layout, start_date);
        if let Err(e) = file.write_all(&prefix) {
            eprintln!("[ibt] failed writing header: {e}");
            return;
        }
        println!("[ibt] writing telemetry to {}", path.display());
        self.open = Some(OpenIbt {
            file,
            layout,
            start_date,
            record_count: 0,
            start_time: 0.0,
            end_time: 0.0,
            lap_count: 0,
            last_lap: i32::MIN,
            last_tick: i32::MIN,
        });
    }

    /// Called by the target after every frame written to the reconstructed map.
    /// Opens a file on the first valid frame, rolls a new file when the session
    /// string changes, and appends one sample row whenever the tick advances.
    pub fn on_map_update(&mut self, map: &[u8]) {
        let layout = match MapLayout::parse(map) {
            Some(l) => l,
            None => return, // not a populated/connected map yet
        };

        // Roll a new file if the session changed (different sessionInfoUpdate).
        if let Some(open) = &self.open {
            if open.layout.session_info_update != layout.session_info_update {
                self.finish();
            }
        }

        if self.open.is_none() {
            let start_date = Self::now_unix();
            self.start_session(map, layout, start_date);
        }
        self.append_row(map);
    }

    fn append_row(&mut self, map: &[u8]) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        let Some((tick, buf_off)) = active_varbuf(map) else {
            return;
        };
        // Only append when the tick advanced — the same frame can be observed
        // multiple times (session-info frame + partial frames share a map state).
        if tick == open.last_tick {
            return;
        }
        open.last_tick = tick;

        if buf_off + open.layout.buf_len > map.len() {
            return; // malformed; skip this row rather than write garbage
        }
        let row = &map[buf_off..buf_off + open.layout.buf_len];
        if let Err(e) = open.file.write_all(row) {
            eprintln!("[ibt] failed writing sample: {e}");
            return;
        }
        open.record_count += 1;

        // Track SessionTime (double) for start/end times.
        if let Some(off) = open.layout.session_time_row_off {
            if off + 8 <= row.len() {
                let t = f64::from_le_bytes(row[off..off + 8].try_into().unwrap());
                if open.record_count == 1 {
                    open.start_time = t;
                    open.end_time = t;
                } else if t > open.end_time {
                    open.end_time = t;
                }
            }
        }
        // Track Lap (int) for lap count.
        if let Some(off) = open.layout.lap_row_off {
            if off + 4 <= row.len() {
                let lap = rd_i32(row, off);
                if open.record_count == 1 {
                    open.last_lap = lap - 1;
                }
                if lap > open.last_lap {
                    open.lap_count += 1;
                    open.last_lap = lap;
                }
            }
        }
    }

    /// Finalise the current file: rewrite the subheader with final counts, flush,
    /// close. Safe to call when no file is open. Call on stale timeout and shutdown.
    pub fn finish(&mut self) {
        let Some(mut open) = self.open.take() else {
            return;
        };
        let sub = subheader_bytes(
            open.start_date,
            open.start_time,
            open.end_time,
            open.lap_count,
            open.record_count,
        );
        // Real iRacing stores the final global tick in varBuf[0].tickCount of the
        // disk header (the SDK's csv2ibt leaves it 0; readers use recordCount, so
        // either is valid). Match iRacing when we have samples.
        let final_tick = if open.record_count > 0 && open.last_tick != i32::MIN {
            open.last_tick
        } else {
            0
        };
        if let Err(e) = open
            .file
            .seek(SeekFrom::Start(OFF_VARBUF as u64))
            .and_then(|_| open.file.write_all(&final_tick.to_le_bytes()))
            .and_then(|_| open.file.seek(SeekFrom::Start(HEADER_SIZE as u64)))
            .and_then(|_| open.file.write_all(&sub))
            .and_then(|_| open.file.flush())
        {
            eprintln!("[ibt] failed finalising subheader: {e}");
        }
        println!(
            "[ibt] closed file: {} samples, {} laps",
            open.record_count, open.lap_count
        );
    }
}

impl Drop for IbtWriter {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Field offsets in a sample row for the synthetic map.
    const ST_OFF: usize = 0; // SessionTime (double)  bytes 0..8
    const LAP_OFF: usize = 8; // Lap (int)             bytes 8..12
    const SPD_OFF: usize = 12; // Speed (float)        bytes 12..16
    const SYN_BUFLEN: usize = 16;
    const SYN_NUMVARS: usize = 3;

    /// Build one irsdk_varHeader (144 bytes).
    fn varheader(vtype: i32, row_off: i32, name: &str) -> Vec<u8> {
        let mut v = vec![0u8; VARHEADER_SIZE];
        v[0..4].copy_from_slice(&vtype.to_le_bytes());
        v[4..8].copy_from_slice(&row_off.to_le_bytes());
        v[8..12].copy_from_slice(&1i32.to_le_bytes()); // count
        let nb = name.as_bytes();
        v[VH_NAME_OFF..VH_NAME_OFF + nb.len()].copy_from_slice(nb);
        v
    }

    /// Build a synthetic *live* irsdk map (numBuf=1) with the given sample rows
    /// already laid out, header pointing at row `tick` as the active buffer.
    fn synth_map(yaml: &[u8], tick: i32, row: &[u8]) -> Vec<u8> {
        let vh_off = HEADER_SIZE;
        let si_off = vh_off + SYN_NUMVARS * VARHEADER_SIZE;
        let buf_off = si_off + yaml.len();
        let mut map = vec![0u8; buf_off + SYN_BUFLEN];
        // header
        map[OFF_VER..OFF_VER + 4].copy_from_slice(&IRSDK_VER.to_le_bytes());
        map[OFF_STATUS..OFF_STATUS + 4].copy_from_slice(&STATUS_CONNECTED.to_le_bytes());
        map[OFF_TICKRATE..OFF_TICKRATE + 4].copy_from_slice(&60i32.to_le_bytes());
        map[OFF_SESSIONINFOLEN..OFF_SESSIONINFOLEN + 4]
            .copy_from_slice(&(yaml.len() as i32).to_le_bytes());
        map[OFF_SESSIONINFOOFFSET..OFF_SESSIONINFOOFFSET + 4]
            .copy_from_slice(&(si_off as i32).to_le_bytes());
        map[OFF_NUMVARS..OFF_NUMVARS + 4].copy_from_slice(&(SYN_NUMVARS as i32).to_le_bytes());
        map[OFF_VARHEADEROFFSET..OFF_VARHEADEROFFSET + 4]
            .copy_from_slice(&(vh_off as i32).to_le_bytes());
        map[OFF_NUMBUF..OFF_NUMBUF + 4].copy_from_slice(&1i32.to_le_bytes());
        map[OFF_BUFLEN..OFF_BUFLEN + 4].copy_from_slice(&(SYN_BUFLEN as i32).to_le_bytes());
        map[OFF_VARBUF..OFF_VARBUF + 4].copy_from_slice(&tick.to_le_bytes());
        map[OFF_VARBUF + 4..OFF_VARBUF + 8].copy_from_slice(&(buf_off as i32).to_le_bytes());
        // varHeaders
        let mut p = vh_off;
        for vh in [
            varheader(5, ST_OFF as i32, "SessionTime"),
            varheader(2, LAP_OFF as i32, "Lap"),
            varheader(4, SPD_OFF as i32, "Speed"),
        ] {
            map[p..p + VARHEADER_SIZE].copy_from_slice(&vh);
            p += VARHEADER_SIZE;
        }
        // YAML
        map[si_off..si_off + yaml.len()].copy_from_slice(yaml);
        // sample row
        map[buf_off..buf_off + SYN_BUFLEN].copy_from_slice(row);
        map
    }

    fn make_row(session_time: f64, lap: i32, speed: f32) -> Vec<u8> {
        let mut r = vec![0u8; SYN_BUFLEN];
        r[ST_OFF..ST_OFF + 8].copy_from_slice(&session_time.to_le_bytes());
        r[LAP_OFF..LAP_OFF + 4].copy_from_slice(&lap.to_le_bytes());
        r[SPD_OFF..SPD_OFF + 4].copy_from_slice(&speed.to_le_bytes());
        r
    }

    #[test]
    fn synthetic_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ibt_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let yaml = b"---\nWeekendInfo:\n TrackName: testtrack\n...\n";

        // Drive 5 ticks: SessionTime climbs, Lap goes 1,1,2,2,3 (=> 3 laps seen).
        let rows = [
            make_row(100.0, 1, 10.0),
            make_row(100.5, 1, 20.0),
            make_row(101.0, 2, 30.0),
            make_row(101.5, 2, 40.0),
            make_row(102.0, 3, 50.0),
        ];

        {
            let mut w = IbtWriter::new(dir.clone());
            for (i, row) in rows.iter().enumerate() {
                // Same session string throughout; tick advances each frame.
                let map = synth_map(yaml, i as i32 + 1, row);
                // start_date fixed for determinism on the first frame.
                if w.open.is_none() {
                    let layout = MapLayout::parse(&map).unwrap();
                    w.start_session(&map, layout, 1_700_000_000);
                }
                // Feed the frame twice to prove tick-dedup (no double rows).
                w.on_map_update(&map);
                w.on_map_update(&map);
            }
            w.finish();
        }

        // Find the produced file.
        let file = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "ibt"))
            .expect("no .ibt produced");
        let data = std::fs::read(&file).unwrap();

        // ── header checks ──
        assert_eq!(rd_i32(&data, OFF_VER), IRSDK_VER);
        assert_eq!(rd_i32(&data, OFF_STATUS), STATUS_CONNECTED);
        assert_eq!(rd_i32(&data, OFF_TICKRATE), 60);
        assert_eq!(rd_i32(&data, OFF_NUMVARS), SYN_NUMVARS as i32);
        assert_eq!(rd_i32(&data, OFF_BUFLEN), SYN_BUFLEN as i32);
        assert_eq!(rd_i32(&data, OFF_NUMBUF), 1);
        assert_eq!(rd_i32(&data, OFF_VARHEADEROFFSET), 144);
        let si_off = rd_i32(&data, OFF_SESSIONINFOOFFSET) as usize;
        let si_len = rd_i32(&data, OFF_SESSIONINFOLEN) as usize;
        assert_eq!(si_off, 144 + SYN_NUMVARS * VARHEADER_SIZE);
        assert_eq!(si_len, yaml.len());
        let buf_off = rd_i32(&data, OFF_VARBUF + 4) as usize;
        assert_eq!(buf_off, si_off + si_len);

        // ── layout integrity: file is prefix + N*bufLen, exactly ──
        let sample_area = data.len() - buf_off;
        assert_eq!(sample_area % SYN_BUFLEN, 0);
        assert_eq!(sample_area / SYN_BUFLEN, rows.len());

        // ── YAML copied verbatim ──
        assert_eq!(&data[si_off..si_off + si_len], yaml);

        // ── subheader derived correctly ──
        let start_date = i64::from_le_bytes(data[112..120].try_into().unwrap());
        let start_t = f64::from_le_bytes(data[120..128].try_into().unwrap());
        let end_t = f64::from_le_bytes(data[128..136].try_into().unwrap());
        let laps = rd_i32(&data, 136);
        let records = rd_i32(&data, 140);
        assert_eq!(start_date, 1_700_000_000);
        assert_eq!(start_t, 100.0);
        assert_eq!(end_t, 102.0);
        assert_eq!(records, rows.len() as i32);
        // Lap went 1->2->3 after the initial; csv2ibt counts increments from lap-1.
        assert_eq!(laps, 3);

        // ── sample rows byte-identical and in order ──
        for (i, row) in rows.iter().enumerate() {
            let s = buf_off + i * SYN_BUFLEN;
            assert_eq!(&data[s..s + SYN_BUFLEN], row.as_slice(), "row {i} mismatch");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Read a real .ibt and return its components.
    #[allow(clippy::type_complexity)]
    fn load_ibt(
        path: &str,
    ) -> (
        i32,
        i32,
        i32,
        usize,
        usize,
        Vec<u8>,
        Vec<u8>,
        Vec<Vec<u8>>,
        Vec<u8>,
    ) {
        let d = std::fs::read(path).unwrap();
        let ver = rd_i32(&d, OFF_VER);
        let tick_rate = rd_i32(&d, OFF_TICKRATE);
        let si_update = rd_i32(&d, OFF_SESSIONINFOUPDATE);
        let si_len = rd_i32(&d, OFF_SESSIONINFOLEN) as usize;
        let si_off = rd_i32(&d, OFF_SESSIONINFOOFFSET) as usize;
        let num_vars = rd_i32(&d, OFF_NUMVARS) as usize;
        let vh_off = rd_i32(&d, OFF_VARHEADEROFFSET) as usize;
        let buf_len = rd_i32(&d, OFF_BUFLEN) as usize;
        let buf_off = rd_i32(&d, OFF_VARBUF + 4) as usize;
        let varheaders = d[vh_off..vh_off + num_vars * VARHEADER_SIZE].to_vec();
        let yaml = d[si_off..si_off + si_len].to_vec();
        let n = (d.len() - buf_off) / buf_len;
        let rows: Vec<Vec<u8>> = (0..n)
            .map(|i| d[buf_off + i * buf_len..buf_off + (i + 1) * buf_len].to_vec())
            .collect();
        (
            ver, tick_rate, si_update, num_vars, buf_len, varheaders, yaml, rows, d,
        )
    }

    /// Build a synthetic live map (numBuf=1) from real components, one varBuf slot
    /// holding `row`, header pointing at it with tickCount `tick`.
    #[allow(clippy::too_many_arguments)]
    fn live_map_from(
        ver: i32,
        tick_rate: i32,
        si_update: i32,
        num_vars: usize,
        buf_len: usize,
        varheaders: &[u8],
        yaml: &[u8],
        tick: i32,
        row: &[u8],
    ) -> Vec<u8> {
        let vh_off = HEADER_SIZE;
        let si_off = vh_off + num_vars * VARHEADER_SIZE;
        let buf_off = si_off + yaml.len();
        let mut map = vec![0u8; buf_off + buf_len];
        map[OFF_VER..OFF_VER + 4].copy_from_slice(&ver.to_le_bytes());
        map[OFF_STATUS..OFF_STATUS + 4].copy_from_slice(&STATUS_CONNECTED.to_le_bytes());
        map[OFF_TICKRATE..OFF_TICKRATE + 4].copy_from_slice(&tick_rate.to_le_bytes());
        map[OFF_SESSIONINFOUPDATE..OFF_SESSIONINFOUPDATE + 4]
            .copy_from_slice(&si_update.to_le_bytes());
        map[OFF_SESSIONINFOLEN..OFF_SESSIONINFOLEN + 4]
            .copy_from_slice(&(yaml.len() as i32).to_le_bytes());
        map[OFF_SESSIONINFOOFFSET..OFF_SESSIONINFOOFFSET + 4]
            .copy_from_slice(&(si_off as i32).to_le_bytes());
        map[OFF_NUMVARS..OFF_NUMVARS + 4].copy_from_slice(&(num_vars as i32).to_le_bytes());
        map[OFF_VARHEADEROFFSET..OFF_VARHEADEROFFSET + 4]
            .copy_from_slice(&(vh_off as i32).to_le_bytes());
        map[OFF_NUMBUF..OFF_NUMBUF + 4].copy_from_slice(&1i32.to_le_bytes());
        map[OFF_BUFLEN..OFF_BUFLEN + 4].copy_from_slice(&(buf_len as i32).to_le_bytes());
        map[OFF_VARBUF..OFF_VARBUF + 4].copy_from_slice(&tick.to_le_bytes());
        map[OFF_VARBUF + 4..OFF_VARBUF + 8].copy_from_slice(&(buf_off as i32).to_le_bytes());
        map[vh_off..vh_off + varheaders.len()].copy_from_slice(varheaders);
        map[si_off..si_off + yaml.len()].copy_from_slice(yaml);
        map[buf_off..buf_off + buf_len].copy_from_slice(row);
        map
    }

    /// Round-trip a REAL iRacing .ibt: feed its samples through the writer and assert
    /// the output is byte-identical except the wall-clock date and the global tick
    /// (both environmental). Skips if the sample file is absent (e.g. in CI).
    #[test]
    fn real_sample_roundtrip() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../samples/sample_3.ibt");
        if !std::path::Path::new(path).exists() {
            eprintln!("skipping real_sample_roundtrip: sample not present at {path}");
            return;
        }
        let (ver, tick_rate, si_update, num_vars, buf_len, varheaders, yaml, rows, orig) =
            load_ibt(path);

        let dir = std::env::temp_dir().join(format!("ibt_real_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut w = IbtWriter::new(dir.clone());
            for (i, row) in rows.iter().enumerate() {
                let map = live_map_from(
                    ver,
                    tick_rate,
                    si_update,
                    num_vars,
                    buf_len,
                    &varheaders,
                    &yaml,
                    i as i32 + 1,
                    row,
                );
                if w.open.is_none() {
                    let layout = MapLayout::parse(&map).unwrap();
                    w.start_session(&map, layout, 1_700_000_000);
                }
                w.on_map_update(&map);
            }
            w.finish();
        }
        let out_path = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "ibt"))
            .unwrap();
        let out = std::fs::read(&out_path).unwrap();

        assert_eq!(out.len(), orig.len(), "file size differs");
        // Header [0..48] identical (ver/status/tickRate/offsets/counts).
        assert_eq!(out[0..48], orig[0..48], "header[0..48] differs");
        // [48..52] varBuf[0].tickCount: iRacing global tick vs synthetic — excluded.
        // [52..112] rest of header identical (bufOffset, pads, varBuf[1..4]).
        assert_eq!(out[52..112], orig[52..112], "header[52..112] differs");
        // [112..120] sessionStartDate: wall clock — excluded.
        // [120..144] subheader sans date: start/end time, lap, record — must match.
        assert_eq!(
            out[120..144],
            orig[120..144],
            "subheader times/laps/records differ"
        );
        // varHeaders + YAML + every sample row: byte-identical.
        assert_eq!(out[144..], orig[144..], "varHeaders/YAML/samples differ");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
