//! High-quality audio resampler using rubato
//!
//! Supports three quality profiles using rubato's FFT-based synchronous resamplers.
//! Handles sample rate conversion between the decoder's source rate and the output
//! device rate, as well as variable-speed playback by adjusting the resampling ratio.
//! Supports both f32 and f64 sample types. All buffers are pre-allocated for
//! zero-allocation operation during playback.
//!
//! # Phase behavior (spec §14, §33)
//!
//! Every tier is a **linear-phase** resampler by construction: rubato's
//! `Fft` resampler convolves each chunk with a *symmetric* (zero
//! group-delay-vs-frequency) windowed-sinc low-pass in the frequency domain,
//! so the filter has a constant group delay of exactly
//! [`AudioResampler::latency_samples`] samples (rubato's `output_delay()`)
//! at **all** frequencies. This is the classic linear-phase trade-off,
//! stated honestly rather than as a marketing claim:
//!
//! - **Linear phase means a constant, reportable group delay** — the engine
//!   adds it to the pipeline latency model (spec §19) and compensates
//!   logical playback position with it. No frequency-dependent smearing of
//!   transients, at the cost of `latency_samples` of pre-ring before a
//!   transient appears.
//! - **Minimum-phase is NOT exposed.** A minimum-phase variant would remove
//!   pre-ring at the price of frequency-dependent group delay and a
//!   non-symmetric impulse response; the current engine deliberately keeps a
//!   single well-measured phase behavior rather than an unvalidated option
//!   (spec §14: "Do not expose a setting unless it corresponds to a real,
//!   measurable algorithmic change").
//!
//! The linear-phase claim is enforced by `tests/fidelity/resampler_measurement.rs`
//! (which measures the realized filters) and by the golden vectors in
//! `tests/fidelity/transition_tails.rs` (delay = `latency_samples`, output
//! conserves signal energy exactly).

use config::ResamplerQuality;
use num_traits::Float;
use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Fft, FixedSync, Resampler, WindowFunction};

/// Error type for resampler construction failures.
#[derive(Debug, thiserror::Error)]
pub enum ResamplerError {
    #[error("Failed to create {quality:?} resampler: {reason}")]
    CreationFailed {
        quality: ResamplerQuality,
        reason: String,
    },
    #[error("Invalid sample rate: source={source_rate}, output={output_rate}")]
    InvalidRates {
        source_rate: usize,
        output_rate: usize,
    },
}

/// Number of channels (stereo)
const CHANNELS: usize = 2;

/// Processing chunk size in frames
const CHUNK_SIZE: usize = 1024;

/// Maximum upsample ratio supported. 44100 → 768000 ≈ 17.4×; round up to 20×.
const MAX_RATIO: usize = 20;

/// Maximum output buffer frames: enough for the worst supported ratio.
/// Sized at CHUNK_SIZE × MAX_RATIO plus a filter margin.
///
/// Public so the engine can size its crossfade scratch buffers to the same
/// worst-case expansion: a realtime block of source frames can produce up to
/// this many resampled output frames before the resampler's own output buffer
/// would overflow.
pub const MAX_OUTPUT_BUFFER_FRAMES: usize = CHUNK_SIZE * MAX_RATIO + 512;

/// Maximum consecutive rebuild failures before disabling the resampler
const MAX_REBUILD_FAILURES: u32 = 5;

/// Enum-based dispatch to avoid dynamic trait objects
enum ResamplerInner<T: rubato::Sample + Send + Sync + 'static = f32> {
    /// High quality: Fft with a longer filter for better anti-aliasing
    HighQuality(Fft<T>),
    /// Ultra: Fft with the longest filter (single sub-chunk, so the
    /// filter length is derived from the full 2048-frame chunk)
    Ultra(Fft<T>),
    /// Balanced: Fft with moderate filter sizes
    Balanced(Fft<T>),
    /// Fast: Fft with minimal processing (fixed input+output chunk sizes)
    Fast(Fft<T>),
}

