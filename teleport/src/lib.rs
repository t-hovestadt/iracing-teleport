pub mod protocol;
pub mod source;
pub mod stats;
pub mod target;
pub mod telemetry;

use std::io;
use std::sync::mpsc;

/// Default multicast group used by source and target when no address is specified.
pub const DEFAULT_MULTICAST: &str = "239.255.0.1";
/// Default UDP port for both sending and receiving.
pub const DEFAULT_PORT: u16 = 5000;

// ── SourceConfig ──────────────────────────────────────────────────────────────

pub struct SourceConfig {
    /// Local UDP bind address (e.g. `"0.0.0.0:0"`).
    pub bind: String,
    /// Destination address — `group:port` in multicast mode, `ip:port` in unicast.
    pub target: String,
    /// Send directly to one host instead of multicast.
    pub unicast: bool,
    /// Spin on the iRacing data-ready event instead of sleeping (lower jitter, costs one CPU core).
    pub busy_wait: bool,
    /// Pin this thread to a specific CPU core (0-based).
    pub pin_core: Option<usize>,
    /// Set `HIGH_PRIORITY_CLASS` on this process.
    pub high_priority: bool,
    /// Spawn a dummy `iRacingSim64DX11.exe` stub so FanaLab / fanatec-tuner detect iRacing.
    pub fanalab: bool,
    /// Zero the telemetry region on shutdown. (No-op on source — the map lives on the target.)
    pub zero_on_exit: bool,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:0".into(),
            target: format!("{DEFAULT_MULTICAST}:{DEFAULT_PORT}"),
            unicast: false,
            busy_wait: false,
            pin_core: None,
            high_priority: false,
            fanalab: false,
            zero_on_exit: false,
        }
    }
}

// ── TargetConfig ──────────────────────────────────────────────────────────────

