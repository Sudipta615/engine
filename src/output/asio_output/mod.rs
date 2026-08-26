//! Native Steinberg ASIO output backend in pure Rust (no C++ SDK required).
//!
//! Provides zero-latency, bit-perfect streaming directly to professional ASIO DACs
//! and audio interfaces on Windows, with support for native 1-bit DSD and hardware
//! buffer size control.
//!
//! ## Architecture
//!
//! ```text
//! engine (DSP) ──push──▶ FixedFrameBuffer ──pull──▶ bufferSwitch callback
//!                                                    │ (ASIO driver audio thread)
//!                                                    ▼
//!                              render_block → planar driver buffers
//! ```
//!
//! - **`new()`** — initializes COM (MTA), enumerates registry drivers,
//!   selects one by name/CLSID, instantiates the IASIO COM object via
//!   `CoCreateInstance` + `QueryInterface`, negotiates sample rate, channel
//!   count, and buffer size, then calls `createBuffers` with our
//!   `bufferSwitch` callback.
//! - **`start()`** — calls `IASIO::start()` to begin streaming; the driver
//!   then begins firing `bufferSwitch` on its internal audio thread.
//! - **`stop()`** — calls `IASIO::stop()`, `disposeBuffers()`, releases the
//!   COM interfaces, and uninitializes COM.

pub mod com;
pub mod render;
pub mod types;

use std::fmt;
#[cfg(windows)]
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use cpal::SampleFormat;

#[cfg(windows)]
use crate::buffer::DsdByteBuffer;
use crate::buffer::FixedFrameBuffer;
use crate::decode::dsd::DsdWireFormat;
use crate::dsp::pipeline::OutputSampleFormat;
use crate::output::capabilities::{OutputAccessMode, OutputAccessState, OutputCapabilities};
use crate::output::cpal_output::{OutputError, OutputVolume};
use crate::output::output::{NativeDsdCapability, Output, StreamErrorBatch, StreamErrorState};
use crate::output::output_info::OutputInfo;

#[cfg(windows)]
use com::enumerate_asio_drivers;
use com::AsioDriverInfo;
use render::AsioRenderContext;
#[cfg(windows)]
use types::*;

// ── Global state for the bufferSwitch callback ────────────────────────────────
//
// ASIO callbacks are bare `extern "C"` function pointers with no user-data
// parameter. We store the active state (buffer infos + render context) in a
// global atomic so the `bufferSwitch` callback can find its buffers.

/// State shared between the control thread and the ASIO `bufferSwitch` callback.
#[cfg(windows)]
struct CallbackState {
    buffer_infos: Vec<ASIOBufferInfo>,
    context: Arc<AsioRenderContext>,
    /// Whether the current stream is native-DSD (vs PCM). When true, the
    /// callback calls `render_block_dsd` instead of `render_block`.
    dsd_mode: bool,
}

/// Global active callback state. Set before `create_buffers`, cleared after
/// `dispose_buffers`. The `bufferSwitch` callback reads this on the driver's
/// audio thread.
#[cfg(windows)]
static ACTIVE_STATE: AtomicPtr<CallbackState> = AtomicPtr::new(std::ptr::null_mut());

/// Register the callback state, dropping any previous state.
#[cfg(windows)]
fn set_callback_state(state: Box<CallbackState>) {
    let old = ACTIVE_STATE.swap(Box::into_raw(state), Ordering::AcqRel);
    if !old.is_null() {
        unsafe {
            drop(Box::from_raw(old));
        }
    }
}

/// Clear the callback state and drop it.
#[cfg(windows)]
fn clear_callback_state() {
    let old = ACTIVE_STATE.swap(std::ptr::null_mut(), Ordering::AcqRel);
    if !old.is_null() {
        unsafe {
            drop(Box::from_raw(old));
        }
    }
}

