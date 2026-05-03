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

use std::io;
use std::sync::mpsc;

pub struct SourceConfig {
    pub bind: String,
    pub target: String,
    pub unicast: bool,
    pub busy_wait: bool,
    pub pin_core: Option<usize>,
    pub high_priority: bool,
    pub reconnect_timeout_secs: u64,
    pub datagram_size: usize,
    pub no_delta: bool,
    pub keyframe_interval: u16,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:0".into(),
            target: String::new(),
            unicast: false,
            busy_wait: false,
            pin_core: None,
            high_priority: false,
            reconnect_timeout_secs: source::DEFAULT_RECONNECT_TIMEOUT_SECS,
            datagram_size: source::DEFAULT_DATAGRAM_SIZE,
            no_delta: false,
            keyframe_interval: source::DEFAULT_KEYFRAME_INTERVAL,
        }
    }
}

pub struct TargetConfig {
    pub bind: String,
    pub unicast: bool,
    pub multicast_group: String,
    pub busy_wait: bool,
    pub pin_core: Option<usize>,
    pub fanalab: bool,
    pub stale_timeout_secs: u64,
    pub high_priority: bool,
}

impl Default for TargetConfig {
    fn default() -> Self {
        Self {
            bind: format!("0.0.0.0:{}", DEFAULT_PORT),
            unicast: false,
            multicast_group: DEFAULT_MULTICAST.into(),
            busy_wait: false,
            pin_core: None,
            fanalab: false,
            stale_timeout_secs: target::DEFAULT_STALE_TIMEOUT_SECS,
            high_priority: false,
        }
    }
}

pub fn run_source(config: SourceConfig, shutdown: mpsc::Receiver<()>) -> io::Result<()> {
    source::run(
        &config.bind,
        &config.target,
        config.unicast,
        config.busy_wait,
        config.pin_core,
        config.high_priority,
        config.reconnect_timeout_secs,
        config.datagram_size,
        config.no_delta,
        config.keyframe_interval,
        shutdown,
    )
}

pub fn run_target(config: TargetConfig, shutdown: mpsc::Receiver<()>) -> io::Result<()> {
    target::run(
        &config.bind,
        config.unicast,
        &config.multicast_group,
        config.busy_wait,
        config.pin_core,
        config.fanalab,
        config.stale_timeout_secs,
        config.high_priority,
        shutdown,
    )
}