pub struct TargetConfig {
    /// UDP bind address (e.g. `"0.0.0.0:5000"`).
    pub bind: String,
    /// Expect a direct unicast stream instead of multicast.
    pub unicast: bool,
    /// Multicast group to join (ignored in unicast mode).
    pub multicast_group: String,
    /// Spin on recv instead of sleeping (lower jitter, costs one CPU core).
    pub busy_wait: bool,
    /// Pin this thread to a specific CPU core (0-based).
    pub pin_core: Option<usize>,
    /// Set `HIGH_PRIORITY_CLASS` on this process.
    pub high_priority: bool,
    /// Spawn a dummy `iRacingSim64DX11.exe` stub so FanaLab / fanatec-tuner detect iRacing.
    pub fanalab: bool,
    /// Zero the shared-memory map before dropping it on shutdown.
    pub zero_on_exit: bool,
    /// Called once when the shared-memory map is first created (iRacing data
    /// arrives) and again each time data resumes after a stale timeout.
    /// Pass `None` if not needed.
    pub on_first_data: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    /// Called when no data arrives for the stale timeout and the map is zeroed.
    /// Pass `None` if not needed.
    pub on_stale: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

impl Default for TargetConfig {
    fn default() -> Self {
        Self {
            bind: format!("0.0.0.0:{DEFAULT_PORT}"),
            unicast: false,
            multicast_group: DEFAULT_MULTICAST.into(),
            busy_wait: false,
            pin_core: None,
            high_priority: false,
            fanalab: false,
            zero_on_exit: false,
            on_first_data: None,
            on_stale: None,
        }
    }
}

// ── Public run API ────────────────────────────────────────────────────────────

pub fn run_source(config: SourceConfig, shutdown: mpsc::Receiver<()>) -> io::Result<()> {
    if config.high_priority {
        set_high_priority();
    }
    if let Some(core) = config.pin_core {
        pin_thread(core);
    }
    let _fanalab_guard = FanabGuard(if config.fanalab {
        spawn_fanalab()
    } else {
        None
    });
    source::run(
        &config.bind,
        &config.target,
        config.unicast,
        config.busy_wait,
        shutdown,
    )
}

pub fn run_target(config: TargetConfig, shutdown: mpsc::Receiver<()>) -> io::Result<()> {
    if config.high_priority {
        set_high_priority();
    }
    if let Some(core) = config.pin_core {
        pin_thread(core);
    }
    let _fanalab_guard = FanabGuard(if config.fanalab {
        spawn_fanalab()
    } else {
        None
    });
    target::run(
        &config.bind,
        config.unicast,
        &config.multicast_group,
        config.busy_wait,
        config.zero_on_exit,
        config.on_first_data,
        config.on_stale,
        shutdown,
    )
}

// ── FanaLab stub ──────────────────────────────────────────────────────────────

struct FanabGuard(Option<std::process::Child>);

impl Drop for FanabGuard {
    fn drop(&mut self) {
        if let Some(ref mut c) = self.0 {
            let _ = c.kill();
            let _ = c.wait();
            eprintln!("[fanalab] stub process terminated");
        }
    }
}

/// Copy this executable to `%TEMP%\iRacingSim64DX11.exe` and spawn it with the
/// hidden `--fanalab-stub` flag, which causes it to sleep forever.  FanaLab and
/// fanatec-tuner scan for a running process with that name to detect iRacing.
#[cfg(windows)]
fn spawn_fanalab() -> Option<std::process::Child> {
    let src = std::env::current_exe().ok()?;
    let tmp = std::env::temp_dir().join("iRacingSim64DX11.exe");
    if let Err(e) = std::fs::copy(&src, &tmp) {
        eprintln!("[fanalab] failed to copy stub: {e}");
        return None;
    }
    match std::process::Command::new(&tmp)
        .arg("--fanalab-stub")
        .spawn()
    {
        Ok(child) => {
            eprintln!(
                "[fanalab] spawned iRacingSim64DX11.exe (pid {})",
                child.id()
            );
            Some(child)
        }
        Err(e) => {
            eprintln!("[fanalab] failed to spawn stub: {e}");
            None
        }
    }
}

#[cfg(not(windows))]
fn spawn_fanalab() -> Option<std::process::Child> {
    None
}

// ── Platform helpers ──────────────────────────────────────────────────────────

/// Exclude CPU 0 from this process's affinity mask.
///
/// iRacing's sim thread is hardcoded to CPU 0. Running on CPU 0
/// causes Type B frame time spikes. This moves our process off
/// CPU 0 entirely so we never compete with the sim thread.
///
/// Reference: https://rcsracing93.github.io/iracing-stutter-fix/
#[cfg(windows)]
pub fn avoid_cpu0() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetProcessAffinityMask, SetProcessAffinityMask,
    };
    unsafe {
        let process = GetCurrentProcess();
        let mut process_mask: usize = 0;
        let mut system_mask: usize = 0;
        if GetProcessAffinityMask(process, &mut process_mask, &mut system_mask) != 0 {
            let new_mask = process_mask & !1usize;
            if new_mask != 0 && SetProcessAffinityMask(process, new_mask) != 0 {
                eprintln!("[cpu] excluded CPU 0 (iRacing sim thread protection)");
            }
        }
    }
}

#[cfg(not(windows))]
pub fn avoid_cpu0() {}

/// Set `HIGH_PRIORITY_CLASS` on the current process.
#[cfg(windows)]
fn set_high_priority() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, SetPriorityClass, HIGH_PRIORITY_CLASS,
    };
    unsafe {
        if SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS) != 0 {
            eprintln!("[priority] HIGH_PRIORITY_CLASS set");
        }
    }
}

#[cfg(not(windows))]
fn set_high_priority() {}

/// Pin the current thread to a single CPU core.
#[cfg(windows)]
fn pin_thread(core: usize) {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};
    unsafe {
        if SetThreadAffinityMask(GetCurrentThread(), 1usize << core) != 0 {
            eprintln!("[cpu] pinned to core {core}");
        }
    }
}

#[cfg(not(windows))]
fn pin_thread(_core: usize) {}