impl<T: rubato::Sample + Send + Sync + 'static> ResamplerInner<T> {
    fn input_frames_next(&self) -> usize {
        match self {
            Self::HighQuality(r) => r.input_frames_next(),
            Self::Ultra(r) => r.input_frames_next(),
            Self::Balanced(r) => r.input_frames_next(),
            Self::Fast(r) => r.input_frames_next(),
        }
    }

    /// Maximum possible output frames per chunk (bounds the scratch buffers).
    fn output_frames_max(&self) -> usize {
        match self {
            Self::HighQuality(r) => r.output_frames_max(),
            Self::Ultra(r) => r.output_frames_max(),
            Self::Balanced(r) => r.output_frames_max(),
            Self::Fast(r) => r.output_frames_max(),
        }
    }

    /// Output frames the next `process_into_buffer` call will write.
    fn output_frames_next(&self) -> usize {
        match self {
            Self::HighQuality(r) => r.output_frames_next(),
            Self::Ultra(r) => r.output_frames_next(),
            Self::Balanced(r) => r.output_frames_next(),
            Self::Fast(r) => r.output_frames_next(),
        }
    }

    /// Filter group delay in *output* frames (rubato's authoritative
    /// `Resampler::output_delay`).
    fn output_delay(&self) -> usize {
        match self {
            Self::HighQuality(r) => r.output_delay(),
            Self::Ultra(r) => r.output_delay(),
            Self::Balanced(r) => r.output_delay(),
            Self::Fast(r) => r.output_delay(),
        }
    }

    /// Resample one chunk into the pre-allocated planar scratch buffers.
    ///
    /// `buffer_in`/`buffer_out` are `audioadapter` wrappers over the
    /// pre-allocated `input_buffers`/`scratch` Vecs in `AudioResampler`;
    /// building them allocates nothing and cannot fail (the sizing is
    /// enforced by the caller before the call).
    fn process_into_buffer(
        &mut self,
        buffer_in: &dyn rubato::audioadapter::Adapter<T>,
        buffer_out: &mut dyn rubato::audioadapter::AdapterMut<T>,
    ) -> Result<(usize, usize), rubato::ResampleError> {
        match self {
            Self::HighQuality(r) => r.process_into_buffer(buffer_in, buffer_out, None),
            Self::Ultra(r) => r.process_into_buffer(buffer_in, buffer_out, None),
            Self::Balanced(r) => r.process_into_buffer(buffer_in, buffer_out, None),
            Self::Fast(r) => r.process_into_buffer(buffer_in, buffer_out, None),
        }
    }

    fn quality(&self) -> ResamplerQuality {
        match self {
            Self::HighQuality(_) => ResamplerQuality::HighQuality,
            Self::Ultra(_) => ResamplerQuality::Ultra,
            Self::Balanced(_) => ResamplerQuality::Balanced,
            Self::Fast(_) => ResamplerQuality::Fast,
        }
    }
}

/// High-quality resampler with configurable quality profiles and sample type.
pub struct AudioResampler<T: rubato::Sample + Float + Default + Send + Sync + 'static = f32> {
    /// Inner resampler using enum dispatch
    inner: ResamplerInner<T>,
    /// Source sample rate
    source_rate: usize,
    /// Output sample rate
    output_rate: usize,
    /// The quality profile the caller requested (may differ from the
    /// effective one if construction fell back, e.g. HighQuality → Fast).
    requested_quality: ResamplerQuality,
    /// Playback speed multiplier (1.0 = normal)
    speed: f32,
    /// Input buffer for accumulating samples before processing
    input_buffers: [Vec<T>; CHANNELS],
    /// Write position in input buffers
    input_pos: usize,
    /// Output ring buffer for samples waiting to be consumed
    output_buffers: [Vec<T>; CHANNELS],
    /// Read position in output buffers
    output_read_pos: usize,
    /// Number of valid samples in output buffers
    output_available: usize,
    /// Whether the resampler needs to be reconfigured
    needs_rebuild: bool,
    /// Pending quality change to apply on next rebuild
    pending_quality: Option<ResamplerQuality>,
    /// After MAX_REBUILD_FAILURES consecutive failures, the resampler
    /// is disabled.
    rebuild_failures: u32,
    disabled: bool,
    /// Receiver for the background thread that builds the new resampler
    rebuild_rx: Option<crossbeam::channel::Receiver<Result<ResamplerInner<T>, ResamplerError>>>,
    /// Recent output samples for crossfade during rebuild (reduces glitches)
    crossfade_buffer: [(T, T); 64],
    /// Current read position in crossfade_buffer
    crossfade_pos: usize,
    /// Number of crossfade samples remaining to blend
    crossfade_remaining: usize,
    crossfade_blend_total: usize,
    rebuilt_effective_source: usize,
    rebuilt_output_rate: usize,
    rebuilt_quality: ResamplerQuality,
    /// Pre-allocated planar scratch buffers for process_chunk (sized for
    /// MAX_RATIO and `output_frames_max`). Using Vecs avoids large stack
    /// frames and handles extreme upsample ratios.
    scratch: [Vec<T>; CHANNELS],
}

pub type AudioResamplerF32 = AudioResampler<f32>;
pub type AudioResamplerF64 = AudioResampler<f64>;

