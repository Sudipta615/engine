//! Steinberg ASIO COM interface vtable and Windows registry driver discovery.

use super::types::*;
use std::ffi::{c_char, c_void};

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
    pub get_channel_info:
        unsafe extern "system" fn(this: *mut c_void, info: *mut ASIOChannelInfo) -> i32,
    pub create_buffers: unsafe extern "system" fn(
        this: *mut c_void,
        buffer_infos: *mut ASIOBufferInfo,
        num_channels: i32,
        buffer_size: i32,
        callbacks: *mut ASIOCallbacks,
    ) -> i32,
    pub dispose_buffers: unsafe extern "system" fn(this: *mut c_void) -> i32,
    pub control_panel: unsafe extern "system" fn(this: *mut c_void) -> i32,
    pub future:
        unsafe extern "system" fn(this: *mut c_void, selector: i32, opt: *mut c_void) -> i32,
    pub output_ready: unsafe extern "system" fn(this: *mut c_void) -> i32,
}

// ── IASIO COM driver wrapper (Windows only) ──────────────────────────────────

#[cfg(windows)]
use crate::output::cpal_output::OutputError;

/// IASIO interface IID.
/// `{8B0B3B4A-9E3D-4D81-9BBB-9A9D946E7ACA}` from the Steinberg ASIO SDK 2.3.
#[cfg(windows)]
const IID_IASIO: windows::core::GUID =
    windows::core::GUID::from_u128(0x8B0B3B4A_9E3D_4D81_9BBB_9A9D946E7ACA);

/// Live IASIO COM driver handle.
///
/// Holds the [`IUnknown`] (so COM does not unload the DLL while we hold
/// references) and the IASIO interface pointer dereferenced to the
/// [`IASIOVtbl`] for direct virtual-method calls.
#[cfg(windows)]
pub struct AsioDriver {
    /// The `IUnknown` returned by [`CoCreateInstance`]; kept alive so the COM
    /// DLL stays loaded and the vtable pointer remains valid.
    _unk: windows::core::IUnknown,
    /// IASIO interface pointer (`*mut *mut IASIOVtbl`).
    iface: *mut *mut IASIOVtbl,
}

#[cfg(windows)]
impl AsioDriver {
    /// Create an IASIO COM object from the driver's CLSID string.
    ///
    /// `"{00000000-0000-0000-0000-000000000000}"` format expected
    /// (exactly as read from the registry).
    pub fn create(clsid: &str) -> Result<Self, OutputError> {
        use windows::core::{Interface, PCWSTR};
        use windows::Win32::System::Com::{CLSIDFromString, CoCreateInstance, CLSCTX_ALL};

        let clsid_wide: Vec<u16> = clsid.encode_utf16().chain(std::iter::once(0)).collect();
        let clsid_guid = unsafe { CLSIDFromString(PCWSTR::from_raw(clsid_wide.as_ptr())) }
            .map_err(|e| OutputError::StreamOpen(format!("CLSIDFromString({clsid}): {e}")))?;

        let unk: windows::core::IUnknown =
            unsafe { CoCreateInstance(&clsid_guid, None, CLSCTX_ALL) }
                .map_err(|e| OutputError::StreamOpen(format!("CoCreateInstance({clsid}): {e}")))?;

        let mut iface_ptr: *mut c_void = std::ptr::null_mut();
        // windows-core 0.59 exposes raw COM resolution as the hidden
        // `Interface::query` method (returns HRESULT); `.ok()` converts it
        // into a `Result` for the `?` path.
        unsafe { unk.query(&IID_IASIO, &mut iface_ptr as *mut *mut c_void) }
            .ok()
            .map_err(|e| OutputError::StreamOpen(format!("QueryInterface(IASIO): {e}")))?;

        Ok(Self {
            _unk: unk,
            iface: iface_ptr as *mut *mut IASIOVtbl,
        })
    }

    /// Dereference the vtable.
    #[inline]
    fn vt(&self) -> &IASIOVtbl {
        unsafe { &**self.iface }
    }

    /// `IASIO::init(sysHandle)` — initialize the driver.
    /// Pass `std::ptr::null_mut()` for `sys_handle` (the driver uses the
    /// calling thread's message queue / window handle on Windows).
    pub fn init(&self, sys_handle: *mut c_void) -> Result<(), OutputError> {
        let r = unsafe { (self.vt().init)(self.iface as *mut c_void, sys_handle) };
        if r == ASIO_FALSE {
            return Err(OutputError::StreamOpen("ASIO init() returned false".into()));
        }
        Ok(())
    }

    /// `IASIO::getChannels` → `(num_inputs, num_outputs)`.
    pub fn get_channels(&self) -> Result<(i32, i32), OutputError> {
        let (mut inputs, mut outputs) = (0i32, 0i32);
        let err: ASIOError = unsafe {
            (self.vt().get_channels)(self.iface as *mut c_void, &mut inputs, &mut outputs)
        }
        .into();
        if !err.is_ok() {
            return Err(OutputError::StreamOpen(format!("get_channels: {err:?}")));
        }
        Ok((inputs, outputs))
    }

    /// `IASIO::getBufferSize` → `(min, max, preferred, granularity)`.
    pub fn get_buffer_size(&self) -> Result<(i32, i32, i32, i32), OutputError> {
        let (mut min, mut max, mut preferred, mut gran) = (0i32, 0i32, 0i32, 0i32);
        let err: ASIOError = unsafe {
            (self.vt().get_buffer_size)(
                self.iface as *mut c_void,
                &mut min,
                &mut max,
                &mut preferred,
                &mut gran,
            )
        }
        .into();
        if !err.is_ok() {
            return Err(OutputError::StreamOpen(format!("get_buffer_size: {err:?}")));
        }
        Ok((min, max, preferred, gran))
    }

