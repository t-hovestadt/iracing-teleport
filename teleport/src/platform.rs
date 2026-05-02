//! Platform-specific performance helpers.
//!
//! On Windows: sets timer resolution to 1 ms, raises thread priority, and can
//! pin the current thread to a specific CPU core.
//! On other platforms: no-ops (the binary only targets Windows anyway).

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Media::{timeBeginPeriod, timeEndPeriod};
    use windows_sys::Win32::System::Threading::{
        AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, GetCurrentProcess,
        GetCurrentThread, HIGH_PRIORITY_CLASS, SetPriorityClass, SetThreadAffinityMask,
        SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
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

    /// Register this thread with MMCSS under the "Games" task for reserved CPU time
    /// and dynamic priority boosts (~18 vs ABOVE_NORMAL ~10). Only use on the target
    /// PC — on the source PC iRacing holds its own MMCSS registrations and adding
    /// another competitor risks micro-stutters in the game.
    pub struct MmcssGuard(HANDLE);

    impl MmcssGuard {
        pub fn acquire() -> Option<Self> {
            let task: Vec<u16> = "Games\0".encode_utf16().collect();
            let mut task_index = 0u32;
            let handle =
                unsafe { AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut task_index) };
            if handle == 0 {
                eprintln!("MMCSS registration failed (continuing without it)");
                None
            } else {
                Some(Self(handle))
            }
        }
    }

    impl Drop for MmcssGuard {
        fn drop(&mut self) {
            unsafe { AvRevertMmThreadCharacteristics(self.0) };
        }
    }

    /// Raise the process to HIGH_PRIORITY_CLASS for lower OS scheduling jitter.
    /// Stacks with MMCSS and ABOVE_NORMAL thread priority. Safe to use on the
    /// SimHub PC. On the iRacing PC this competes with iRacing's own scheduling;
    /// only use it if the machine is dedicated to streaming with no game running.
    pub fn set_high_priority() {
        let ok = unsafe { SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS) };
        if ok == 0 {
            eprintln!("set_high_priority: SetPriorityClass failed");
        } else {
            println!("Process priority set to HIGH_PRIORITY_CLASS.");
        }
    }

    /// Pin the calling thread to a single CPU core. Reduces jitter from
    /// cross-core migration at the cost of giving up scheduling flexibility.
    /// `core` is a 0-based CPU index; cores past 63 are ignored.
    pub fn pin_thread_to_core(core: usize) {
        if core >= 64 {
            eprintln!("pin_thread_to_core: core {core} out of range (max 63), ignoring");
            return;
        }
        let mask: usize = 1usize << core;
        let prev = unsafe { SetThreadAffinityMask(GetCurrentThread(), mask) };
        if prev == 0 {
            eprintln!("pin_thread_to_core: SetThreadAffinityMask failed for core {core}");
        } else {
            println!("Pinned thread to CPU core {core}.");
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub struct HighResTimer;
    impl HighResTimer {
        pub fn acquire() -> Self { Self }
    }
    pub struct MmcssGuard;
    impl MmcssGuard {
        pub fn acquire() -> Option<Self> { Some(Self) }
    }
    pub fn boost_thread_priority() {}
    pub fn set_high_priority() {}
    pub fn pin_thread_to_core(_core: usize) {}
}

pub use imp::{HighResTimer, MmcssGuard, boost_thread_priority, pin_thread_to_core, set_high_priority};
