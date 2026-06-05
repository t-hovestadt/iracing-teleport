pub mod ibt_writer;
pub mod platform;
pub mod protocol;
pub mod source;
pub mod stats;
pub mod target;
pub mod telemetry;

/// Callback type for per-window transfer stats.
/// Arguments: `(total_messages, total_bytes, avg_latency_us)` since the run started.
pub type StatsCb = Arc<dyn Fn(u64, u64, u64) + Send + Sync>;

/// Default multicast group used by source and target when no address is specified.
pub const DEFAULT_MULTICAST: &str = "239.255.0.1";
/// Default UDP port for both sending and receiving.
pub const DEFAULT_PORT: u16 = 5000;

use std::io;
use std::sync::{mpsc, Arc};

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
    /// Called every ~5 s when the stats window fires.
    /// Arguments: `(total_messages, total_bytes, avg_latency_us)` since `run_source()` was called.
    /// Pass `None` if not needed.
    pub on_stats: Option<StatsCb>,
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
            on_stats: None,
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
    /// Write iRacing `.ibt` telemetry files to the iRacing telemetry folder so
    /// disk-based tools (e.g. Garage 61) can read teleported telemetry locally.
    pub write_ibt: bool,
    /// Called once when the first complete frame is received. None = no-op.
    pub on_first_data: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Called when the stale timeout fires and the telemetry map is dropped. None = no-op.
    pub on_stale: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Called every ~5 s when the stats window fires.
    /// Arguments: `(total_messages, total_bytes, avg_latency_us)` since `run_target()` was called.
    /// Pass `None` if not needed.
    pub on_stats: Option<StatsCb>,
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
            write_ibt: false,
            on_first_data: None,
            on_stale: None,
            on_stats: None,
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
        config.on_stats,
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
        config.write_ibt,
        shutdown,
        config.on_first_data,
        config.on_stale,
        config.on_stats,
    )
}