/// ASIO `bufferSwitch` callback.
///
/// Called by the driver's internal audio thread when one half of the
/// double-buffer is ready to be filled. `double_buffer_index` is 0 or 1.
/// `direct_process` is non-zero when the driver requests direct (monitored)
/// processing; we ignore those and only fill on normal switches.
#[cfg(windows)]
unsafe extern "C" fn asio_buffer_switch(double_buffer_index: i32, direct_process: ASIOBool) {
    let state_ptr = ACTIVE_STATE.load(Ordering::Acquire);
    let Some(state) = state_ptr.as_ref() else {
        return;
    };

    // direct_process means the driver wants us to monitor, not fill.
    if direct_process != ASIO_FALSE || state.buffer_infos.is_empty() {
        return;
    }

    let buf_idx = double_buffer_index as usize;
    let buffer_ptrs: Vec<*mut std::ffi::c_void> = state
        .buffer_infos
        .iter()
        .map(|info| info.buffers[buf_idx])
        .collect();

    if state.dsd_mode {
        state
            .context
            .render_block_dsd(&buffer_ptrs, state.context.buffer_size_frames);
    } else {
        state
            .context
            .render_block(&buffer_ptrs, state.context.buffer_size_frames);
    }
}

/// Native ASIO Output endpoint.
pub struct AsioOutput {
    device_name: String,
    device_id: Option<String>,
    sample_rate: u32,
    channels: u16,
    sample_format: SampleFormat,
    buffer_size_frames: u32,
    context: Arc<AsioRenderContext>,
    errors: StreamErrorState,
    driver_info: Option<AsioDriverInfo>,
    /// Source→output channel remap (`map[out_ch] = source_ch`), shared with
    /// every (re)built render context so it survives DSD mode switches.
    channel_map: Arc<arc_swap::ArcSwap<Option<Vec<u16>>>>,
    /// The live IASIO COM driver (Windows only). `None` on non-Windows or
    /// when the driver could not be instantiated.
    #[cfg(windows)]
    driver: Option<com::AsioDriver>,
    /// COM initialized flag — `CoUninitialize` must be called exactly once
    /// per `CoInitializeEx`. Set true when `new()` succeeds.
    #[cfg(windows)]
    com_initialized: bool,
    /// True while the stream runs in native-DSD transport mode.
    #[cfg(windows)]
    dsd_active: bool,
    /// Negotiated DSD wire format while `dsd_active`.
    #[cfg(windows)]
    dsd_wire_format: Option<DsdWireFormat>,
    /// Byte ring drained by the DSD render callback.
    #[cfg(windows)]
    dsd_buffer: Option<Arc<DsdByteBuffer>>,
    /// The PCM sample rate to restore when leaving DSD mode.
    #[cfg(windows)]
    pcm_rate: u32,
    /// The PCM channel count to restore when leaving DSD mode.
    #[cfg(windows)]
    pcm_channels: u16,
    is_running: bool,
}

impl fmt::Debug for AsioOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AsioOutput")
            .field("device_name", &self.device_name)
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("buffer_size_frames", &self.buffer_size_frames)
            .field("is_running", &self.is_running)
            .finish()
    }
}

impl AsioOutput {
    /// Create and initialize an ASIO output stream.
    #[cfg(windows)]
    pub fn new(
        driver_name_or_id: Option<&str>,
        target_sample_rate: u32,
        ring_buffer: Arc<FixedFrameBuffer>,
    ) -> Result<Self, OutputError> {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

        // ── 1. Init COM ─────────────────────────────────────────────────
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() {
            return Err(OutputError::StreamOpen(format!(
                "CoInitializeEx failed: {hr:?}"
            )));
        }
        let com_initialized = true;

        // Helper to early-return with COM cleanup.
        let result = Self::new_inner(driver_name_or_id, target_sample_rate, ring_buffer);
        match result {
            Ok(out) => Ok(out),
            Err(e) => {
                if com_initialized {
                    unsafe { windows::Win32::System::Com::CoUninitialize() };
                }
                Err(e)
            }
        }
    }