impl<T: rubato::Sample + Float + Default + Send + Sync + 'static> AudioResampler<T> {
    /// Create a new resampler with the given quality profile and sample rates.
    pub fn new(
        quality: ResamplerQuality,
        source_rate: f32,
        output_rate: f32,
    ) -> Result<Self, ResamplerError> {
        let src = (source_rate.round() as usize).max(1);
        let out = (output_rate.round() as usize).max(1);
        if source_rate <= 0.0 || output_rate <= 0.0 {
            return Err(ResamplerError::InvalidRates {
                source_rate: src,
                output_rate: out,
            });
        }
        let inner = Self::create_resampler(quality, src, out)?;
        let rebuilt_quality = inner.quality();
        let mut resampler = Self {
            inner,
            source_rate: src,
            output_rate: out,
            requested_quality: quality,
            speed: 1.0,
            input_buffers: [Vec::new(), Vec::new()],
            input_pos: 0,
            output_buffers: [Vec::new(), Vec::new()],
            output_read_pos: 0,
            output_available: 0,
            needs_rebuild: false,
            pending_quality: None,
            rebuild_failures: 0,
            disabled: false,
            rebuild_rx: None,
            crossfade_buffer: [(T::zero(), T::zero()); 64],
            crossfade_pos: 0,
            crossfade_remaining: 0,
            crossfade_blend_total: 1,
            rebuilt_effective_source: src,
            rebuilt_output_rate: out,
            rebuilt_quality,
            scratch: [Vec::new(), Vec::new()],
        };
        resampler.allocate_buffers();
        Ok(resampler)
    }

    /// Create the appropriate rubato resampler for the quality profile.
    /// Falls back to Fast if HighQuality or Balanced fails.
    fn create_resampler(
        quality: ResamplerQuality,
        source_rate: usize,
        output_rate: usize,
    ) -> Result<ResamplerInner<T>, ResamplerError> {
        Self::create_resampler_exact(quality, source_rate, output_rate).or_else(|orig_err| {
            if quality != ResamplerQuality::Fast {
                log::warn!(
                    "Resampler creation failed for {:?} ({} -> {} Hz), falling back to Fast: {}",
                    quality,
                    source_rate,
                    output_rate,
                    orig_err
                );
                Self::create_resampler_exact(ResamplerQuality::Fast, source_rate, output_rate)
            } else {
                Err(orig_err)
            }
        })
    }

    /// Exact constructor without fallback.
    fn create_resampler_exact(
        quality: ResamplerQuality,
        source_rate: usize,
        output_rate: usize,
    ) -> Result<ResamplerInner<T>, ResamplerError> {
        match quality {
            // rubato 5.0's `Fft::new_custom(fs_in, fs_out, chunk_size,
            // sub_chunks, channels, window, fixed)`: the anti-aliasing filter
            // length derives from `chunk_size / sub_chunks` (rounded up to
            // the rational ratio grid: for 44.1↔48 kHz the per-tier filters
            // are ≈640 (Fast), ≈588 (Balanced), ≈1029 (HighQuality) and
            // ≈2058 (Ultra) taps). Each tier is therefore a genuinely longer
            // filter with a deeper stopband — verified by
            // `tests/fidelity/resampler_measurement.rs`.
            ResamplerQuality::HighQuality => {
                Fft::new_custom(
                    source_rate,
                    output_rate,
                    CHUNK_SIZE * 2,
                    2,
                    CHANNELS,
                    WindowFunction::BlackmanHarris2,
                    FixedSync::Input,
                )
                .map(ResamplerInner::HighQuality)
                .map_err(|e| ResamplerError::CreationFailed {
                    quality,
                    reason: e.to_string(),
                })
            }
            // Single sub-chunk: the filter derives from the whole 2048-frame
            // chunk (~2× longer than HighQuality), giving the deepest
            // stopband in this engine.
            ResamplerQuality::Ultra => {
                Fft::new_custom(
                    source_rate,
                    output_rate,
                    CHUNK_SIZE * 2,
                    1,
                    CHANNELS,
                    WindowFunction::BlackmanHarris2,
                    FixedSync::Input,
                )
                .map(ResamplerInner::Ultra)
                .map_err(|e| ResamplerError::CreationFailed {
                    quality,
                    reason: e.to_string(),
                })
            }
            ResamplerQuality::Balanced => {
                Fft::new_custom(
                    source_rate,
                    output_rate,
                    CHUNK_SIZE,
                    2,
                    CHANNELS,
                    WindowFunction::BlackmanHarris2,
                    FixedSync::Input,
                )
                .map(ResamplerInner::Balanced)
                .map_err(|e| ResamplerError::CreationFailed {
                    quality,
                    reason: e.to_string(),
                })
            }
            ResamplerQuality::Fast => {
                // FixedSync::Both has no sub-chunks: its filter derives from
                // the whole chunk. A small chunk (256 frames) keeps the
                // filter shortest (~294-320 taps), so Fast is genuinely the
                // cheapest tier with the most aliasing — strictly below
                // Balanced (~588-640) in filter length.
                Fft::new(source_rate, output_rate, 256, CHANNELS, FixedSync::Both)
                    .map(ResamplerInner::Fast)
                    .map_err(|e| ResamplerError::CreationFailed {
                        quality,
                        reason: e.to_string(),
                    })
            }
        }
    }

    /// Pre-allocate all internal buffers.
    ///
    /// The scratch buffers are sized to handle the worst-case upsample ratio
    /// (currently `MAX_RATIO`×) plus a filter group-delay margin. This ensures
    /// `process_chunk` never overflows regardless of source/output rate combination.
    fn allocate_buffers(&mut self) {
        let input_frames = self.inner.input_frames_next();
        let input_capacity = input_frames.max(CHUNK_SIZE * 4);
        let output_capacity = MAX_OUTPUT_BUFFER_FRAMES;

        for ch in 0..CHANNELS {
            self.input_buffers[ch].resize(input_frames, T::zero());
            self.input_buffers[ch].reserve(input_capacity - input_frames);
            self.output_buffers[ch].resize(output_capacity, T::zero());
        }

        // Compute the scratch size from the actual ratio so that even
        // 44.1 → 768 kHz (17.4×) or 48 → 384 kHz (8×) never overflows.
        // `output_frames_max()` is the authoritative per-chunk output bound,
        // so the realtime guard in `process_chunk` can never trip.
        let ratio = self.output_rate as f64 / self.compute_effective_source_rate() as f64;
        let filter_margin = self.inner.input_frames_next().max(256);
        let scratch_needed = ((input_frames as f64 * ratio).ceil() as usize + filter_margin)
            .max(MAX_OUTPUT_BUFFER_FRAMES)
            .max(self.inner.output_frames_max());

        for ch in 0..CHANNELS {
            self.scratch[ch].clear();
            self.scratch[ch].resize(scratch_needed, T::zero());
        }

        self.input_pos = 0;
        self.output_read_pos = 0;
        self.output_available = 0;
    }

    /// Feed a stereo sample into the resampler
    #[inline]
    pub fn feed(&mut self, left: T, right: T) {
        if !self.disabled
            && self.rebuild_rx.is_none()
            && !self.needs_rebuild
            && (self.rebuilt_effective_source != self.compute_effective_source_rate()
                || self.rebuilt_output_rate != self.output_rate
                || self.rebuilt_quality != self.inner.quality()
                || self.pending_quality.is_some())
        {
            self.needs_rebuild = true;
        }

        if self.disabled || self.is_passthrough() {
            self.push_sample_direct(left, right);
            return;
        }

        if let Some(ref rx) = self.rebuild_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.rebuild_rx = None;
                    self.apply_rebuild_result(result);
                }
                Err(crossbeam::channel::TryRecvError::Empty) => {}
                Err(crossbeam::channel::TryRecvError::Disconnected) => {
                    log::error!("Resampler builder thread disconnected unexpectedly.");
                    self.rebuild_rx = None;
                    self.needs_rebuild = true;
                    self.rebuild_failures += 1;
                }
            }
        } else if self.needs_rebuild {
            if self.rebuild_failures >= MAX_REBUILD_FAILURES {
                self.needs_rebuild = false;
                self.disabled = true;
                log::error!(
                    "Resampler disabled after {} consecutive rebuild failures.",
                    MAX_REBUILD_FAILURES
                );
                self.push_sample_direct(left, right);
                return;
            } else {
                self.trigger_rebuild();
            }
        }

        if self.input_pos >= self.input_buffers[0].len() {
            self.process_chunk();
            if self.input_pos >= self.input_buffers[0].len() {
                return;
            }
        }

        self.input_buffers[0][self.input_pos] = left;
        self.input_buffers[1][self.input_pos] = right;
        self.input_pos += 1;

        let needed = self.inner.input_frames_next();
        if self.input_pos >= needed {
            self.process_chunk();
        }
    }

    /// Write a single stereo sample directly into the output buffers with
    /// no heap allocation. Used by the disabled-resampler bypass path.
    #[inline]
    fn push_sample_direct(&mut self, left: T, right: T) {
        let cap = MAX_OUTPUT_BUFFER_FRAMES;
        // When the consumer has drained the queue completely, the read cursor
        // may sit at the old end of the backing storage. Rewind it before the
        // next direct write; otherwise passthrough silently drops every sample
        // after one full output-buffer span.
        if self.output_available == 0 {
            self.output_read_pos = 0;
        }
        if self.output_buffers[0].len() < cap || self.output_buffers[1].len() < cap {
            log::error!(
                "Resampler output buffers are under-allocated; dropping passthrough sample"
            );
            return;
        }
        let write_start = self.output_read_pos + self.output_available;
        if write_start < cap {
            self.output_buffers[0][write_start] = left;
            self.output_buffers[1][write_start] = right;
            self.output_available += 1;
        } else {
            if self.output_available > 0 && self.output_read_pos > 0 {
                self.output_buffers[0].copy_within(
                    self.output_read_pos..self.output_read_pos + self.output_available,
                    0,
                );
                self.output_buffers[1].copy_within(
                    self.output_read_pos..self.output_read_pos + self.output_available,
                    0,
                );
                self.output_read_pos = 0;
            }
            let new_write_start = self.output_read_pos + self.output_available;
            if new_write_start < cap {
                self.output_buffers[0][new_write_start] = left;
                self.output_buffers[1][new_write_start] = right;
                self.output_available += 1;
            }
        }
    }

    /// Process a full chunk of input samples through rubato.
    ///
    /// Uses pre-allocated planar `scratch` Vecs (sized at construction time
    /// for the worst-case ratio and `output_frames_max`) to avoid any heap
    /// allocation on this hot path. The scratch buffers are sized in
    /// `allocate_buffers()` off the audio path; if they are somehow smaller
    /// than this chunk's authoritative `output_frames_next()` requirement,
    /// the chunk is bypassed (with an error log) rather than truncated, so a
    /// buffer invariant violation is never silently converted into a partial
    /// resampling operation.
    fn process_chunk(&mut self) {
        let needed = self.inner.input_frames_next();
        if self.input_pos < needed {
            for ch in 0..CHANNELS {
                self.input_buffers[ch][self.input_pos..needed].fill(T::zero());
            }
            self.input_pos = needed;
        }

        if self.output_available > 0 && self.output_read_pos > 0 {
            let rpos = self.output_read_pos;
            let avail = self.output_available;
            let safe_avail = avail.min(self.output_buffers[0].len().saturating_sub(rpos));
            for ch in 0..CHANNELS {
                self.output_buffers[ch].copy_within(rpos..rpos + safe_avail, 0);
            }
            self.output_read_pos = 0;
        } else if self.output_available == 0 {
            self.output_read_pos = 0;
        }

        let write_start = self.output_available;
        let capacity = MAX_OUTPUT_BUFFER_FRAMES;
        let space_available = capacity.saturating_sub(write_start);

        for ch in 0..CHANNELS {
            if self.output_buffers[ch].len() < capacity {
                log::error!("Resampler output buffer is under-allocated; skipping chunk");
                self.input_pos = 0;
                return;
            }
        }

        if space_available == 0 {
            log::warn!("Resampler output buffer full before rubato call; skipping chunk");
            self.input_pos = 0;
            return;
        }

        // The exact number of output frames rubato will write for this chunk.
        let out_frames = self.inner.output_frames_next();

        // STRICT zero-allocation invariant: the scratch buffers are sized in
        // `allocate_buffers()` (which runs on every rebuild, OFF the audio
        // path) and must never be resized here. `allocate_buffers` sizes
        // them from `output_frames_max()`, so `out_frames <= scratch.len()`
        // always holds; if it does not, the buffer invariant is broken and
        // the chunk is bypassed (never truncated, never partially written).
        if self.scratch[0].len() < out_frames || self.scratch[1].len() < out_frames {
            log::error!(
                "Resampler scratch under-allocated (need {out_frames}, have {}); \
                 bypassing chunk",
                self.scratch[0].len()
            );
            self.input_pos = 0;
            return;
        }

        let in_adapter = SequentialSliceOfVecs::new(&self.input_buffers[..], CHANNELS, needed)
            .expect("input buffers are sized in allocate_buffers");
        let mut out_adapter =
            SequentialSliceOfVecs::new_mut(&mut self.scratch[..], CHANNELS, out_frames)
                .expect("scratch buffers are sized in allocate_buffers");

        let result = self.inner.process_into_buffer(&in_adapter, &mut out_adapter);

        match result {
            Ok((_in_consumed, produced)) => {
                let frames_to_add = produced.min(space_available);
                if frames_to_add > 0 {
                    self.output_buffers[0][write_start..write_start + frames_to_add]
                        .copy_from_slice(&self.scratch[0][..frames_to_add]);
                    self.output_buffers[1][write_start..write_start + frames_to_add]
                        .copy_from_slice(&self.scratch[1][..frames_to_add]);
                    self.output_available += frames_to_add;
                }
            }

            Err(e) => {
                log::warn!("Resampler process error: {}", e);
            }
        }

        self.input_pos = 0;
    }

    /// Read a resampled stereo sample. Returns None if no output is available.
    #[inline]
    pub fn read(&mut self) -> Option<(T, T)> {
        if self.crossfade_remaining > 0 {
            let (new_l, new_r) = if self.output_available > 0 {
                let l = self.output_buffers[0][self.output_read_pos];
                let r = self.output_buffers[1][self.output_read_pos];
                self.output_read_pos += 1;
                self.output_available -= 1;
                (l, r)
            } else {
                (T::zero(), T::zero())
            };
            let (old_l, old_r) = self.crossfade_buffer[self.crossfade_pos % 64];
            self.crossfade_pos += 1;
            self.crossfade_remaining -= 1;
            let t_f = self.crossfade_remaining as f64 / self.crossfade_blend_total as f64;
            let t = T::from(t_f).unwrap_or(T::zero());
            let one_minus_t = T::one() - t;
            return Some((
                new_l * one_minus_t + old_l * t,
                new_r * one_minus_t + old_r * t,
            ));
        }

        if self.output_available == 0 {
            return None;
        }

        let left = self.output_buffers[0][self.output_read_pos];
        let right = self.output_buffers[1][self.output_read_pos];
        self.output_read_pos += 1;
        self.output_available -= 1;

        Some((left, right))
    }

    /// Number of output samples available for reading
    pub fn available_output(&self) -> usize {
        self.output_available
    }

    /// Set playback speed (0.25 to 4.0)
    pub fn set_speed(&mut self, speed: f32) {
        let new_speed = speed.clamp(0.25, 4.0);
        if (new_speed - self.speed).abs() > 0.001 {
            self.speed = new_speed;
            self.needs_rebuild = true;
        }
    }

    /// Get current playback speed
    pub fn speed(&self) -> f32 {
        self.speed
    }

    /// Set the quality profile (triggers rebuild)
    pub fn set_quality(&mut self, quality: ResamplerQuality) {
        if quality != self.inner.quality() {
            self.requested_quality = quality;
            self.pending_quality = Some(quality);
            self.needs_rebuild = true;
        }
    }

    /// The quality profile the caller requested. If construction of the
    /// requested profile failed, the resampler silently fell back to
    /// [`Self::effective_quality`] — see [`Self::quality_fell_back`].
    pub fn requested_quality(&self) -> ResamplerQuality {
        self.requested_quality
    }

    /// The quality profile actually running. May be lower than
    /// [`Self::requested_quality`] when a rebuild fell back.
    pub fn effective_quality(&self) -> ResamplerQuality {
        self.inner.quality()
    }

    /// True when the running resampler is not the quality the caller asked
    /// for (a silent quality downgrade occurred).
    pub fn quality_fell_back(&self) -> bool {
        self.requested_quality != self.inner.quality()
    }

    /// Set the source sample rate (triggers rebuild)
    pub fn set_source_rate(&mut self, rate: f32) {
        if !rate.is_finite() || rate <= 0.0 {
            log::warn!(
                "AudioResampler::set_source_rate: ignoring invalid rate {}",
                rate
            );
            return;
        }
        let rate_usize = (rate.round() as usize).max(1);
        if rate_usize != self.source_rate {
            self.source_rate = rate_usize;
            self.needs_rebuild = true;
        }
    }

    /// Set the output sample rate (triggers rebuild)
    pub fn set_output_rate(&mut self, rate: f32) {
        if !rate.is_finite() || rate <= 0.0 {
            log::warn!(
                "AudioResampler::set_output_rate: ignoring invalid rate {}",
                rate
            );
            return;
        }
        let rate_usize = (rate.round() as usize).max(1);
        if rate_usize != self.output_rate {
            self.output_rate = rate_usize;
            self.needs_rebuild = true;
        }
    }

    /// Rebuild the resampler with current parameters
    fn trigger_rebuild(&mut self) {
        let effective_source_f32 = self.source_rate as f32 * self.speed;
        let effective_source = (effective_source_f32.round() as usize).max(1);
        let quality = self.pending_quality.unwrap_or_else(|| self.inner.quality());
        let output_rate = self.output_rate;

        let (tx, rx) = crossbeam::channel::bounded(1);
        std::thread::spawn(move || {
            let result = Self::create_resampler(quality, effective_source, output_rate);
            let _ = tx.send(result);
        });

        self.rebuild_rx = Some(rx);
    }

    fn apply_rebuild_result(&mut self, result: Result<ResamplerInner<T>, ResamplerError>) {
        if self.input_pos > 0 {
            self.process_chunk();
        }
        let save_count = self.output_available.min(64);

        self.crossfade_buffer = [(T::zero(), T::zero()); 64];
        for i in 0..save_count {
            let pos = self.output_read_pos + i;
            if pos < self.output_buffers[0].len() {
                let l = self.output_buffers[0]
                    .get(pos)
                    .copied()
                    .unwrap_or(T::zero());
                let r = self.output_buffers[1]
                    .get(pos)
                    .copied()
                    .unwrap_or(T::zero());
                self.crossfade_buffer[i] = (l, r);
            }
        }
        self.crossfade_pos = 0;
        self.crossfade_remaining = save_count;
        self.crossfade_blend_total = save_count.max(1);

        match result {
            Ok(new_inner) => {
                self.inner = new_inner;
                self.allocate_buffers();

                self.pending_quality = None;
                self.needs_rebuild = false;
                self.rebuild_failures = 0;
                self.disabled = false;

                self.rebuilt_effective_source = self.compute_effective_source_rate();
                self.rebuilt_output_rate = self.output_rate;
                self.rebuilt_quality = self.inner.quality();
            }
            Err(e) => {
                self.rebuild_failures += 1;
                log::error!(
                    "Failed to rebuild resampler ({}/{}), will retry on next feed: {}",
                    self.rebuild_failures,
                    MAX_REBUILD_FAILURES,
                    e
                );
            }
        }
    }

    fn compute_effective_source_rate(&self) -> usize {
        let effective = self.source_rate as f32 * self.speed;
        (effective.round() as usize).max(1)
    }

    /// Flush all pending samples through the resampler
    pub fn flush(&mut self) {
        if self.input_pos > 0 {
            self.process_chunk();
        }
    }

    /// Reset all state
    pub fn reset(&mut self) {
        self.input_pos = 0;
        self.output_read_pos = 0;
        self.output_available = 0;
        self.needs_rebuild = false;
        self.rebuild_rx = None;
        self.crossfade_buffer = [(T::zero(), T::zero()); 64];
        self.crossfade_pos = 0;
        self.crossfade_remaining = 0;
        for ch in 0..CHANNELS {
            self.input_buffers[ch].fill(T::zero());
            self.output_buffers[ch].fill(T::zero());
        }
        self.disabled = false;
        self.rebuild_failures = 0;
    }

    /// Check if source and output rates match (passthrough possible)
    pub fn is_passthrough(&self) -> bool {
        self.source_rate == self.output_rate && (self.speed - 1.0).abs() < 0.001
    }

    /// Authoritative filter group delay in output frames, straight from the
    /// underlying rubato resampler (`Resampler::output_delay`). Zero when the
    /// resampler is disabled or in passthrough mode (no conversion is
    /// performed, so no filter delay is introduced).
    pub fn latency_samples(&self) -> usize {
        if self.disabled || self.is_passthrough() {
            0
        } else {
            self.inner.output_delay()
        }
    }

    /// Group delay in milliseconds at the output sample rate.
    pub fn latency_ms(&self) -> f32 {
        self.latency_samples() as f32 / self.output_rate.max(1) as f32 * 1000.0
    }

    pub fn is_disabled(&self) -> bool {
        self.disabled
    }
}

