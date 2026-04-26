//! Platform-specific performance helpers.
//!
//! On Windows: sets timer resolution to 1 ms and raises thread priority.
//! On other platforms: no-ops (the binary only targets Windows anyway).

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Media::{timeBeginPeriod, timeEndPeriod};
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    };

    /// RAII guard that requests 1 ms Windows timer resolution for the lifetime
    /// of the process. The default 15.6 ms resolution caps how quickly the OS
    /// wakes a sleeping thread, adding jitter to every recv_from/WaitForSingleObject
    /// call.  iRacing sets this on the source machine, but the target machine
    /// (running SimHub, not iRacing) may not have it set.
    pub struct HighResTimer;

    impl HighResTimer {
        pub fn acquire() -> Self {
            unsafe { timeBeginPeriod(1) };
            Self
        }
    }

    impl Drop for HighResTimer {
        fn drop(&mut self) {
            unsafe { timeEndPeriod(1) };
        }
    }

    /// Raise the calling thread to ABOVE_NORMAL priority so the OS scheduler
    /// preempts it less often during the hot send/receive loop.
    pub fn boost_thread_priority() {
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
    }
}

#[cfg(not(windows))]
mod imp {
    pub struct HighResTimer;
    impl HighResTimer {
        pub fn acquire() -> Self { Self }
    }
    pub fn boost_thread_priority() {}
}

pub use imp::{HighResTimer, boost_thread_priority};