    /// Inner constructor: COM is already initialized.
    #[cfg(windows)]
    fn new_inner(
        driver_name_or_id: Option<&str>,
        target_sample_rate: u32,
        ring_buffer: Arc<FixedFrameBuffer>,
    ) -> Result<Self, OutputError> {
        // ── 2. Enumerate and select driver ───────────────────────────────
        let drivers = enumerate_asio_drivers();
        let selected = if let Some(target) = driver_name_or_id {
            drivers.into_iter().find(|d| {
                d.name.eq_ignore_ascii_case(target) || d.clsid.eq_ignore_ascii_case(target)
            })
        } else {
            drivers.into_iter().next()
        };
        let driver_info = selected.ok_or_else(|| {
            OutputError::StreamOpen("No installed ASIO drivers found on this system".to_string())
        })?;

        log::info!(
            "ASIO: selected driver '{}' (CLSID {})",
            driver_info.description,
            driver_info.clsid
        );

        // ── 3. Instantiate COM object ────────────────────────────────────
        let driver = com::AsioDriver::create(&driver_info.clsid)?;

        // ── 4. Negotiate: init, channels, sample rate, buffer size ───────
        driver.init(std::ptr::null_mut())?;

        let (_num_inputs, num_outputs) = driver.get_channels()?;
        let channels = (num_outputs.max(2) as u16).min(16);
        log::info!(
            "ASIO: {} input(s), {} output(s); using {} output channels",
            _num_inputs,
            num_outputs,
            channels
        );

        let rate = target_sample_rate.max(44100) as f64;
        driver.can_sample_rate(rate)?;
        driver.set_sample_rate(rate)?;
        let actual_rate = driver.get_sample_rate()?;
        log::info!("ASIO: sample rate negotiated: {} Hz", actual_rate);

        let (_min, _max, preferred, _gran) = driver.get_buffer_size()?;
        let buffer_size = (preferred.max(64) as u32).min(8192);
        log::info!(
            "ASIO: buffer size: {} frames (preferred {preferred})",
            buffer_size
        );

        // ── 5. Create render context ─────────────────────────────────────
        let sample_format = SampleFormat::F32;
        let channel_map = Arc::new(arc_swap::ArcSwap::from_pointee(None::<Vec<u16>>));
        let mut context = AsioRenderContext::new(
            ring_buffer,
            channels as usize,
            ASIOSampleType::Float32LSB,
            buffer_size as usize,
        );
        context.channel_map = Arc::clone(&channel_map);
        let context = Arc::new(context);

        // ── 6. Allocate buffer infos and call create_buffers ─────────────
        let mut buffer_infos: Vec<ASIOBufferInfo> = (0..channels)
            .map(|ch| ASIOBufferInfo {
                is_input: ASIO_FALSE,
                channel_num: ch as i32,
                buffers: [std::ptr::null_mut(), std::ptr::null_mut()],
            })
            .collect();

        let mut callbacks = ASIOCallbacks {
            buffer_switch: Some(asio_buffer_switch),
            sample_rate_did_change: None,
            asio_message: None,
            buffer_switch_time_info: None,
        };

        driver.create_buffers(&mut buffer_infos, buffer_size as i32, &mut callbacks)?;

        // Verify the driver filled in the buffer pointers.
        if buffer_infos.iter().any(|info| info.buffers[0].is_null()) {
            return Err(OutputError::StreamOpen(
                "ASIO create_buffers succeeded but did not fill buffer pointers".into(),
            ));
        }
        log::info!("ASIO: create_buffers succeeded ({channels} ch × {buffer_size} frames)");

        // ── 7. Register global callback state ────────────────────────────
        set_callback_state(Box::new(CallbackState {
            buffer_infos,
            context: Arc::clone(&context),
            dsd_mode: false,
        }));

        let device_name = driver_info.description.clone();
        let device_id = Some(driver_info.clsid.clone());

        Ok(Self {
            device_name,
            device_id,
            sample_rate: actual_rate as u32,
            channels,
            sample_format,
            buffer_size_frames: buffer_size,
            context,
            errors: StreamErrorState::default(),
            driver_info: Some(driver_info),
            channel_map,
            driver: Some(driver),
            com_initialized: true,
            dsd_active: false,
            dsd_wire_format: None,
            dsd_buffer: None,
            pcm_rate: actual_rate as u32,
            pcm_channels: channels,
            is_running: false,
        })
    }

    /// Stub for non-Windows: returns an error since ASIO is Windows-only.
    #[cfg(not(windows))]
    pub fn new(
        _driver_name_or_id: Option<&str>,
        _target_sample_rate: u32,
        _ring_buffer: Arc<FixedFrameBuffer>,
    ) -> Result<Self, OutputError> {
        Err(OutputError::StreamOpen(
            "ASIO backend is Windows-only".to_string(),
        ))
    }

