//! Phase 7 S5 — the correction node: a per-channel bank of partitioned
//! [`ConvolutionEngine`]s running as an `AllChannels`-scoped plan step placed
//! **post-aux / pre-EQ** (`mix → aux → correction → eq → …`), so user EQ
//! stacks on the corrected response and the node's declared latency flows
//! into the graph's latency metadata (`position_secs_compensated`).
//!
//! The node only ever processes **pre-rendered** correction IRs
//! ([`CorrectionIrSet`], produced by the S4 derive chain on the control
//! thread). Nothing in the S1–S4 measurement machinery is on the hot path:
//! the IR set is loaded once (control path — allocation is fine there) and
//! the engines then run the same allocation-free partitioned-FFT contract as
//! the existing convolution node.
//!
//! Realtime contract: disabled, depth 0, or no IR loaded = the process step
//! returns without touching a sample — bit-exact. One `ConvolutionEngine`
//! per channel (each loaded with a mono IR — the channel's own correction
//! IR), so a multichannel master corrects every plane the set covers and
//! passes the rest through untouched.

use crate::buffer::MAX_AUDIO_BLOCK_FRAMES;
use crate::dsp::{
    convolution::ConvolutionEngine,
    correction::{
        derive_correction_ir, derive_params_from_config, CorrectionIrSet, IrConditioner, PhaseMode,
    },
    graph::node::DspNode,
    pipeline::{DspStageCapability, StageChannelSupport, StagePrecision},
};

/// SNR assumed for config-driven derivation (`ir_paths` at config-apply
/// time). A live measurement reports its own `snr_db`; a config file has no
/// measurement, so boosts are trusted up to the clamp.
const CONFIG_DRIVEN_SNR_DB: f64 = 60.0;

/// Derivation IR render length for config-driven IRs: the conditioned
/// measurement rounded up to a power of two, clamped to a sane correction
/// budget (128 ms – 682 ms @ 48 kHz).
const MIN_IR_LEN: usize = 1024;
const MAX_IR_LEN: usize = 32_768;

/// Telemetry snapshot of the correction node, published into
/// `PlaybackInfo.correction` on the engine's telemetry cadence.
#[derive(Debug, Clone, PartialEq)]
pub struct CorrectionNodeInfo {
    /// Enabled (live toggle / config).
    pub enabled: bool,
    /// Wet/dry depth in [0, 1].
    pub depth: f32,
    /// Phase mode the loaded IR set was rendered in (`None` = no IR).
    pub phase_mode: Option<PhaseMode>,
    /// Length of the loaded IRs (samples); 0 = none.
    pub ir_len_samples: usize,
    /// Declared correction latency (ms) — the phase mode's group delay,
    /// 0 when inactive.
    pub latency_ms: f32,
    /// Peak gain the loaded set applies (dB), from its per-channel max.
    pub max_gain_db: f32,
}

/// The correction stage: per-channel partitioned convolution bank.
pub struct CorrectionNode {
    /// One engine per channel of the loaded IR set. Each holds a mono IR
    /// (the channel's own correction IR), so the node corrects every plane
    /// the set covers.
    engines: Vec<ConvolutionEngine>,
    /// Enabled (live toggle / config). Disabled = the step is skipped,
    /// bit-exact.
    enabled: bool,
    /// Wet/dry depth in [0, 1] (1.0 = fully corrected).
    depth: f32,
    /// Declared group delay of the loaded set (samples) — the phase mode's
    /// latency. 0 when inactive.
    delay_samples: usize,
    /// Length of the loaded IRs (samples).
    ir_len_samples: usize,
    /// Phase mode of the loaded set.
    phase_mode: Option<PhaseMode>,
    /// Peak gain of the loaded set (dB).
    max_gain_db: f32,
    /// Discard plane for the mono per-channel convolution (the engine writes
    /// both L and R; R is thrown away).
    scratch: Vec<f32>,
    scratch_f64: Vec<f64>,
    sample_rate: f32,
}

impl CorrectionNode {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            engines: Vec::new(),
            enabled: false,
            depth: 1.0,
            delay_samples: 0,
            ir_len_samples: 0,
            phase_mode: None,
            max_gain_db: 0.0,
            scratch: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            scratch_f64: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            sample_rate,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn depth(&self) -> f32 {
        self.depth
    }

