//! State and buffers for native DSD bitstream and DoP transport.

use crate::buffer::DsdByteBuffer;
use crate::decode::dsd::DsdWireFormat;
use crate::decode::DsdTransportReport;
use std::sync::Arc;

/// Build the reason text for why DoP cannot engage because the output is not
/// exclusive (used by the `load_track` fallback warning). When the user is on
/// the Auto/shared backend it names a concrete exclusive backend to switch to,
/// so the failure is actionable rather than a dead-end log line.
pub fn dop_exclusive_reason(
    out_info: &crate::output::OutputInfo,
    requested_backend: config::AudioBackend,
) -> String {
    if out_info.is_fallback {
        match &out_info.fallback_reason {
            Some(r) => format!("the exclusive backend request fell back to a shared device ({r})"),
            None => "the exclusive backend request fell back to a shared device".to_string(),
        }
    } else if requested_backend == config::AudioBackend::Auto {
        let exclusive_names = if cfg!(target_os = "linux") {
            "ExclusiveAlsa (or select a direct hw: device)".to_string()
        } else if cfg!(target_os = "windows") {
            "ExclusiveAsio (WASAPI exclusive requires a native IAudioClient \
             backend not available through cpal)"
                .to_string()
        } else {
            "a native CoreAudio hog-mode backend (not available through cpal)".to_string()
        };
        format!(
            "the backend is Auto (shared device); switch to an exclusive backend \
             ({exclusive_names})"
        )
    } else if requested_backend == config::AudioBackend::ExclusiveAsio
        && !cfg!(feature = "asio")
        && !cfg!(feature = "asio-native")
    {
        "the ASIO backend is not compiled in (enable the 'asio-native' or 'asio' feature)"
            .to_string()
    } else {
        "the selected backend did not provide exclusive access".to_string()
    }
}

pub(crate) struct DsdTransportState {
    /// True when the current track is being output as DSD-over-PCM (DoP): raw
    /// DSD packed into 24-bit frames, DSP fully bypassed, no resampling.
    pub(crate) dop_active: bool,
    /// Output rate of the active DoP stream (bit_rate / 16); 0 when inactive.
    pub(crate) dop_rate: u32,
    /// True when the current track is being output as native DSD: raw 1-bit
    /// bitstream to a DSD-capable DAC, entire f32 DSP path structurally
    /// bypassed (no decimation, no resampling, no filters).
    pub(crate) native_dsd_active: bool,
    /// Negotiated native-DSD wire format (e.g. DSD_U8); `None` when native
    /// DSD is not active.
    pub(crate) dsd_wire_format: Option<DsdWireFormat>,
    /// Byte ring the engine's native-DSD path pushes raw interleaved DSD
    /// bytes into; the DSD-capable output backend drains it to the DAC.
    pub(crate) dsd_byte_buffer: Option<Arc<DsdByteBuffer>>,
    /// Reusable packed-wire scratch buffer for the native-DSD path
    /// (allocated once; the hot path stays allocation-free).
    pub(crate) dsd_pack_scratch: Vec<u8>,
    /// Explicit DSD transport negotiation report (§7): requested vs actual
    /// transport plus ordered fallback steps (native → DoP → PCM).
    pub(crate) dsd_transport_report: DsdTransportReport,
}

impl Default for DsdTransportState {
    fn default() -> Self {
        Self {
            dop_active: false,
            dop_rate: 0,
            native_dsd_active: false,
            dsd_wire_format: None,
            dsd_byte_buffer: None,
            dsd_pack_scratch: Vec::with_capacity(1 << 16),
            dsd_transport_report: DsdTransportReport::default(),
        }
    }
}
