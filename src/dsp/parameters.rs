//! Unified parameter metadata system (§9.1, Item 17).
//!
//! Provides a standardized parameter metadata model across DSP algorithms,
//! the graph architecture, plugins, and host automation:
//! - Explicit parameter descriptors ([`ParameterDescriptor`])
//! - Rich units ([`ParameterUnit`]) and display formatting
//! - Taper/warping curves ([`ParameterCurve`]: linear, log, exp, dB, S-curve)
//! - Smoothing specifications ([`ParameterSmoothing`]: OnePole, linear ramp, slew rate)
//! - Bi-directional normalization: `value ⇄ normalized [0.0, 1.0]`
//! - Discrete step snapping and validation
//! - Centralized [`ParameterRegistry`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Stable identifier for a DSP or plugin parameter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ParameterId(pub String);

impl ParameterId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<T: Into<String>> From<T> for ParameterId {
    fn from(val: T) -> Self {
        Self::new(val)
    }
}

impl std::fmt::Display for ParameterId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Physical or engineering unit of a parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParameterUnit {
    #[default]
    Generic,
    Hertz,
    Decibels,
    Percent,
    Milliseconds,
    Seconds,
    LinearGain,
    Semitones,
    Cents,
    Normalized,
    QFactor,
    Samples,
    Bpm,
}

impl ParameterUnit {
    /// Short symbol representation for UI and logs.
    pub fn symbol(self) -> &'static str {
        match self {
            ParameterUnit::Generic => "",
            ParameterUnit::Hertz => "Hz",
            ParameterUnit::Decibels => "dB",
            ParameterUnit::Percent => "%",
            ParameterUnit::Milliseconds => "ms",
            ParameterUnit::Seconds => "s",
            ParameterUnit::LinearGain => "x",
            ParameterUnit::Semitones => "st",
            ParameterUnit::Cents => "ct",
            ParameterUnit::Normalized => "",
            ParameterUnit::QFactor => "Q",
            ParameterUnit::Samples => "smp",
            ParameterUnit::Bpm => "BPM",
        }
    }
}

impl std::fmt::Display for ParameterUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.symbol())
    }
}

/// Parameter response curve / taper mapping between normalized `[0, 1]` and user values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParameterCurve {
    #[default]
    Linear,
    /// Logarithmic scaling (ideal for frequency ranges like 20 Hz .. 20 kHz).
    Logarithmic,
    /// Exponential scaling (curved taper).
    Exponential,
    /// Decibel taper with perceptual fader curvature.
    Decibel,
    /// Smoothstep cubic Hermite ease-in/ease-out ($3t^2 - 2t^3$).
    SCurve,
}

/// Real-time smoothing strategy to prevent clicks and zipper noise.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ParameterSmoothing {
    /// No smoothing; changes step instantly at block or sample boundary.
    #[default]
    None,
    /// Exponential 1-pole low-pass filter with time constant `tau_ms`.
    OnePole { tau_ms: f32 },
    /// Linear ramp over a fixed time duration in milliseconds.
    LinearRamp { duration_ms: f32 },
    /// Slew-rate limiting bounding maximum change per second.
    SlewRateLimit { max_change_per_sec: f32 },
}

/// Complete parameter metadata descriptor (§9.1, Item 17).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParameterDescriptor {
    /// Unique machine identifier.
    pub id: ParameterId,
    /// Human-readable display label.
    pub name: String,
    /// Engineering unit.
    pub unit: ParameterUnit,
    /// Minimum allowed value.
    pub min: f32,
    /// Maximum allowed value.
    pub max: f32,
    /// Default nominal value.
    pub default: f32,
    /// Discrete quantization step size (`None` for continuous parameters).
    pub step: Option<f32>,
    /// Warping / mapping curve.
    pub curve: ParameterCurve,
    /// Real-time parameter smoothing configuration.
    pub smoothing: ParameterSmoothing,
    /// Whether this parameter can be modulated via host or timeline automation.
    pub automatable: bool,
    /// Whether this parameter only accepts integer or stepped discrete values.
    pub discrete: bool,
    /// Whether this parameter supports sub-block sample-accurate automation.
    pub sample_accurate: bool,
}