    /// Whether the node will process this block (enabled + an IR set
    /// loaded + no stale-rate reload pending).
    pub fn processing(&self) -> bool {
        self.enabled
            && !self.engines.is_empty()
            && self
                .engines
                .iter()
                .all(|e| e.is_ir_loaded() && !e.ir_needs_reload())
    }

    /// Apply the derivation config (Phase 7 S5): enabled / depth plus the
    /// optional measured IR paths, which run the full S2→S4 chain
    /// (condition → smooth → SNR-weighted regularized inverse → phase
    /// render) on the control path. A missing/unreadable IR leaves the node
    /// inactive — bit-exact passthrough.
    pub fn apply_config(&mut self, cfg: &config::CorrectionConfig, sample_rate: f32) {
        self.sample_rate = sample_rate;
        self.depth = cfg.depth.clamp(0.0, 1.0);
        self.enabled = cfg.enabled;
        if cfg.ir_paths.is_empty() {
            self.clear_ir();
            return;
        }
        match derive_from_config(cfg, sample_rate as f64) {
            Ok(set) => {
                if let Err(e) = self.load_set(&set, sample_rate) {
                    log::warn!("correction: failed to load derived IRs: {e}");
                    self.clear_ir();
                }
            }
            Err(e) => {
                log::warn!(
                    "correction: failed to derive from configured IRs {}: {e}",
                    cfg.ir_paths.join(", ")
                );
                self.clear_ir();
            }
        }
    }

    /// Live runtime toggle (control-queue drain): enabled + depth only — the
    /// loaded IR stays exactly as it was.
    pub fn set_runtime(&mut self, enabled: bool, depth: f32) {
        self.enabled = enabled;
        self.depth = depth.clamp(0.0, 1.0);
        for engine in &mut self.engines {
            engine.set_wet_mix(self.depth);
        }
    }

    /// Load a pre-rendered correction IR set (the `LoadCorrectionIr` /
    /// `MeasureRoom` control-path land, or generation replay). Control path
    /// only — the engines are rebuilt here (allocation is fine).
    pub fn load_set(&mut self, set: &CorrectionIrSet, sample_rate: f32) -> Result<(), String> {
        self.sample_rate = sample_rate;
        if set.channels.is_empty() {
            return Err("correction IR set has no channels".to_string());
        }
        let mut engines = Vec::with_capacity(set.channels.len());
        for ch in &set.channels {
            if ch.len() > MAX_IR_LEN {
                return Err(format!(
                    "correction IR of {} samples exceeds the {} limit",
                    ch.len(),
                    MAX_IR_LEN
                ));
            }
            let mut engine = ConvolutionEngine::new(sample_rate, MAX_IR_LEN.min(ch.len()).max(64));
            engine.set_wet_mix(self.depth);
            let pairs: Vec<(f64, f64)> = ch.iter().map(|&s| (s, s)).collect();
            engine
                .load_ir_from_samples_f64(&pairs)
                .map_err(|e| format!("partitioned-convolution load failed: {e}"))?;
            engine.set_enabled(true);
            engines.push(engine);
        }
        self.engines = engines;
        self.delay_samples = set.delay_samples.round().max(0.0) as usize;
        self.ir_len_samples = set.channels[0].len();
        self.phase_mode = Some(set.phase_mode);
        self.max_gain_db = set
            .channels
            .iter()
            .map(|ch| {
                ch.iter()
                    .map(|s| 20.0 * s.abs().max(1e-12).log10())
                    .fold(f64::NEG_INFINITY, f64::max)
            })
            .fold(f64::NEG_INFINITY, f64::max)
            .max(0.0) as f32;
        log::info!(
            "correction: loaded {} IR(s) of {} samples ({} phase, {} ms latency)",
            self.engines.len(),
            self.ir_len_samples,
            format!("{:?}", set.phase_mode).to_lowercase(),
            self.latency_ms(self.sample_rate)
        );
        Ok(())
    }

    /// Drop the loaded IR set (config with no paths / failed derive).
    fn clear_ir(&mut self) {
        self.engines.clear();
        self.delay_samples = 0;
        self.ir_len_samples = 0;
        self.phase_mode = None;
        self.max_gain_db = 0.0;
    }

