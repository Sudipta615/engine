//! Steinberg ASIO COM interface vtable and Windows registry driver discovery.

use std::ffi::{c_char, c_void};
use super::types::*;

/// Installed ASIO driver metadata read from Windows registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsioDriverInfo {
    pub name: String,
    pub description: String,
    pub clsid: String,
}

/// The Steinberg `IASIO` COM interface vtable (24 virtual methods).
#[repr(C)]
#[allow(non_snake_case)]
pub struct IASIOVtbl {
    // IUnknown methods
    pub QueryInterface: unsafe extern "system" fn(
        this: *mut c_void,
        riid: *const std::ffi::c_void,
        ppvObject: *mut *mut std::ffi::c_void,
    ) -> i32,
    pub AddRef: unsafe extern "system" fn(this: *mut c_void) -> u32,
    pub Release: unsafe extern "system" fn(this: *mut c_void) -> u32,

    // IASIO specific methods
    pub init: unsafe extern "system" fn(this: *mut c_void, sys_handle: *mut c_void) -> ASIOBool,
    pub get_driver_name: unsafe extern "system" fn(this: *mut c_void, name: *mut c_char),
    pub get_driver_version: unsafe extern "system" fn(this: *mut c_void) -> i32,
    pub get_error_message: unsafe extern "system" fn(this: *mut c_void, string: *mut c_char),
    pub start: unsafe extern "system" fn(this: *mut c_void) -> i32,
    pub stop: unsafe extern "system" fn(this: *mut c_void) -> i32,
    pub get_channels: unsafe extern "system" fn(
        this: *mut c_void,
        num_input_channels: *mut i32,
        num_output_channels: *mut i32,
    ) -> i32,
    pub get_latencies: unsafe extern "system" fn(
        this: *mut c_void,
        input_latency: *mut i32,
        output_latency: *mut i32,
    ) -> i32,
    pub get_buffer_size: unsafe extern "system" fn(
        this: *mut c_void,
        min_size: *mut i32,
        max_size: *mut i32,
        preferred_size: *mut i32,
        granularity: *mut i32,
    ) -> i32,
    pub can_sample_rate: unsafe extern "system" fn(this: *mut c_void, sample_rate: f64) -> i32,
    pub get_sample_rate: unsafe extern "system" fn(this: *mut c_void, sample_rate: *mut f64) -> i32,
    pub set_sample_rate: unsafe extern "system" fn(this: *mut c_void, sample_rate: f64) -> i32,
    pub get_clock_sources: unsafe extern "system" fn(
        this: *mut c_void,
        clocks: *mut ASIOClockSource,
        num_sources: *mut i32,
    ) -> i32,
    pub set_clock_source: unsafe extern "system" fn(this: *mut c_void, reference: i32) -> i32,
    pub get_sample_position: unsafe extern "system" fn(
        this: *mut c_void,
        s_pos: *mut ASIOSamples,
        t_stamp: *mut ASIOTimeStamp,
    ) -> i32,
    pub get_channel_info: unsafe extern "system" fn(this: *mut c_void, info: *mut ASIOChannelInfo) -> i32,
    pub create_buffers: unsafe extern "system" fn(
        this: *mut c_void,
        buffer_infos: *mut ASIOBufferInfo,
        num_channels: i32,
        buffer_size: i32,
        callbacks: *mut ASIOCallbacks,
    ) -> i32,
    pub dispose_buffers: unsafe extern "system" fn(this: *mut c_void) -> i32,
    pub control_panel: unsafe extern "system" fn(this: *mut c_void) -> i32,
    pub future: unsafe extern "system" fn(this: *mut c_void, selector: i32, opt: *mut c_void) -> i32,
    pub output_ready: unsafe extern "system" fn(this: *mut c_void) -> i32,
}

/// Enumerate installed ASIO drivers registered under `HKLM\SOFTWARE\ASIO`.
#[cfg(windows)]
pub fn enumerate_asio_drivers() -> Vec<AsioDriverInfo> {
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE,
        KEY_READ, REG_SZ,
    };
    use windows::core::PCWSTR;

    let mut drivers = Vec::new();
    let subkey: Vec<u16> = "SOFTWARE\\ASIO\0".encode_utf16().collect();
    let mut h_key: HKEY = HKEY::default();

    unsafe {
        if RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            0,
            KEY_READ,
            &mut h_key,
        )
        .is_err()
        {
            return drivers;
        }

        let mut index = 0u32;
        let mut key_name = [0u16; 256];
        loop {
            let mut key_name_len = key_name.len() as u32;
            if RegEnumKeyExW(
                h_key,
                index,
                windows::core::PWSTR(key_name.as_mut_ptr()),
                &mut key_name_len,
                None,
                windows::core::PWSTR::null(),
                None,
                None,
            )
            .is_err()
            {
                break;
            }

            let driver_subpath = String::from_utf16_lossy(&key_name[..key_name_len as usize]);
            let driver_key_str: Vec<u16> = format!("SOFTWARE\\ASIO\\{}\0", driver_subpath)
                .encode_utf16()
                .collect();
            let mut h_driver_key: HKEY = HKEY::default();

            if RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(driver_key_str.as_ptr()),
                0,
                KEY_READ,
                &mut h_driver_key,
            )
            .is_ok()
            {
                let mut clsid_buf = [0u16; 128];
                let mut clsid_len = (clsid_buf.len() * 2) as u32;
                let clsid_val_name: Vec<u16> = "CLSID\0".encode_utf16().collect();
                let mut val_type = REG_SZ;

                let clsid = if RegQueryValueExW(
                    h_driver_key,
                    PCWSTR(clsid_val_name.as_ptr()),
                    None,
                    Some(&mut val_type),
                    Some(clsid_buf.as_mut_ptr() as *mut u8),
                    Some(&mut clsid_len),
                )
                .is_ok()
                {
                    let chars_len = (clsid_len as usize / 2).saturating_sub(1);
                    String::from_utf16_lossy(&clsid_buf[..chars_len])
                } else {
                    String::new()
                };

                let mut desc_buf = [0u16; 256];
                let mut desc_len = (desc_buf.len() * 2) as u32;
                let desc_val_name: Vec<u16> = "Description\0".encode_utf16().collect();

                let desc = if RegQueryValueExW(
                    h_driver_key,
                    PCWSTR(desc_val_name.as_ptr()),
                    None,
                    Some(&mut val_type),
                    Some(desc_buf.as_mut_ptr() as *mut u8),
                    Some(&mut desc_len),
                )
                .is_ok()
                {
                    let chars_len = (desc_len as usize / 2).saturating_sub(1);
                    String::from_utf16_lossy(&desc_buf[..chars_len])
                } else {
                    driver_subpath.clone()
                };

                let _ = RegCloseKey(h_driver_key);

                if !clsid.is_empty() {
                    drivers.push(AsioDriverInfo {
                        name: driver_subpath,
                        description: desc,
                        clsid,
                    });
                }
            }

            index += 1;
        }

        let _ = RegCloseKey(h_key);
    }

    drivers
}

#[cfg(not(windows))]
pub fn enumerate_asio_drivers() -> Vec<AsioDriverInfo> {
    Vec::new()
}