/// Unified resampler container supporting both f32 (Performance) and f64 (Quality) modes.
pub enum GenericResampler {
    F32(AudioResamplerF32),
    F64(AudioResamplerF64),
}

impl GenericResampler {
    #[inline]
    pub fn is_passthrough(&self) -> bool {
        match self {
            Self::F32(r) => r.is_passthrough(),
            Self::F64(r) => r.is_passthrough(),
        }
    }

    #[inline]
    pub fn is_disabled(&self) -> bool {
        match self {
            Self::F32(r) => r.is_disabled(),
            Self::F64(r) => r.is_disabled(),
        }
    }

    #[inline]
    pub fn set_speed(&mut self, speed: f32) {
        match self {
            Self::F32(r) => r.set_speed(speed),
            Self::F64(r) => r.set_speed(speed),
        }
    }

    #[inline]
    pub fn speed(&self) -> f32 {
        match self {
            Self::F32(r) => r.speed(),
            Self::F64(r) => r.speed(),
        }
    }

    #[inline]
    pub fn set_quality(&mut self, quality: ResamplerQuality) {
        match self {
            Self::F32(r) => r.set_quality(quality),
            Self::F64(r) => r.set_quality(quality),
        }
    }

    /// Requested quality profile (the one the caller asked for).
    #[inline]
    pub fn requested_quality(&self) -> ResamplerQuality {
        match self {
            Self::F32(r) => r.requested_quality(),
            Self::F64(r) => r.requested_quality(),
        }
    }

