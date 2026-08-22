#[cfg(target_os = "linux")]
pub mod alsa_output;
// Native CoreAudio hog-mode exclusive backend (no cpal). macOS-only.
pub mod asio_output;
pub mod capabilities;
#[cfg(target_os = "macos")]
pub mod coreaudio_output;
pub mod cpal_callbacks;
pub mod cpal_devices;
pub mod cpal_output;
pub mod device_match;
pub mod device_monitor;
pub mod format_converter;
pub mod output;
pub mod output_info;
pub mod output_profile;
pub mod rate_policy;
// Native WASAPI exclusive-mode backend (no cpal). Windows-only, opt-in via
// the `wasapi-native` feature.
#[cfg(all(target_os = "windows", feature = "wasapi-native"))]
pub mod wasapi_output;

pub use asio_output::AsioOutput;
pub use capabilities::{OutputAccessMode, OutputCapabilities, OutputValidationError};
pub use cpal_output::{CpalOutput, OutputError, OutputVolume};
pub use device_match::{classify_device_name_match, DeviceNameMatch};
pub use device_monitor::{DeviceDelta, DeviceMonitor};
pub use format_converter::{AudioFormatConverter, TargetFormat};
pub use output::{
    create_output, NativeDsdCapability, NativeDsdParams, Output, StreamErrorBatch,
    StreamErrorEvent, StreamErrorKind, StreamErrorState,
};
pub use output_info::OutputInfo;
pub use output_profile::{OutputProfile, OutputProfileLibrary};
pub use rate_policy::SampleRatePolicy;