impl ParameterDescriptor {
    pub fn new(
        id: impl Into<ParameterId>,
        name: impl Into<String>,
        min: f32,
        max: f32,
        default: f32,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            unit: ParameterUnit::Generic,
            min,
            max,
            default: default.clamp(min, max),
            step: None,
            curve: ParameterCurve::Linear,
            smoothing: ParameterSmoothing::None,
            automatable: true,
            discrete: false,
            sample_accurate: true,
        }
    }

    pub fn with_unit(mut self, unit: ParameterUnit) -> Self {
        self.unit = unit;
        self
    }

    pub fn with_curve(mut self, curve: ParameterCurve) -> Self {
        self.curve = curve;
        self
    }

    pub fn with_smoothing(mut self, smoothing: ParameterSmoothing) -> Self {
        self.smoothing = smoothing;
        self
    }

    pub fn with_step(mut self, step: f32) -> Self {
        self.step = Some(step.abs());
        self.discrete = true;
        self
    }

    pub fn with_automatable(mut self, automatable: bool) -> Self {
        self.automatable = automatable;
        self
    }

    pub fn with_sample_accurate(mut self, sample_accurate: bool) -> Self {
        self.sample_accurate = sample_accurate;
        self
    }

    /// Clamp a raw user value within the declared valid range.
    #[inline]
    pub fn clamp(&self, value: f32) -> f32 {
        if !value.is_finite() {
            return self.default;
        }
        value.clamp(self.min, self.max)
    }

    /// Snap value to step grid if configured.
    #[inline]
    pub fn snap_step(&self, value: f32) -> f32 {
        let clamped = self.clamp(value);
        if let Some(step) = self.step {
            if step > 1e-7 {
                let steps = ((clamped - self.min) / step).round();
                return (self.min + steps * step).clamp(self.min, self.max);
            }
        }
        clamped
    }

    /// Normalize a physical value to normalized range `[0.0, 1.0]`.
    pub fn normalize(&self, value: f32) -> f32 {
        let v = self.clamp(value);
        let range = self.max - self.min;
        if range.abs() < 1e-7 {
            return 0.0;
        }

        let linear_t = ((v - self.min) / range).clamp(0.0, 1.0);
        match self.curve {
            ParameterCurve::Linear => linear_t,
            ParameterCurve::Logarithmic => {
                if self.min > 0.0 && self.max > 0.0 {
                    let log_min = self.min.ln();
                    let log_max = self.max.ln();
                    ((v.ln() - log_min) / (log_max - log_min)).clamp(0.0, 1.0)
                } else {
                    linear_t
                }
            }
            ParameterCurve::Exponential => linear_t * linear_t,
            ParameterCurve::Decibel => {
                // dB curve: linear fader taper
                linear_t.powf(2.0)
            }
            ParameterCurve::SCurve => {
                // Invert smoothstep approximately or return linear t
                linear_t
            }
        }
    }

    /// Denormalize a normalized `[0.0, 1.0]` scalar back to the parameter's physical range.
    pub fn denormalize(&self, normalized: f32) -> f32 {
        let t = normalized.clamp(0.0, 1.0);
        let range = self.max - self.min;

        let raw = match self.curve {
            ParameterCurve::Linear => self.min + t * range,
            ParameterCurve::Logarithmic => {
                if self.min > 0.0 && self.max > 0.0 {
                    let log_min = self.min.ln();
                    let log_max = self.max.ln();
                    (log_min + t * (log_max - log_min)).exp()
                } else {
                    self.min + t * range
                }
            }
            ParameterCurve::Exponential => self.min + (t.sqrt()) * range,
            ParameterCurve::Decibel => self.min + (t.sqrt()) * range,
            ParameterCurve::SCurve => {
                let s = 3.0 * t * t - 2.0 * t * t * t;
                self.min + s * range
            }
        };

        self.snap_step(raw)
    }

    /// Format a parameter value into human-readable string with unit symbol.
    pub fn format_value(&self, value: f32) -> String {
        let val = self.snap_step(value);
        let sym = self.unit.symbol();
        if sym.is_empty() {
            format!("{:.2}", val)
        } else {
            format!("{:.2} {}", val, sym)
        }
    }
}

/// Central catalog holding declared parameters across the engine.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ParameterRegistry {
    parameters: BTreeMap<ParameterId, ParameterDescriptor>,
}

impl ParameterRegistry {
    pub fn new() -> Self {
        Self {
            parameters: BTreeMap::new(),
        }
    }

    /// Register a parameter descriptor.
    pub fn register(&mut self, descriptor: ParameterDescriptor) {
        self.parameters.insert(descriptor.id.clone(), descriptor);
    }

    /// Query a parameter descriptor by ID.
    pub fn get(&self, id: &ParameterId) -> Option<&ParameterDescriptor> {
        self.parameters.get(id)
    }

    /// Total registered parameter count.
    pub fn len(&self) -> usize {
        self.parameters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parameters.is_empty()
    }

