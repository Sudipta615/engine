#[cfg(target_os = "linux")]
pub mod alsa_output;
// Native Steinberg ASIO backend in pure Rust — bypasses cpal entirely.
// Gated behind the `asio-native` feature; hosts that don't need pro-audio
// ASIO don't pay the compile cost of the COM vtable, IASIO wrappers, and
// the `windows` crate's registry enumeration.
#[cfg(feature = "asio-native")]
pub mod asio_output;
pub mod capabilities;
#[cfg(target_os = "macos")]
pub mod coreaudio_output;
pub mod cpal_callbacks;
pub mod cpal_devices;
pub mod cpal_output;
pub mod device_match;
pub mod device_monitor;
pub mod endpoint;
pub mod format_converter;
// The directory `src/output/` holds the `output.rs` core trait + factory
// alongside the backend modules; the naming is intentional.
#[allow(clippy::module_inception)]
pub mod output;
pub mod output_info;
pub mod output_profile;
pub mod rate_policy;
pub mod wav_writer;
// Native WASAPI exclusive-mode backend (no cpal). Windows-only, opt-in via
// the `wasapi-native` feature.
#[cfg(all(target_os = "windows", feature = "wasapi-native"))]
pub mod wasapi_output;
// WASAPI loopback capture — records the system mix. Windows-only; same
// feature gate as the native WASAPI backend (it reuses its COM plumbing).
#[cfg(all(target_os = "windows", feature = "wasapi-native"))]
pub mod wasapi_loopback;
#[cfg(all(target_os = "windows", feature = "wasapi-native"))]
pub use wasapi_loopback::WasapiLoopbackCapture;

#[cfg(feature = "asio-native")]
pub use asio_output::AsioOutput;
pub use capabilities::{OutputAccessMode, OutputCapabilities, OutputValidationError};
pub use cpal_output::{CpalOutput, OutputError, OutputVolume};
pub use device_match::{classify_device_name_match, DeviceNameMatch};
pub use device_monitor::{DeviceDelta, DeviceMonitor};
pub use endpoint::{
    EndpointConfig, EndpointId, EndpointRegistry, EndpointRing, EndpointStats, EndpointWorker,
    VirtualEndpoint,
};
pub use format_converter::{AudioFormatConverter, TargetFormat};
pub use output::{
    create_output, NativeDsdCapability, NativeDsdParams, Output, StreamErrorBatch,
    StreamErrorEvent, StreamErrorKind, StreamErrorState,
};
pub use output_info::OutputInfo;
pub use output_profile::{OutputProfile, OutputProfileLibrary};
pub use rate_policy::SampleRatePolicy;
