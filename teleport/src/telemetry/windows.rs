use super::{TelemetryError, TelemetryProvider};
use std::ffi::c_void;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
    Security::{
        InitializeSecurityDescriptor, SetSecurityDescriptorDacl,
        SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
    },
    System::Memory::{
        CreateFileMappingW, MapViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS, OpenFileMappingW,
        UnmapViewOfFile, FILE_MAP_ALL_ACCESS, FILE_MAP_READ, PAGE_READWRITE,
    },
    System::Threading::{CreateEventW, OpenEventW, SetEvent, WaitForSingleObject},
};

const MEM_NAME: &str = "Local\\IRSDKMemMapFileName\0";
const EVENT_NAME: &str = "Local\\IRSDKDataValidEvent\0";

// Minimum access right needed for WaitForSingleObject.
const SYNCHRONIZE: u32 = 0x00100000;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

pub struct WindowsTelemetry {
    h_map: HANDLE,
    h_event: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    size: usize,
}

unsafe impl Send for WindowsTelemetry {}
unsafe impl Sync for WindowsTelemetry {}

impl Drop for WindowsTelemetry {
    fn drop(&mut self) {
        unsafe {
            if !self.view.Value.is_null() {
                UnmapViewOfFile(self.view);
                self.view.Value = std::ptr::null_mut();
            }
            if self.h_map != 0 && self.h_map != INVALID_HANDLE_VALUE {
                CloseHandle(self.h_map);
                self.h_map = 0;
            }
            if self.h_event != 0 && self.h_event != INVALID_HANDLE_VALUE {
                CloseHandle(self.h_event);
                self.h_event = 0;
            }
        }
    }
}

impl TelemetryProvider for WindowsTelemetry {
    fn open() -> Result<Self, TelemetryError> {
        unsafe {
            let h_map = OpenFileMappingW(FILE_MAP_READ, 0, wide(MEM_NAME).as_ptr());
            if h_map == 0 || h_map == INVALID_HANDLE_VALUE {
                return Err(TelemetryError::Unavailable);
            }

            let view = MapViewOfFile(h_map, FILE_MAP_READ, 0, 0, 0);
            if view.Value.is_null() {
                CloseHandle(h_map);
                return Err(TelemetryError::Unavailable);
            }

            let h_event = OpenEventW(SYNCHRONIZE, 0, wide(EVENT_NAME).as_ptr());
            if h_event == 0 || h_event == INVALID_HANDLE_VALUE {
                UnmapViewOfFile(view);
                CloseHandle(h_map);
                return Err(TelemetryError::Unavailable);
            }

            let size = query_region_size(view.Value as *const u8)
                .unwrap_or(super::MAX_TELEMETRY_SIZE);

            Ok(Self { h_map, h_event, view, size })
        }
    }

    fn create(size: usize) -> Result<Self, TelemetryError> {
        unsafe {
            // Use a NULL DACL so any process can open the shared memory and
            // event regardless of elevation or user context — matches iRacing's
            // own shared memory setup.
            let mut sd = std::mem::zeroed::<SECURITY_DESCRIPTOR>();
            InitializeSecurityDescriptor(
                &mut sd as *mut _ as *mut c_void,
                1, // SECURITY_DESCRIPTOR_REVISION
            );
            SetSecurityDescriptorDacl(
                &mut sd as *mut _ as *mut c_void,
                1,                        // bDaclPresent = TRUE
                std::ptr::null_mut(),     // pDacl = NULL → grant all access
                0,                        // bDaclDefaulted = FALSE
            );
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: &mut sd as *mut _ as *mut c_void,
                bInheritHandle: 0,
            };

            let h_map = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                &sa,
                PAGE_READWRITE,
                (size >> 32) as u32,
                (size & 0xFFFF_FFFF) as u32,
                wide(MEM_NAME).as_ptr(),
            );
            if h_map == 0 {
                return Err(TelemetryError::Other("CreateFileMappingW failed".into()));
            }

            let view = MapViewOfFile(h_map, FILE_MAP_ALL_ACCESS, 0, 0, 0);
            if view.Value.is_null() {
                CloseHandle(h_map);
                return Err(TelemetryError::Other("MapViewOfFile failed".into()));
            }

            let h_event = CreateEventW(&sa, 0, 0, wide(EVENT_NAME).as_ptr());
            if h_event == 0 || h_event == INVALID_HANDLE_VALUE {
                UnmapViewOfFile(view);
                CloseHandle(h_map);
                return Err(TelemetryError::Other("CreateEventW failed".into()));
            }

            Ok(Self { h_map, h_event, view, size })
        }
    }

    fn wait_for_data(&self, timeout_ms: u32) -> bool {
        unsafe { WaitForSingleObject(self.h_event, timeout_ms) == WAIT_OBJECT_0 }
    }

    fn signal_data_ready(&self) -> Result<(), TelemetryError> {
        unsafe {
            if SetEvent(self.h_event) == 0 {
                Err(TelemetryError::Other("SetEvent failed".into()))
            } else {
                Ok(())
            }
        }
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.view.Value as *const u8, self.size) }
    }

    fn as_slice_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.view.Value as *mut u8, self.size) }
    }

    fn size(&self) -> usize {
        self.size
    }

    fn active_var_buf(&self) -> Option<(usize, usize)> {
        let d = self.as_slice();
        if d.len() < super::IRSDK_HEADER_SIZE {
            return None;
        }
        // irsdk_header offsets (all i32 little-endian):
        //   32  numBuf   — number of active ring slots (≤ 4)
        //   36  bufLen   — byte length of one variable buffer
        //   48  varBuf[0..numBuf], each 16 bytes:
        //         +0  tickCount  — highest = most-recently written slot
        //         +4  bufOffset  — byte offset within the map
        let buf_len = i32::from_le_bytes(d[36..40].try_into().ok()?) as usize;
        if buf_len == 0 || buf_len > d.len() {
            return None;
        }
        let num_buf = (i32::from_le_bytes(d[32..36].try_into().ok()?) as usize).min(4);
        if num_buf == 0 {
            return None;
        }
        let mut best_tick = i32::MIN;
        let mut best_off: usize = 0;
        for i in 0..num_buf {
            let b = 48 + i * 16;
            let tick = i32::from_le_bytes(d[b..b + 4].try_into().ok()?);
            let off = i32::from_le_bytes(d[b + 4..b + 8].try_into().ok()?) as usize;
            if tick > best_tick {
                best_tick = tick;
                best_off = off;
            }
        }
        if best_off + buf_len > d.len() {
            return None;
        }
        Some((best_off, buf_len))
    }

    fn clear_status(&mut self) {
        // Zero the irsdk_header.status field at byte offset 4 to clear the
        // IRSDK_ST_CONNECTED flag. SimHub polls this to detect disconnects.
        let d = self.as_slice_mut();
        if d.len() >= 8 {
            d[4..8].copy_from_slice(&0u32.to_le_bytes());
        }
    }
}

fn query_region_size(ptr: *const u8) -> Option<usize> {
    use windows_sys::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION};
    unsafe {
        let mut mbi = std::mem::zeroed::<MEMORY_BASIC_INFORMATION>();
        let ret = VirtualQuery(
            ptr as *const c_void,
            &mut mbi,
            std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        );
        if ret == 0 { None } else { Some(mbi.RegionSize) }
    }
}