    /// `IASIO::canSampleRate`.
    pub fn can_sample_rate(&self, rate: f64) -> Result<(), OutputError> {
        let err: ASIOError =
            unsafe { (self.vt().can_sample_rate)(self.iface as *mut c_void, rate) }.into();
        if !err.is_ok() {
            return Err(OutputError::StreamOpen(format!(
                "can_sample_rate({rate}): {err:?}"
            )));
        }
        Ok(())
    }

    /// `IASIO::setSampleRate`.
    pub fn set_sample_rate(&self, rate: f64) -> Result<(), OutputError> {
        let err: ASIOError =
            unsafe { (self.vt().set_sample_rate)(self.iface as *mut c_void, rate) }.into();
        if !err.is_ok() {
            return Err(OutputError::StreamOpen(format!(
                "set_sample_rate({rate}): {err:?}"
            )));
        }
        Ok(())
    }

    /// `IASIO::getSampleRate`.
    pub fn get_sample_rate(&self) -> Result<f64, OutputError> {
        let mut rate = 0.0f64;
        let err: ASIOError =
            unsafe { (self.vt().get_sample_rate)(self.iface as *mut c_void, &mut rate) }.into();
        if !err.is_ok() {
            return Err(OutputError::StreamOpen(format!("get_sample_rate: {err:?}")));
        }
        Ok(rate)
    }

    /// `IASIO::getLatencies` → `(input_latency_samples, output_latency_samples)`.
    pub fn get_latencies(&self) -> Result<(i32, i32), OutputError> {
        let (mut in_lat, mut out_lat) = (0i32, 0i32);
        let err: ASIOError = unsafe {
            (self.vt().get_latencies)(self.iface as *mut c_void, &mut in_lat, &mut out_lat)
        }
        .into();
        if !err.is_ok() {
            return Err(OutputError::StreamOpen(format!("get_latencies: {err:?}")));
        }
        Ok((in_lat, out_lat))
    }

    /// `IASIO::createBuffers`.
    pub fn create_buffers(
        &self,
        buffer_infos: &mut [ASIOBufferInfo],
        buffer_size: i32,
        callbacks: &mut ASIOCallbacks,
    ) -> Result<(), OutputError> {
        let err: ASIOError = unsafe {
            (self.vt().create_buffers)(
                self.iface as *mut c_void,
                buffer_infos.as_mut_ptr(),
                buffer_infos.len() as i32,
                buffer_size,
                callbacks as *mut ASIOCallbacks,
            )
        }
        .into();
        if !err.is_ok() {
            return Err(OutputError::StreamOpen(format!("create_buffers: {err:?}")));
        }
        Ok(())
    }

    /// `IASIO::disposeBuffers`.
    pub fn dispose_buffers(&self) {
        unsafe {
            (self.vt().dispose_buffers)(self.iface as *mut c_void);
        }
    }

    /// `IASIO::start`.
    pub fn start(&self) -> Result<(), OutputError> {
        let err: ASIOError = unsafe { (self.vt().start)(self.iface as *mut c_void) }.into();
        if !err.is_ok() {
            return Err(OutputError::StreamOpen(format!("start: {err:?}")));
        }
        Ok(())
    }

    /// `IASIO::stop`.
    pub fn stop(&self) -> Result<(), OutputError> {
        let err: ASIOError = unsafe { (self.vt().stop)(self.iface as *mut c_void) }.into();
        if !err.is_ok() {
            return Err(OutputError::StreamOpen(format!("stop: {err:?}")));
        }
        Ok(())
    }

    /// `IASIO::controlPanel` — open the manufacturer's settings dialog.
    /// Returns `Ok(())` when the driver presented the panel; the error is
    /// `Ok` here per the ASIO SDK 2.3 contract (the driver returned zero).
    pub fn control_panel(&self) -> Result<(), OutputError> {
        let err: ASIOError = unsafe { (self.vt().control_panel)(self.iface as *mut c_void) }.into();
        if !err.is_ok() {
            return Err(OutputError::StreamOpen(format!("control_panel: {err:?}")));
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for AsioDriver {
    fn drop(&mut self) {
        if !self.iface.is_null() {
            let vt = unsafe { &**self.iface };
            unsafe {
                (vt.Release)(self.iface as *mut c_void);
            }
        }
        // _unk drops next, calling IUnknown::Release
    }
}

// SAFETY: the raw IASIO interface pointer is safe to send across threads
// (it is a COM object pointer; the driver's internal state is guarded by
// the driver's own threading model — typically single-threaded STA through
// the message queue of the thread that called init()).
#[cfg(windows)]
unsafe impl Send for AsioDriver {}

/// Enumerate installed ASIO drivers registered under `HKLM\SOFTWARE\ASIO`.
#[cfg(windows)]
pub fn enumerate_asio_drivers() -> Vec<AsioDriverInfo> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE,
        KEY_READ, REG_SZ,
    };

    let mut drivers = Vec::new();
    let subkey: Vec<u16> = "SOFTWARE\\ASIO\0".encode_utf16().collect();
    let mut h_key: HKEY = HKEY::default();

    unsafe {
        if RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            None,
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
                Some(windows::core::PWSTR(key_name.as_mut_ptr())),
                &mut key_name_len,
                None,
                None,
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
                None,
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
