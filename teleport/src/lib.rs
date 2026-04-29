pub mod platform;
pub mod protocol;
pub mod source;
pub mod stats;
pub mod target;
pub mod telemetry;

/// Default multicast group used by source and target when no address is specified.
pub const DEFAULT_MULTICAST: &str = "239.255.0.1";
/// Default UDP port for both sending and receiving.
pub const DEFAULT_PORT: u16 = 5000;