    /// Effective (actually running) quality profile. May be lower than
    /// `requested_quality` if construction fell back.
    #[inline]
    pub fn effective_quality(&self) -> ResamplerQuality {
        match self {
            Self::F32(r) => r.effective_quality(),
            Self::F64(r) => r.effective_quality(),
        }
    }

    /// True when the running resampler is not the quality the caller asked
    /// for (silent downgrade after a construction failure).
    #[inline]
    pub fn quality_fell_back(&self) -> bool {
        match self {
            Self::F32(r) => r.quality_fell_back(),
            Self::F64(r) => r.quality_fell_back(),
        }
    }

    #[inline]
    pub fn set_source_rate(&mut self, rate: f32) {
        match self {
            Self::F32(r) => r.set_source_rate(rate),
            Self::F64(r) => r.set_source_rate(rate),
        }
    }

    #[inline]
    pub fn set_output_rate(&mut self, rate: f32) {
        match self {
            Self::F32(r) => r.set_output_rate(rate),
            Self::F64(r) => r.set_output_rate(rate),
        }
    }

    #[inline]
    pub fn reset(&mut self) {
        match self {
            Self::F32(r) => r.reset(),
            Self::F64(r) => r.reset(),
        }
    }

    #[inline]
    pub fn flush(&mut self) {
        match self {
            Self::F32(r) => r.flush(),
            Self::F64(r) => r.flush(),
        }
    }

