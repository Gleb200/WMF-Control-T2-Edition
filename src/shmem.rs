// WMF Control (Windows Mac Fan Control) - by melinoe-ux - MIT License
//! Shared memory for external widgets (e.g. Touch Bar, rainmeter, DFRDisplayKm).
//! Exposes current CPU temperature and fan RPMs via Windows Named Shared Memory: `Local\WMF_CpuTemp`.

use std::time::SystemTime;

#[repr(C)]
pub struct WmfSharedData {
    pub magic: u32,         // 0x574D4654 ("WMFT")
    pub version: u32,       // 1
    pub cpu_temp: f32,      // e.g. 54.5 °C
    pub fan1_rpm: f32,      // e.g. 2100.0
    pub fan2_rpm: f32,      // e.g. 2200.0
    pub timestamp_ms: u64,  // Unix timestamp ms
    pub text: [u8; 32],     // ASCII null-terminated string, e.g. "54.5 °C\0"
}

pub struct SharedMem {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(windows)]
    view: *mut u8,
}

unsafe impl Send for SharedMem {}
unsafe impl Sync for SharedMem {}

impl SharedMem {
    pub fn new() -> Self {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
            use windows_sys::Win32::System::Memory::{
                CreateFileMappingW, MapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
            };

            let name: Vec<u16> = "Local\\WMF_CpuTemp\0".encode_utf16().collect();
            let handle = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                128,
                name.as_ptr(),
            );
            if handle.is_null() {
                return SharedMem { handle: std::ptr::null_mut(), view: std::ptr::null_mut() };
            }
            let view_addr = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, 128);
            if view_addr.Value.is_null() {
                windows_sys::Win32::Foundation::CloseHandle(handle);
                return SharedMem { handle: std::ptr::null_mut(), view: std::ptr::null_mut() };
            }
            let view = view_addr.Value as *mut u8;
            SharedMem { handle, view }
        }
        #[cfg(not(windows))]
        SharedMem {}
    }

    pub fn write_cpu_temp(&self, cpu_temp: f64) {
        #[cfg(windows)]
        unsafe {
            if self.view.is_null() {
                return;
            }
            let bytes = (cpu_temp as f32).to_ne_bytes();
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.view, 4);
        }
    }

    pub fn write_status(&self, cpu_temp: f64, fan1: f64, fan2: f64) {
        #[cfg(windows)]
        unsafe {
            if self.view.is_null() {
                return;
            }
            let mut buf = [0u8; 128];
            // 0..4: CPU Temp as 4-byte float (f32) for external widgets
            buf[0..4].copy_from_slice(&(cpu_temp as f32).to_ne_bytes());
            // 4..8: Fan 1 RPM (f32)
            buf[4..8].copy_from_slice(&(fan1 as f32).to_ne_bytes());
            // 8..12: Fan 2 RPM (f32)
            buf[8..12].copy_from_slice(&(fan2 as f32).to_ne_bytes());
            // 12..20: Timestamp ms
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            buf[12..20].copy_from_slice(&now.to_ne_bytes());
            // 20..24: Magic "WMFT"
            buf[20..24].copy_from_slice(&0x574D4654u32.to_ne_bytes());
            // 24..28: Version
            buf[24..28].copy_from_slice(&1u32.to_ne_bytes());
            // 28..60: String format "54.5 °C\0"
            let str_val = format!("{cpu_temp:.1} °C\0");
            let str_bytes = str_val.as_bytes();
            let copy_len = str_bytes.len().min(31);
            buf[28..28 + copy_len].copy_from_slice(&str_bytes[..copy_len]);

            std::ptr::copy_nonoverlapping(buf.as_ptr(), self.view, 128);
        }
    }
}

impl Drop for SharedMem {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            if !self.view.is_null() {
                windows_sys::Win32::System::Memory::UnmapViewOfFile(
                    windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view as _ }
                );
            }
            if !self.handle.is_null() {
                windows_sys::Win32::Foundation::CloseHandle(self.handle);
            }
        }
    }
}
