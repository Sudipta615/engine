//! Steinberg ASIO C / COM ABI types, sample formats, and structs.
//!
//! Defined in pure Rust according to the Steinberg Audio Stream Input/Output
//! Specification (ASIO SDK 2.3), without requiring any external C++ SDK headers.

use std::ffi::{c_char, c_void};

/// ASIO error return codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ASIOError {
    /// Operation succeeded.
    Ok = 0,
    /// Operation succeeded and has special meaning (e.g. host requested buffer size accepted).
    Success = 0x3f482c10,
    /// Hardware input or output is not present or failed to initialize.
    NotPresent = -1000,
    /// Hardware malfunction.
    HWMalfunction = -999,
    /// Invalid parameter passed to driver method.
    InvalidParameter = -998,
    /// Invalid mode or driver state.
    InvalidMode = -997,
    /// Sample position pointer not advancing.
    SPNotAdvancing = -996,
    /// No hardware clock available.
    NoClock = -995,
    /// Memory allocation failed in driver.
    NoMemory = -994,
    /// Unknown error code.
    Unknown(i32),
}

impl From<i32> for ASIOError {
    fn from(val: i32) -> Self {
        match val {
            0 => Self::Ok,
            0x3f482c10 => Self::Success,
            -1000 => Self::NotPresent,
            -999 => Self::HWMalfunction,
            -998 => Self::InvalidParameter,
            -997 => Self::InvalidMode,
            -996 => Self::SPNotAdvancing,
            -995 => Self::NoClock,
            -994 => Self::NoMemory,
            other => Self::Unknown(other),
        }
    }
}

impl ASIOError {
    #[inline]
    pub fn is_ok(self) -> bool {
        matches!(self, Self::Ok | Self::Success)
    }
}

/// ASIO boolean type.
pub type ASIOBool = i32;
pub const ASIO_FALSE: ASIOBool = 0;
pub const ASIO_TRUE: ASIOBool = 1;

/// ASIO sample formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ASIOSampleType {
    Int16MSB = 0,
    Int24MSB = 1,
    Int32MSB = 2,
    Float32MSB = 3,
    Float64MSB = 4,

    // Little-endian formats (standard on Windows x86/x64)
    Int16LSB = 16,
    Int24LSB = 17,
    Int32LSB = 18,
    Float32LSB = 19,
    Float64LSB = 20,

    // 24-bit alignment variants
    Int32LSB16 = 24, // 32-bit container with 16-bit alignment
    Int32LSB18 = 25, // 32-bit container with 18-bit alignment
    Int32LSB20 = 26, // 32-bit container with 20-bit alignment
    Int32LSB24 = 27, // 32-bit container with 24-bit alignment (most common pro-audio 24-bit PCM)

    // DSD 1-bit formats (native 1-bit bitstream)
    DSDInt8LSB1 = 32, // DSD 1-bit data, 8 samples per byte, LSB first
    DSDInt8MSB1 = 33, // DSD 1-bit data, 8 samples per byte, MSB first
    DSDInt8NER8 = 34, // DSD 1-bit data, non-reversed 8-bit

    Unknown(i32),
}

impl From<i32> for ASIOSampleType {
    fn from(val: i32) -> Self {
        match val {
            0 => Self::Int16MSB,
            1 => Self::Int24MSB,
            2 => Self::Int32MSB,
            3 => Self::Float32MSB,
            4 => Self::Float64MSB,
            16 => Self::Int16LSB,
            17 => Self::Int24LSB,
            18 => Self::Int32LSB,
            19 => Self::Float32LSB,
            20 => Self::Float64LSB,
            24 => Self::Int32LSB16,
            25 => Self::Int32LSB18,
            26 => Self::Int32LSB20,
            27 => Self::Int32LSB24,
            32 => Self::DSDInt8LSB1,
            33 => Self::DSDInt8MSB1,
            34 => Self::DSDInt8NER8,
            other => Self::Unknown(other),
        }
    }
}