    /// Information about the active ASIO driver.
    pub fn driver_info(&self) -> Option<&AsioDriverInfo> {
        self.driver_info.as_ref()
    }

    /// Release buffers and driver, clearing the global callback state.
    #[cfg(windows)]
    fn teardown(&mut self) {
        clear_callback_state();
        if let Some(ref driver) = self.driver {
            driver.dispose_buffers();
        }
        self.driver = None;
        if self.com_initialized {
            self.com_initialized = false;
            unsafe {
                windows::Win32::System::Com::CoUninitialize();
            }
        }
    }

    #[cfg(not(windows))]
    fn teardown(&mut self) {}

    /// True when the current stream is in native-DSD transport mode.
    #[inline]
    fn is_dsd_active(&self) -> bool {
        #[cfg(windows)]
        {
            self.dsd_active
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// Map the engine's DSD wire format to the ASIO sample type used at
    /// `create_buffers`. `DSD_U8` maps to `DSDInt8LSB1` (8 DSD samples
    /// per byte, LSB-first — the standard ASIO DSD format). `DSD_U32_LE`
    /// maps to `Int32LSB` (32-bit container, which some DSD DACs use for
    /// native-DSD transport over ASIO at DSD128+).
    ///
    /// Formats without a direct ASIO mapping (`U16Be`, `U32Be`) return
    /// `None`; the negotiation loop will try the next preference.
    #[cfg(windows)]
    fn asio_dsd_type(wire: DsdWireFormat) -> Option<ASIOSampleType> {
        match wire {
            DsdWireFormat::U8 => Some(ASIOSampleType::DSDInt8LSB1),
            DsdWireFormat::U16Le => Some(ASIOSampleType::Int16LSB),
            DsdWireFormat::U32Le => Some(ASIOSampleType::Int32LSB),
            DsdWireFormat::U16Be => None,
            DsdWireFormat::U32Be => None,
        }
    }

    /// Byte width per channel per ASIO DSD word.
    #[cfg(windows)]
    fn asio_dsd_byte_width(st: ASIOSampleType) -> usize {
        match st {
            ASIOSampleType::DSDInt8LSB1
            | ASIOSampleType::DSDInt8MSB1
            | ASIOSampleType::DSDInt8NER8 => 1,
            ASIOSampleType::Int16LSB | ASIOSampleType::Int16MSB => 2,
            ASIOSampleType::Int32LSB | ASIOSampleType::Int32MSB => 4,
            _ => 4,
        }
    }

    /// Dispose existing buffers and re-create them with `asio_type` at
    /// `rate` Hz for `channels` outputs. Updates `self.context`,
    /// `self.buffer_size_frames`, `self.sample_rate`, and the global
    /// callback state.
    ///
    /// Takes the driver from `self` rather than a parameter so callers never
    /// hold a borrow of `self.driver` across a `&mut self` call.
    #[cfg(windows)]
    fn recreate_buffers(
        &mut self,
        rate: f64,
        channels: u16,
        asio_type: ASIOSampleType,
        dsd_mode: bool,
    ) -> Result<(), OutputError> {
        let driver = self
            .driver
            .as_ref()
            .ok_or_else(|| OutputError::StreamError("no ASIO driver loaded".to_string()))?;
        driver.can_sample_rate(rate)?;
        driver.set_sample_rate(rate)?;
        let actual = driver.get_sample_rate()?;

        let (_min, _max, preferred, _gran) = driver.get_buffer_size()?;
        let buf_size = (preferred.max(64) as u32).min(8192);

        // Build ring-backed context with DSD ring wired when in DSD mode.
        let mut ctx = AsioRenderContext::new(
            Arc::clone(&self.context.ring_buffer),
            channels as usize,
            if dsd_mode {
                asio_type
            } else {
                ASIOSampleType::Float32LSB
            },
            buf_size as usize,
        );
        ctx.channel_map = Arc::clone(&self.channel_map);
        if dsd_mode {
            ctx.dsd_buffer = self.dsd_buffer.clone();
            ctx.dsd_frame_width = channels as usize * Self::asio_dsd_byte_width(asio_type);
        }
        let context = Arc::new(ctx);

        let mut buffer_infos: Vec<ASIOBufferInfo> = (0..channels)
            .map(|ch| ASIOBufferInfo {
                is_input: ASIO_FALSE,
                channel_num: ch as i32,
                buffers: [std::ptr::null_mut(), std::ptr::null_mut()],
            })
            .collect();

        let mut callbacks = ASIOCallbacks {
            buffer_switch: Some(asio_buffer_switch),
            sample_rate_did_change: None,
            asio_message: None,
            buffer_switch_time_info: None,
        };

        driver.create_buffers(&mut buffer_infos, buf_size as i32, &mut callbacks)?;
        if buffer_infos.iter().any(|info| info.buffers[0].is_null()) {
            return Err(OutputError::StreamOpen(
                "ASIO create_buffers succeeded but did not fill buffer pointers".into(),
            ));
        }

        set_callback_state(Box::new(CallbackState {
            buffer_infos,
            context: Arc::clone(&context),
            dsd_mode,
        }));

        self.context = context;
        self.buffer_size_frames = buf_size;
        self.sample_rate = actual as u32;
        Ok(())
    }
}

impl OutputVolume for AsioOutput {
    fn supports_hardware_volume(&self) -> bool {
        // ASIO streams drive the hardware DAC directly without OS volume intermediate
        false
    }

    fn set_hardware_volume_db(&self, _db: f32) -> Result<(), OutputError> {
        Err(OutputError::StreamError(
            "Hardware volume is not supported on ASIO".to_string(),
        ))
    }
}

impl Output for AsioOutput {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn sample_format(&self) -> SampleFormat {
        self.sample_format
    }

    fn buffer_size_frames(&self) -> u32 {
        self.buffer_size_frames
    }

    fn output_info(&self) -> OutputInfo {
        let verified = self.is_running;
        let dsd = self.is_dsd_active();
        let (access_mode, sample_format) = if dsd {
            (
                OutputAccessMode::BitstreamPassthrough,
                OutputSampleFormat::Unknown,
            )
        } else {
            (OutputAccessMode::Exclusive, OutputSampleFormat::F32)
        };
        OutputInfo {
            device_name: self.device_name.clone(),
            actual_backend: Some(config::AudioBackend::ExclusiveAsio),
            requested_backend: Some(config::AudioBackend::ExclusiveAsio),
            requested_rate: self.sample_rate,
            actual_rate: self.sample_rate,
            channels: self.channels,
            buffer_size_frames: self.buffer_size_frames,
            buffer_size_estimated: false,
            sample_format,
            dither_enabled: !dsd,
            is_fallback: false,
            fallback_reason: None,
            is_exclusive: true,
            access_mode,
            access_state: OutputAccessState {
                requested: OutputAccessMode::Exclusive,
                actual: access_mode,
                verified,
            },
        }
    }

    fn capabilities(&self) -> OutputCapabilities {
        let dsd = self.is_dsd_active();
        let access_mode = if dsd {
            OutputAccessMode::BitstreamPassthrough
        } else {
            OutputAccessMode::Exclusive
        };
        OutputCapabilities {
            sample_rates: vec![
                44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000, 705600, 768000,
            ],
            hardware_ranges: vec![(44100, 768000)],
            formats: vec![SampleFormat::F32, SampleFormat::I32, SampleFormat::I16],
            channels: vec![2, 6, 8, 12, 16],
            device_name: self.device_name.clone(),
            access_mode,
            access_state: OutputAccessState {
                requested: OutputAccessMode::Exclusive,
                actual: access_mode,
                verified: self.is_running,
            },
            likely_direct_access: true,
            supports_exclusive: true,
        }
    }

    fn device_name(&self) -> String {
        self.device_name.clone()
    }

    fn device_id(&self) -> Option<String> {
        self.device_id.clone()
    }

    fn reconfigure_sample_rate(&mut self, target_sample_rate: u32) -> Result<u32, OutputError> {
        if self.is_dsd_active() {
            return Err(OutputError::StreamError(
                "cannot reconfigure a DSD stream to a PCM rate".into(),
            ));
        }
        if target_sample_rate == self.sample_rate {
            return Ok(self.sample_rate);
        }
        #[cfg(windows)]
        if let Some(ref driver) = self.driver {
            let stopped = self.is_running;
            if stopped {
                let _ = driver.stop();
                self.context.active.store(false, Ordering::Relaxed);
            }
            driver.can_sample_rate(target_sample_rate as f64)?;
            driver.set_sample_rate(target_sample_rate as f64)?;
            let actual = driver.get_sample_rate()?;
            self.sample_rate = actual as u32;
            if stopped {
                self.context.active.store(true, Ordering::Relaxed);
                driver.start()?;
            }
            return Ok(self.sample_rate);
        }
        self.sample_rate = target_sample_rate;
        Ok(self.sample_rate)
    }

    fn reconfigure_sample_format(
        &mut self,
        target_sample_rate: u32,
        sample_format: SampleFormat,
    ) -> Result<u32, OutputError> {
        self.sample_format = sample_format;
        self.reconfigure_sample_rate(target_sample_rate)
    }

    fn reset_buffer(&self) {
        self.context.ring_buffer.reset();
    }

    fn take_underruns(&self) -> u32 {
        self.context.underrun_count.swap(0, Ordering::Relaxed)
    }

    fn take_clips(&self) -> u32 {
        self.context.clip_count.swap(0, Ordering::Relaxed)
    }

    fn take_nans(&self) -> u32 {
        self.context.nan_count.swap(0, Ordering::Relaxed)
    }

    fn take_stream_errors(&self) -> StreamErrorBatch {
        self.errors.take()
    }

    fn set_dither_enabled(&self, enabled: bool) {
        self.context
            .dither_enabled
            .store(enabled, Ordering::Relaxed);
    }

    fn pause(&self) {
        self.context.active.store(false, Ordering::Relaxed);
    }

    fn resume(&self) {
        self.context.active.store(true, Ordering::Relaxed);
    }

    fn start(&mut self) -> Result<(), OutputError> {
        #[cfg(windows)]
        if let Some(ref driver) = self.driver {
            if !self.is_running {
                driver.start()?;
                log::info!(
                    "ASIO stream started: {} Hz, {} ch",
                    self.sample_rate,
                    self.channels
                );
            }
        }
        self.context.active.store(true, Ordering::Relaxed);
        self.is_running = true;
        Ok(())
    }

    fn stop(&mut self) {
        self.context.active.store(false, Ordering::Relaxed);
        #[cfg(windows)]
        if let Some(ref driver) = self.driver {
            let _ = driver.stop();
        }
        self.is_running = false;
    }

    fn native_dsd_capabilities(&self) -> Vec<DsdWireFormat> {
        vec![DsdWireFormat::U8, DsdWireFormat::U32Le]
    }

    fn native_dsd_capability_matrix(&self) -> Vec<NativeDsdCapability> {
        vec![
            NativeDsdCapability {
                wire_format: DsdWireFormat::U8,
                bit_rates: vec![2_822_400, 5_644_800, 11_289_600, 22_579_200],
                channels: vec![2, 6, 8],
            },
            NativeDsdCapability {
                wire_format: DsdWireFormat::U32Le,
                bit_rates: vec![2_822_400, 5_644_800, 11_289_600, 22_579_200],
                channels: vec![2, 6, 8],
            },
        ]
    }

    fn set_native_dsd(
        &mut self,
        params: Option<crate::output::output::NativeDsdParams>,
    ) -> Result<Option<DsdWireFormat>, OutputError> {
        match params {
            None => {
                // Leave native-DSD mode: dispose DSD buffers, rebuild PCM.
                #[cfg(windows)]
                if self.dsd_active {
                    if self.driver.is_some() {
                        if self.is_running {
                            let _ = self.driver.as_ref().unwrap().stop();
                        }
                        self.driver.as_ref().unwrap().dispose_buffers();
                        self.recreate_buffers(
                            self.pcm_rate as f64,
                            self.pcm_channels,
                            ASIOSampleType::Float32LSB,
                            false,
                        )?;
                        if self.is_running {
                            self.driver.as_ref().unwrap().start()?;
                        }
                    }
                    self.context
                        .active
                        .store(self.is_running, Ordering::Relaxed);
                    self.dsd_active = false;
                    self.dsd_wire_format = None;
                    self.dsd_buffer = None;
                    self.sample_format = SampleFormat::F32;
                }
                Ok(None)
            }
            Some(p) => {
                #[cfg(not(windows))]
                {
                    let _ = p;
                }
                #[cfg(windows)]
                {
                    // Negotiate: map wire format → ASIO sample type, stop
                    // PCM stream, rebuild with DSD buffers.
                    if self.driver.is_none() {
                        return Err(OutputError::StreamError(
                            "no ASIO driver loaded".to_string(),
                        ));
                    }

                    // Take the DSD ring out of `p` once: the negotiation loop
                    // below may retry several wire formats, and the ring is
                    // not `Copy`.
                    let dsd_buffer = p.buffer;
                    let bit_rate = p.bit_rate;
                    let channels = p.channels;

                    // Try the requested wire format first, then fall back.
                    let order: [DsdWireFormat; 3] =
                        [p.wire_format, DsdWireFormat::U8, DsdWireFormat::U32Le];
                    let mut last_err = None;
                    for &wire in &order {
                        let Some(asio_type) = Self::asio_dsd_type(wire) else {
                            if last_err.is_none() {
                                last_err = Some(OutputError::StreamError(format!(
                                    "no ASIO mapping for DSD format {}",
                                    wire.label()
                                )));
                            }
                            continue;
                        };

                        let frame_rate = wire.frame_rate_hz(bit_rate) as f64;
                        if self.is_running {
                            let _ = self.driver.as_ref().unwrap().stop();
                        }
                        self.driver.as_ref().unwrap().dispose_buffers();

                        // Save PCM state before switching to DSD.
                        self.pcm_rate = self.sample_rate;
                        self.pcm_channels = self.channels;

                        self.dsd_active = true;
                        self.dsd_wire_format = Some(wire);
                        self.dsd_buffer = Some(std::sync::Arc::clone(&dsd_buffer));

                        match self.recreate_buffers(frame_rate, channels, asio_type, true) {
                            Ok(()) => {
                                log::info!(
                                    "ASIO: native DSD negotiated {} ({} format) at {} Hz",
                                    wire.label(),
                                    asio_type.as_i32(),
                                    frame_rate
                                );
                                if self.is_running {
                                    self.driver.as_ref().unwrap().start()?;
                                }
                                self.context
                                    .active
                                    .store(self.is_running, Ordering::Relaxed);
                                self.sample_format = SampleFormat::I32;
                                return Ok(Some(wire));
                            }
                            Err(e) => {
                                self.dsd_active = false;
                                self.dsd_wire_format = None;
                                self.dsd_buffer = None;
                                last_err = Some(e);
                            }
                        }
                    }
                    Err(last_err.unwrap_or_else(|| {
                        OutputError::StreamError(
                            "no native DSD format could be negotiated on this ASIO driver".into(),
                        )
                    }))
                }
                #[cfg(not(windows))]
                Err(OutputError::StreamError(
                    "ASIO DSD transport is Windows-only".into(),
                ))
            }
        }
    }

    fn open_control_panel(&self) -> Result<(), OutputError> {
        #[cfg(all(windows, feature = "asio-native"))]
        if let Some(ref driver) = self.driver {
            return driver.control_panel();
        }
        Ok(())
    }
}

impl AsioOutput {
    /// Install a source→output channel remap: `map[out_ch] = source_ch`.
    /// `None` (or an empty slice) restores the identity mapping. Applied
    /// lock-free on the next render block, so it can be called while the
    /// stream is running. Output channels whose source index is out of
    /// range are silenced.
    pub fn set_channel_map(&self, map: Option<&[u16]>) {
        let new = map.map(|m| m.to_vec()).filter(|m| !m.is_empty());
        self.channel_map.store(Arc::new(new));
    }
}

impl Drop for AsioOutput {
    fn drop(&mut self) {
        if self.is_running {
            self.stop();
        }
        self.teardown();
    }
}