    /// Iterate over all registered parameter descriptors.
    pub fn iter(&self) -> impl Iterator<Item = (&ParameterId, &ParameterDescriptor)> {
        self.parameters.iter()
    }

    /// Create standard default registry populated with core engine parameters.
    pub fn with_standard_engine_parameters() -> Self {
        let mut reg = Self::new();

        // Master Volume
        reg.register(
            ParameterDescriptor::new("master_volume", "Master Volume", 0.0, 2.0, 1.0)
                .with_unit(ParameterUnit::LinearGain)
                .with_curve(ParameterCurve::Decibel)
                .with_smoothing(ParameterSmoothing::LinearRamp { duration_ms: 10.0 }),
        );

        // Balance
        reg.register(
            ParameterDescriptor::new("master_balance", "Master Balance", -1.0, 1.0, 0.0)
                .with_unit(ParameterUnit::Generic)
                .with_smoothing(ParameterSmoothing::LinearRamp { duration_ms: 10.0 }),
        );

        // Parametric EQ Band Frequency
        reg.register(
            ParameterDescriptor::new("eq_band_freq", "Band Frequency", 20.0, 20000.0, 1000.0)
                .with_unit(ParameterUnit::Hertz)
                .with_curve(ParameterCurve::Logarithmic)
                .with_smoothing(ParameterSmoothing::OnePole { tau_ms: 20.0 }),
        );

        // Parametric EQ Band Gain
        reg.register(
            ParameterDescriptor::new("eq_band_gain", "Band Gain", -24.0, 24.0, 0.0)
                .with_unit(ParameterUnit::Decibels)
                .with_curve(ParameterCurve::Linear)
                .with_smoothing(ParameterSmoothing::LinearRamp { duration_ms: 15.0 }),
        );

        // Parametric EQ Band Q
        reg.register(
            ParameterDescriptor::new("eq_band_q", "Band Q", 0.1, 18.0, 1.0)
                .with_unit(ParameterUnit::QFactor)
                .with_curve(ParameterCurve::Logarithmic),
        );

        // Dynamics / Compressor Threshold
        reg.register(
            ParameterDescriptor::new("compressor_threshold", "Threshold", -60.0, 0.0, -18.0)
                .with_unit(ParameterUnit::Decibels)
                .with_smoothing(ParameterSmoothing::LinearRamp { duration_ms: 10.0 }),
        );

        // Dynamics / Compressor Ratio
        reg.register(
            ParameterDescriptor::new("compressor_ratio", "Ratio", 1.0, 30.0, 4.0)
                .with_unit(ParameterUnit::Generic)
                .with_curve(ParameterCurve::Logarithmic),
        );

        // Limiter Ceiling
        reg.register(
            ParameterDescriptor::new("limiter_ceiling", "Ceiling", -12.0, 0.0, -0.1)
                .with_unit(ParameterUnit::Decibels),
        );

        reg
    }
}

impl From<&plugin_abi::ParamDescriptor> for ParameterDescriptor {
    fn from(desc: &plugin_abi::ParamDescriptor) -> Self {
        let curve = match desc.curve.as_str() {
            "logarithmic" => ParameterCurve::Logarithmic,
            "exponential" => ParameterCurve::Exponential,
            "decibel" => ParameterCurve::Decibel,
            "s_curve" => ParameterCurve::SCurve,
            _ => ParameterCurve::Linear,
        };
        let smoothing = match desc.smoothing.as_str() {
            "one_pole" => ParameterSmoothing::OnePole { tau_ms: 10.0 },
            "linear_ramp" => ParameterSmoothing::LinearRamp { duration_ms: 10.0 },
            _ => ParameterSmoothing::None,
        };
        let unit = match desc.unit.as_str() {
            "Hz" => ParameterUnit::Hertz,
            "dB" => ParameterUnit::Decibels,
            "%" => ParameterUnit::Percent,
            "ms" => ParameterUnit::Milliseconds,
            "s" => ParameterUnit::Seconds,
            "x" => ParameterUnit::LinearGain,
            "st" => ParameterUnit::Semitones,
            "ct" => ParameterUnit::Cents,
            "Q" => ParameterUnit::QFactor,
            "smp" => ParameterUnit::Samples,
            "Bpm" | "BPM" => ParameterUnit::Bpm,
            _ => ParameterUnit::Generic,
        };
        let mut p = ParameterDescriptor::new(
            desc.id.clone(),
            desc.label.clone(),
            desc.min,
            desc.max,
            desc.default,
        )
        .with_unit(unit)
        .with_curve(curve)
        .with_smoothing(smoothing)
        .with_automatable(desc.automatable)
        .with_sample_accurate(desc.sample_accurate);
        if let Some(step) = desc.step {
            p = p.with_step(step);
        }
        p.discrete = desc.discrete;
        p
    }
}

