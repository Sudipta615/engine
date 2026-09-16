//! Parameter metadata + the fixed-capacity parameter set that crosses the
//! engine's control queues as plain data.
//!
//! The host declares parameters through [`ParamDescriptor`]s at load time;
//! live values travel as [`PluginParams`] — a fixed-capacity `(index,
//! value)` array the engine enqueues over the SPSC control bus without
//! allocating (mirroring `NodeCmd`).

#[cfg(feature = "serde-types")]
use serde::{Deserialize, Serialize};

/// The normalized value of one parameter (0..=1 is conventional but
/// plugins may declare wider ranges; the value is clamped by the plugin).
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde-types", derive(Serialize, Deserialize))]
pub struct ParamValue {
    pub index: u32,
    pub value: f32,
}

/// A fixed-capacity batch of parameter values, applied atomically at a
/// block boundary. Capacity matches [`MAX_PLUGIN_PARAMS`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PluginParams {
    values: [ParamValue; super::MAX_PLUGIN_PARAMS],
    len: u32,
}

impl PluginParams {
    /// An empty batch.
    pub const fn empty() -> Self {
        Self {
            values: [ParamValue {
                index: 0,
                value: 0.0,
            }; super::MAX_PLUGIN_PARAMS],
            len: 0,
        }
    }

    /// Append one value. Returns `false` (and drops the value) when the
    /// batch is full — matches the engine's bounded, drop-on-overflow
    /// control posture.
    pub fn push(&mut self, index: u32, value: f32) -> bool {
        if (self.len as usize) >= super::MAX_PLUGIN_PARAMS || !value.is_finite() {
            return false;
        }
        self.values[self.len as usize] = ParamValue { index, value };
        self.len += 1;
        true
    }

    /// Append every `(index, value)` pair; silently drops overflow.
    pub fn extend_from_pairs<'a>(&mut self, pairs: impl IntoIterator<Item = &'a (u32, f32)>) {
        for &(i, v) in pairs {
            self.push(i, v);
        }
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Return the populated parameter values as a slice.
    pub fn as_slice(&self) -> &[ParamValue] {
        &self.values[..self.len as usize]
    }

    /// Iterate `(index, value)` in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = ParamValue> + '_ {
        self.values[..self.len as usize].iter().copied()
    }
}

#[cfg(feature = "serde-types")]
impl Serialize for PluginParams {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeSeq;
        let slice = self.as_slice();
        let mut seq = serializer.serialize_seq(Some(slice.len()))?;
        for elem in slice {
            seq.serialize_element(elem)?;
        }
        seq.end()
    }
}

#[cfg(feature = "serde-types")]
impl<'de> Deserialize<'de> for PluginParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct PluginParamsVisitor;

        impl<'de> serde::de::Visitor<'de> for PluginParamsVisitor {
            type Value = PluginParams;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a sequence of ParamValue")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut params = PluginParams::empty();
                while let Some(val) = seq.next_element::<ParamValue>()? {
                    if !params.push(val.index, val.value) {
                        break;
                    }
                }
                Ok(params)
            }
        }

        deserializer.deserialize_seq(PluginParamsVisitor)
    }
}

impl Default for PluginParams {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(feature = "serde-types")]
fn default_true() -> bool {
    true
}

#[cfg(feature = "serde-types")]
fn default_curve() -> String {
    "linear".to_string()
}

#[cfg(feature = "serde-types")]
fn default_smoothing() -> String {
    "none".to_string()
}

