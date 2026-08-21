//! State and buffers for native DSD bitstream and DoP transport.

use std::sync::Arc;
use crate::buffer::DsdByteBuffer;
use crate::decode::dsd::DsdWireFormat;
use crate::decode::DsdTransportReport;

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