    #[inline]
    pub fn available_output(&self) -> usize {
        match self {
            Self::F32(r) => r.available_output(),
            Self::F64(r) => r.available_output(),
        }
    }

    /// Authoritative filter group delay in output frames.
    #[inline]
    pub fn latency_samples(&self) -> usize {
        match self {
            Self::F32(r) => r.latency_samples(),
            Self::F64(r) => r.latency_samples(),
        }
    }

    /// Filter group delay in milliseconds at the output sample rate.
    #[inline]
    pub fn latency_ms(&self) -> f32 {
        match self {
            Self::F32(r) => r.latency_ms(),
            Self::F64(r) => r.latency_ms(),
        }
    }

    #[inline]
    pub fn feed_f32(&mut self, left: f32, right: f32) {
        match self {
            Self::F32(r) => r.feed(left, right),
            Self::F64(r) => r.feed(left as f64, right as f64),
        }
    }

    #[inline]
    pub fn feed_f64(&mut self, left: f64, right: f64) {
        match self {
            Self::F32(r) => r.feed(left as f32, right as f32),
            Self::F64(r) => r.feed(left, right),
        }
    }

    #[inline]
    pub fn read(&mut self) -> Option<(f32, f32)> {
        self.read_f32()
    }

    #[inline]
    pub fn read_f32(&mut self) -> Option<(f32, f32)> {
        match self {
            Self::F32(r) => r.read(),
            Self::F64(r) => r.read().map(|(l, r)| (l as f32, r as f32)),
        }
    }