/// The shape of one declared parameter (host-side mirror of what the
/// plugin's descriptor reports, adhering to §9.1 unified parameter metadata).
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde-types", derive(Serialize, Deserialize))]
pub struct ParamDescriptor {
    /// Stable parameter index (0..param_count).
    pub index: u32,
    /// Unique machine identifier.
    #[cfg_attr(feature = "serde-types", serde(default))]
    pub id: String,
    /// Human-readable label / name.
    pub label: String,
    /// Engineering unit representation (e.g. "dB", "ms", "Hz", "%").
    #[cfg_attr(feature = "serde-types", serde(default))]
    pub unit: String,
    /// Minimum allowed value.
    pub min: f32,
    /// Maximum allowed value.
    pub max: f32,
    /// Default nominal value.
    pub default: f32,
    /// Discrete quantization step size (`None` for continuous parameters).
    #[cfg_attr(feature = "serde-types", serde(default))]
    pub step: Option<f32>,
    /// Parameter response curve ("linear", "logarithmic", "exponential", "decibel", "s_curve").
    #[cfg_attr(feature = "serde-types", serde(default = "default_curve"))]
    pub curve: String,
    /// Real-time smoothing strategy ("none", "one_pole", "linear_ramp", "slew_rate").
    #[cfg_attr(feature = "serde-types", serde(default = "default_smoothing"))]
    pub smoothing: String,
    /// Whether this parameter can be modulated via host or timeline automation.
    #[cfg_attr(feature = "serde-types", serde(default = "default_true"))]
    pub automatable: bool,
    /// Whether this parameter only accepts integer or stepped discrete values.
    #[cfg_attr(feature = "serde-types", serde(default))]
    pub discrete: bool,
    /// Whether this parameter supports sub-block sample-accurate automation.
    #[cfg_attr(feature = "serde-types", serde(default = "default_true"))]
    pub sample_accurate: bool,
}

/// Convenience helpers for validating and mapping host-side parameter input.
impl ParamDescriptor {
    /// Create a fully specified parameter descriptor.
    pub fn new(
        index: u32,
        id: impl Into<String>,
        label: impl Into<String>,
        min: f32,
        max: f32,
        default: f32,
    ) -> Self {
        let id_str = id.into();
        Self {
            index,
            id: id_str,
            label: label.into(),
            unit: String::new(),
            min,
            max,
            default: default.clamp(min, max),
            step: None,
            curve: "linear".to_string(),
            smoothing: "none".to_string(),
            automatable: true,
            discrete: false,
            sample_accurate: true,
        }
    }

    /// Create a simple parameter descriptor from index and label.
    pub fn simple(index: u32, label: impl Into<String>, min: f32, max: f32, default: f32) -> Self {
        let lbl = label.into();
        Self::new(index, lbl.clone(), lbl, min, max, default)
    }

    /// Access the human-readable name (alias of label).
    pub fn name(&self) -> &str {
        &self.label
    }

    /// Clamp a value into the declared range.
    pub fn clamp(&self, value: f32) -> f32 {
        value.clamp(self.min, self.max)
    }

    /// `true` when `value` is within the declared range.
    pub fn contains(&self, value: f32) -> bool {
        value >= self.min && value <= self.max
    }

    /// Normalize a physical parameter value to `[0.0, 1.0]`.
    pub fn normalize(&self, value: f32) -> f32 {
        let clamped = self.clamp(value);
        let range = self.max - self.min;
        if range.abs() < 1e-12 {
            return 0.0;
        }
        (clamped - self.min) / range
    }

    /// Denormalize a `[0.0, 1.0]` normalized float back to the declared parameter range.
    pub fn denormalize(&self, normalized: f32) -> f32 {
        let norm = normalized.clamp(0.0, 1.0);
        let range = self.max - self.min;
        self.clamp(self.min + norm * range)
    }

    /// Quantize/snap a value to the nearest discrete step if declared.
    pub fn snap_step(&self, value: f32) -> f32 {
        if let Some(step) = self.step {
            if step > 1e-12 {
                let steps = ((value - self.min) / step).round();
                return self.clamp(self.min + steps * step);
            }
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_iterate() {
        let mut p = PluginParams::empty();
        assert!(p.is_empty());
        assert!(p.push(0, 0.5));
        assert!(p.push(1, -3.0));
        assert!(!p.push(0, f32::NAN), "non-finite values are dropped");
        let collected: Vec<_> = p.iter().collect();
        assert_eq!(collected.len(), 2);
        assert_eq!(collected[0].index, 0);
        assert_eq!(collected[1].value, -3.0);
    }

    #[test]
    fn capacity_is_bounded() {
        let mut p = PluginParams::empty();
        for i in 0..super::super::MAX_PLUGIN_PARAMS {
            assert!(p.push(i as u32, i as f32));
        }
        assert!(!p.push(999, 1.0), "the batch is full");
        assert_eq!(p.len(), super::super::MAX_PLUGIN_PARAMS);
    }
}