impl From<&ParameterDescriptor> for plugin_abi::ParamDescriptor {
    fn from(desc: &ParameterDescriptor) -> Self {
        let curve = match desc.curve {
            ParameterCurve::Linear => "linear",
            ParameterCurve::Logarithmic => "logarithmic",
            ParameterCurve::Exponential => "exponential",
            ParameterCurve::Decibel => "decibel",
            ParameterCurve::SCurve => "s_curve",
        };
        let smoothing = match desc.smoothing {
            ParameterSmoothing::None => "none",
            ParameterSmoothing::OnePole { .. } => "one_pole",
            ParameterSmoothing::LinearRamp { .. } => "linear_ramp",
            ParameterSmoothing::SlewRateLimit { .. } => "slew_rate",
        };
        Self {
            index: 0,
            id: desc.id.0.clone(),
            label: desc.name.clone(),
            unit: desc.unit.symbol().to_string(),
            min: desc.min,
            max: desc.max,
            default: desc.default,
            step: desc.step,
            curve: curve.to_string(),
            smoothing: smoothing.to_string(),
            automatable: desc.automatable,
            discrete: desc.discrete,
            sample_accurate: desc.sample_accurate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_parameter_normalization_round_trip() {
        let desc = ParameterDescriptor::new("gain", "Gain", -10.0, 10.0, 0.0)
            .with_unit(ParameterUnit::Decibels);

        assert_eq!(desc.normalize(-10.0), 0.0);
        assert_eq!(desc.normalize(0.0), 0.5);
        assert_eq!(desc.normalize(10.0), 1.0);

        assert!((desc.denormalize(0.0) - (-10.0)).abs() < 1e-5);
        assert!((desc.denormalize(0.5) - 0.0).abs() < 1e-5);
        assert!((desc.denormalize(1.0) - 10.0).abs() < 1e-5);
    }

    #[test]
    fn logarithmic_frequency_normalization() {
        let desc = ParameterDescriptor::new("cutoff", "Cutoff", 20.0, 20000.0, 1000.0)
            .with_unit(ParameterUnit::Hertz)
            .with_curve(ParameterCurve::Logarithmic);

        let norm_1k = desc.normalize(1000.0);
        assert!(norm_1k > 0.4 && norm_1k < 0.7);

        let back_1k = desc.denormalize(norm_1k);
        assert!((back_1k - 1000.0).abs() < 1.0);
    }

    #[test]
    fn discrete_stepping_and_clamping() {
        let desc = ParameterDescriptor::new("coarse_tune", "Tune", -12.0, 12.0, 0.0)
            .with_unit(ParameterUnit::Semitones)
            .with_step(1.0);

        assert_eq!(desc.snap_step(3.4), 3.0);
        assert_eq!(desc.snap_step(3.7), 4.0);
        assert_eq!(desc.snap_step(15.0), 12.0);
        assert_eq!(desc.format_value(5.0), "5.00 st");
    }

    #[test]
    fn registry_lookup_and_serialization() {
        let reg = ParameterRegistry::with_standard_engine_parameters();
        assert!(reg.len() >= 8);

        let vol = reg.get(&ParameterId::new("master_volume")).unwrap();
        assert_eq!(vol.unit, ParameterUnit::LinearGain);
        assert_eq!(vol.default, 1.0);

        let json = serde_json::to_string(&reg).unwrap();
        let back: ParameterRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), reg.len());
    }

    #[test]
    fn test_plugin_param_descriptor_roundtrip() {
        let orig = ParameterDescriptor::new("wet_mix", "Wet Mix", 0.0, 1.0, 0.5)
            .with_unit(ParameterUnit::Percent)
            .with_curve(ParameterCurve::Linear)
            .with_step(0.01)
            .with_automatable(true)
            .with_sample_accurate(true);

        let abi_desc: plugin_abi::ParamDescriptor = (&orig).into();
        assert_eq!(abi_desc.id, "wet_mix");
        assert_eq!(abi_desc.label, "Wet Mix");
        assert_eq!(abi_desc.unit, "%");
        assert_eq!(abi_desc.step, Some(0.01));

        let back: ParameterDescriptor = (&abi_desc).into();
        assert_eq!(back.id.0, orig.id.0);
        assert_eq!(back.name, orig.name);
        assert_eq!(back.unit, orig.unit);
        assert_eq!(back.step, orig.step);
        assert_eq!(back.min, orig.min);
        assert_eq!(back.max, orig.max);
    }
}
