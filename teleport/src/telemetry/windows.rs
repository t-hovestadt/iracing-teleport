use super::{TelemetryError, TelemetryProvider};
use std::ffi::c_void;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
    Security::SECURITY_ATTRIBUTES,
    System::Memory::{
        CreateFileMappingW, MapViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS, OpenFileMappingW,
        UnmapViewOfFile, FILE_MAP_ALL_ACCESS, FILE_MAP_READ, PAGE_READWRITE,
    },
    System::Threading::{CreateEventW, OpenEventW, SetEvent, WaitForSingleObject},
};

const MEM_NAME: &str = "Local\\IRSDKMemMapFileName\0";
const EVENT_NAME: &str = "Local\\IRSDKDataValidEvent\0";

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

            // 0x00100000 = SYNCHRONIZE — minimum access needed for WaitForSingleObject.
            let h_event = OpenEventW(0x00100000u32, 0, wide(EVENT_NAME).as_ptr());
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
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: std::ptr::null_mut(),
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
