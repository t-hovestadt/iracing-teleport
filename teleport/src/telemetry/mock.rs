/// Non-Windows stub used for compilation and unit testing on macOS/Linux.
use super::{TelemetryError, TelemetryProvider};

pub struct MockTelemetry {
    data: Vec<u8>,
}

impl TelemetryProvider for MockTelemetry {
    fn open() -> Result<Self, TelemetryError> {
        Err(TelemetryError::Unavailable)
    }

    fn create(size: usize) -> Result<Self, TelemetryError> {
        Ok(Self {
            data: vec![0u8; size],
        })
    }

    fn wait_for_data(&self, _timeout_ms: u32) -> bool {
        false
    }

    fn signal_data_ready(&self) -> Result<(), TelemetryError> {
        Ok(())
    }

    fn as_slice(&self) -> &[u8] {
        &self.data
    }

    fn as_slice_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    fn size(&self) -> usize {
        self.data.len()
    }
}