    #[inline]
    pub fn read_f64(&mut self) -> Option<(f64, f64)> {
        match self {
            Self::F32(r) => r.read().map(|(l, r)| (l as f64, r as f64)),
            Self::F64(r) => r.read(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resampler_creation() {
        let resampler =
            AudioResampler::<f32>::new(ResamplerQuality::Balanced, 44100.0, 48000.0).unwrap();
        assert!(!resampler.is_passthrough());
    }

    #[test]
    fn test_resampler_f64_creation() {
        let mut resampler =
            AudioResampler::<f64>::new(ResamplerQuality::Balanced, 44100.0, 48000.0).unwrap();
        assert!(!resampler.is_passthrough());
        for i in 0..5000 {
            let sample = (i as f64 / 44100.0 * 440.0 * 2.0 * std::f64::consts::PI).sin() * 0.5;
            resampler.feed(sample, sample);
        }
        resampler.flush();
        assert!(resampler.available_output() > 0);
        let (l, r) = resampler.read().unwrap();
        assert!(l.abs() <= 1.0 && r.abs() <= 1.0);
    }

    #[test]
    fn test_passthrough_detection() {
        let resampler =
            AudioResampler::<f32>::new(ResamplerQuality::Balanced, 44100.0, 44100.0).unwrap();
        assert!(resampler.is_passthrough());
    }

    #[test]
    fn test_latency_reports_authoritative_group_delay() {
        // A real conversion must report rubato's nonzero filter group delay.
        let resampler =
            AudioResampler::<f32>::new(ResamplerQuality::Balanced, 44100.0, 48000.0).unwrap();
        assert!(
            resampler.latency_samples() > 0,
            "44.1->48 kHz must introduce filter delay"
        );

        // The ms value must be the frame count scaled at the OUTPUT rate.
        let expected_ms = resampler.latency_samples() as f32 / 48000.0 * 1000.0;
        assert!((resampler.latency_ms() - expected_ms).abs() < 1e-3);

        // f64 and f32 report the same group delay for the same conversion.
        let f64_resampler =
            AudioResampler::<f64>::new(ResamplerQuality::Balanced, 44100.0, 48000.0).unwrap();
        assert_eq!(resampler.latency_samples(), f64_resampler.latency_samples());
    }

    #[test]
    fn test_passthrough_latency_is_zero() {
        let resampler =
            AudioResampler::<f32>::new(ResamplerQuality::Fast, 44100.0, 44100.0).unwrap();
        assert!(resampler.is_passthrough());
        assert_eq!(resampler.latency_samples(), 0);
        assert_eq!(resampler.latency_ms(), 0.0);
    }

    #[test]
    fn test_resampler_speed_change() {
        let mut resampler =
            AudioResampler::<f32>::new(ResamplerQuality::Fast, 44100.0, 44100.0).unwrap();
        resampler.set_speed(1.5);
        assert!((resampler.speed() - 1.5).abs() < 0.001);
        assert!(resampler.needs_rebuild);
    }

    #[test]
    fn test_resampler_produces_output() {
        let mut resampler =
            AudioResampler::<f32>::new(ResamplerQuality::Fast, 44100.0, 48000.0).unwrap();
        for i in 0..5000 {
            let sample = (i as f32 / 44100.0 * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.5;
            resampler.feed(sample, sample);
        }
        resampler.flush();
        assert!(
            resampler.available_output() > 0,
            "Resampler should produce output after feeding samples"
        );
    }

    #[test]
    fn test_resampler_quality_change() {
        let mut resampler =
            AudioResampler::<f32>::new(ResamplerQuality::Fast, 44100.0, 48000.0).unwrap();
        resampler.set_quality(ResamplerQuality::HighQuality);
        assert!(resampler.needs_rebuild);
    }

    #[test]
    fn test_resampler_reset() {
        let mut resampler =
            AudioResampler::<f32>::new(ResamplerQuality::Fast, 44100.0, 48000.0).unwrap();
        for _ in 0..1000 {
            resampler.feed(0.5f32, 0.5f32);
        }
        resampler.reset();
        assert_eq!(resampler.available_output(), 0);
        assert_eq!(resampler.input_pos, 0);
    }

    #[test]
    fn test_resampler_invalid_rates() {
        let result = AudioResampler::<f32>::new(ResamplerQuality::Fast, 0.0, 48000.0);
        assert!(result.is_err());
        let result = AudioResampler::<f32>::new(ResamplerQuality::Fast, 44100.0, 0.0);
        assert!(result.is_err());
    }

    #[test]
    fn test_resampler_speed_2x_not_inverted() {
        let mut resampler =
            AudioResampler::<f32>::new(ResamplerQuality::Fast, 44100.0, 44100.0).unwrap();
        resampler.set_speed(2.0);
        while resampler.needs_rebuild || resampler.rebuild_rx.is_some() {
            resampler.feed(0.0f32, 0.0f32);
            if resampler.rebuild_rx.is_some() {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        while resampler.read().is_some() {}

        let n_input: usize = 8192;
        for i in 0..n_input {
            let s = (i as f32 / 44100.0 * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.5;
            resampler.feed(s, s);
        }
        resampler.flush();

        let mut n_output: usize = 0;
        while resampler.read().is_some() {
            n_output += 1;
        }
        let ratio = n_output as f32 / n_input as f32;
        assert!(
            ratio <= 1.25,
            "F#02 regression: speed=2.0 with {} input frames produced {} output (ratio {:.3}). \
             Correct ratio is ~0.5; inverted ratio is ~2.0. Got ratio > 1.25 → formula is inverted again.",
            n_input,
            n_output,
            ratio,
        );
    }

    #[test]
    fn test_resampler_speed_half_not_inverted() {
        let mut resampler =
            AudioResampler::<f32>::new(ResamplerQuality::Fast, 44100.0, 44100.0).unwrap();
        resampler.set_speed(0.5);
        while resampler.needs_rebuild || resampler.rebuild_rx.is_some() {
            resampler.feed(0.0f32, 0.0f32);
            if resampler.rebuild_rx.is_some() {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        while resampler.read().is_some() {}

        let n_input: usize = 4096;
        for i in 0..n_input {
            let s = (i as f32 / 44100.0 * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.5;
            resampler.feed(s, s);
        }
        resampler.flush();

        let mut n_output: usize = 0;
        while resampler.read().is_some() {
            n_output += 1;
        }
        let ratio = n_output as f32 / n_input as f32;
        assert!(
            ratio >= 1.25,
            "F#02 regression: speed=0.5 with {} input frames produced {} output (ratio {:.3}). \
             Correct ratio is ~2.0; inverted ratio is ~0.5. Got ratio < 1.25 → formula is inverted again.",
            n_input,
            n_output,
            ratio,
        );
    }
}