impl ASIOSampleType {
    pub fn as_i32(self) -> i32 {
        match self {
            Self::Int16MSB => 0,
            Self::Int24MSB => 1,
            Self::Int32MSB => 2,
            Self::Float32MSB => 3,
            Self::Float64MSB => 4,
            Self::Int16LSB => 16,
            Self::Int24LSB => 17,
            Self::Int32LSB => 18,
            Self::Float32LSB => 19,
            Self::Float64LSB => 20,
            Self::Int32LSB16 => 24,
            Self::Int32LSB18 => 25,
            Self::Int32LSB20 => 26,
            Self::Int32LSB24 => 27,
            Self::DSDInt8LSB1 => 32,
            Self::DSDInt8MSB1 => 33,
            Self::DSDInt8NER8 => 34,
            Self::Unknown(v) => v,
        }
    }
}

/// Information about a single ASIO channel buffer allocation.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ASIOBufferInfo {
    /// 1 = input channel, 0 = output channel.
    pub is_input: ASIOBool,
    /// 0-based channel index.
    pub channel_num: i32,
    /// Ping-pong double buffers provided by the driver.
    pub buffers: [*mut c_void; 2],
}

impl Default for ASIOBufferInfo {
    fn default() -> Self {
        Self {
            is_input: ASIO_FALSE,
            channel_num: 0,
            buffers: [std::ptr::null_mut(), std::ptr::null_mut()],
        }
    }
}

/// Metadata describing one physical or virtual ASIO channel.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ASIOChannelInfo {
    /// Channel index (0-based).
    pub channel: i32,
    /// 1 = input, 0 = output.
    pub is_input: ASIOBool,
    /// 1 = currently allocated/active.
    pub is_active: ASIOBool,
    /// Channel group (e.g. 0 for main stereo).
    pub channel_group: i32,
    /// Sample format of this channel.
    pub sample_type: i32,
    /// Channel name reported by driver (null-terminated string, 32 bytes max).
    pub name: [c_char; 32],
}

impl Default for ASIOChannelInfo {
    fn default() -> Self {
        Self {
            channel: 0,
            is_input: ASIO_FALSE,
            is_active: ASIO_FALSE,
            channel_group: 0,
            sample_type: ASIOSampleType::Float32LSB.as_i32(),
            name: [0; 32],
        }
    }
}

impl ASIOChannelInfo {
    pub fn name_string(&self) -> String {
        let bytes = self.name.iter().map(|&b| b as u8).take_while(|&b| b != 0).collect::<Vec<_>>();
        String::from_utf8_lossy(&bytes).to_string()
    }
}

/// Clock source metadata.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ASIOClockSource {
    pub index: i32,
    pub associated_channel: i32,
    pub associated_group: i32,
    pub is_current_source: ASIOBool,
    pub name: [c_char; 32],
}

/// 64-bit integer timestamp for ASIO timekeeping.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ASIOTimeStamp {
    pub hi: u32,
    pub lo: u32,
}

/// 64-bit sample counter.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ASIOSamples {
    pub hi: u32,
    pub lo: u32,
}

/// Comprehensive ASIO time structure passed to `bufferSwitchTimeInfo`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ASIOTime {
    pub reserved: [i32; 4],
    pub time_stamp: ASIOTimeStamp,
    pub sample_position: ASIOSamples,
    pub sample_rate: f64,
    pub flags: u32,
}

/// Callback function pointers provided by host to driver during `createBuffers`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ASIOCallbacks {
    /// Ping-pong buffer switch callback: `fn(doubleBufferIndex: i32, directProcess: ASIOBool)`.
    pub buffer_switch: Option<unsafe extern "C" fn(double_buffer_index: i32, direct_process: ASIOBool)>,
    /// Sample rate change notification.
    pub sample_rate_did_change: Option<unsafe extern "C" fn(sample_rate: f64)>,
    /// Driver message handler.
    pub asio_message: Option<unsafe extern "C" fn(selector: i32, value: i32, message: *mut c_void, opt: *mut f64) -> i32>,
    /// Time-stamped buffer switch callback.
    pub buffer_switch_time_info: Option<unsafe extern "C" fn(params: *mut ASIOTime, double_buffer_index: i32, direct_process: ASIOBool) -> *mut ASIOTime>,
}
