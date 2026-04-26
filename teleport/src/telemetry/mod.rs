/// iRacing writes ~1.1 MB of telemetry, but allocate 2 MB for future headroom.
pub const MAX_TELEMETRY_SIZE: usize = 2 * 1024 * 1024;

#[derive(Debug)]
#[allow(dead_code)]
pub enum TelemetryError {
    /// iRacing is not running / shared memory not available.
    Unavailable,
    /// Any other OS or I/O error.
    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for TelemetryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TelemetryError::Unavailable => write!(f, "iRacing telemetry not available"),
            TelemetryError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TelemetryError {}

/// Platform-agnostic interface over iRacing's shared-memory telemetry.
pub trait TelemetryProvider: Sized {
    /// Open the existing shared-memory region created by iRacing (read side).
    fn open() -> Result<Self, TelemetryError>;

    /// Create a new shared-memory region and data-ready event (write side).
    fn create(size: usize) -> Result<Self, TelemetryError>;

    /// Block until iRacing signals new data or `timeout_ms` elapses.
    /// Returns `true` if data is ready, `false` on timeout.
    fn wait_for_data(&self, timeout_ms: u32) -> bool;

    /// Signal the data-ready event so consumers know to read.
    fn signal_data_ready(&self) -> Result<(), TelemetryError>;

    /// Read-only view of the mapped memory.
    fn as_slice(&self) -> &[u8];

    /// Mutable view of the mapped memory (write side only).
    fn as_slice_mut(&mut self) -> &mut [u8];

    /// Size of the mapped region in bytes.
    fn size(&self) -> usize;

    /// Returns `(byte_offset, byte_len)` of the active iRSdk variable buffer
    /// within the map, or `None` if the header can't be parsed (caller falls
    /// back to sending the full map).
    fn active_var_buf(&self) -> Option<(usize, usize)> {
        None
    }

    /// Zero the IRSDK_ST_CONNECTED status flag at offset 4 so that consumers
    /// (e.g. SimHub) observe a clean disconnect when we close the map.
    fn clear_status(&mut self) {}
}

// ── Platform dispatch ─────────────────────────────────────────────────────────

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::WindowsTelemetry as Telemetry;

#[cfg(not(windows))]
mod mock;
#[cfg(not(windows))]
pub use mock::MockTelemetry as Telemetry;