    /// Telemetry snapshot.
    pub fn info(&self) -> CorrectionNodeInfo {
        CorrectionNodeInfo {
            enabled: self.enabled,
            depth: self.depth,
            phase_mode: self.phase_mode,
            ir_len_samples: if self.processing() {
                self.ir_len_samples
            } else {
                0
            },
            latency_ms: self.latency_ms(self.sample_rate),
            max_gain_db: if self.processing() {
                self.max_gain_db
            } else {
                0.0
            },
        }
    }
}

/// Run the S2→S4 chain over the configured measured IR paths (control
/// path). One path = a multichannel WAV; several = per-channel files (each
/// file's channel 0 is that channel's measurement).
fn derive_from_config(
    cfg: &config::CorrectionConfig,
    session_rate: f64,
) -> Result<CorrectionIrSet, crate::dsp::correction::CorrectionError> {
    use crate::dsp::correction::read_wav_ir;
    use std::path::Path;

    let (channels, file_rate) = if cfg.ir_paths.len() == 1 {
        let wav = read_wav_ir(Path::new(&cfg.ir_paths[0]))?;
        (wav.channels, wav.sample_rate)
    } else {
        let mut merged = Vec::new();
        let mut rate = 0.0;
        for path in &cfg.ir_paths {
            let wav = read_wav_ir(Path::new(path))?;
            if let Some(ch0) = wav.channels.first() {
                merged.push(ch0.clone());
            }
            rate = wav.sample_rate;
        }
        if merged.is_empty() {
            return Err(crate::dsp::correction::CorrectionError::InvalidConfig {
                what: "IR paths",
                message: "no channels could be read".into(),
            });
        }
        (merged, rate)
    };

    // The S2 conditioner is strict about rate alignment: a file whose rate
    // differs from the session must be resampled before conditioning (the
    // engine's rate machinery owns that; here the mismatch surfaces as a
    // logged warning and the node stays bit-exact).
    let conditioner = IrConditioner::default();
    let measured = conditioner.condition(
        &crate::dsp::correction::WavIr {
            channels,
            sample_rate: file_rate,
        },
        session_rate,
    )?;

    let meas_len = measured.channels[0].len();
    let ir_len = meas_len
        .next_power_of_two()
        .clamp(MIN_IR_LEN, MAX_IR_LEN)
        .max(MIN_IR_LEN);
    let params = derive_params_from_config(cfg, session_rate, ir_len, CONFIG_DRIVEN_SNR_DB);
    derive_correction_ir(&measured, &params)
}

impl DspNode for CorrectionNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "correction",
            channel_support: StageChannelSupport::AllChannels,
            position: "post-aux, pre-EQ",
            stateful: true,
            realtime_safe: true,
            bit_perfect_compatible: false,
            sample_rate_sensitive: true,
            precision: StagePrecision::Any,
        }
    }

    fn is_active(&self) -> bool {
        self.processing()
    }

    fn latency_samples(&self) -> usize {
        if self.processing() {
            self.delay_samples
        } else {
            0
        }
    }

    fn tail_samples(&self) -> usize {
        if self.processing() {
            self.ir_len_samples.saturating_sub(self.delay_samples)
        } else {
            0
        }
    }

    fn reset(&mut self) {
        for engine in &mut self.engines {
            engine.reset();
        }
    }

    fn prepare(&mut self, sample_rate: f32, _max_channels: usize) {
        self.sample_rate = sample_rate;
        for engine in &mut self.engines {
            engine.set_sample_rate(sample_rate);
        }
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        if !self.processing() || planes.is_empty() {
            return;
        }
        let frames = planes[0].len().min(MAX_AUDIO_BLOCK_FRAMES);
        let n = self.engines.len().min(planes.len());
        for (ch, plane) in planes.iter_mut().take(n).enumerate() {
            let engine = &mut self.engines[ch];
            let scratch = &mut self.scratch[..frames];
            engine.process_block(plane, scratch);
        }
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        if !self.processing() || planes.is_empty() {
            return;
        }
        let frames = planes[0].len().min(MAX_AUDIO_BLOCK_FRAMES);
        let n = self.engines.len().min(planes.len());
        for (ch, plane) in planes.iter_mut().take(n).enumerate() {
            let engine = &mut self.engines[ch];
            let scratch = &mut self.scratch_f64[..frames];
            engine.process_block_f64(plane, scratch);
        }
    }
}
