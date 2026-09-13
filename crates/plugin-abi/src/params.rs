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
pub struct ParamValue {
    pub index: u32,
    pub value: f32,
}

/// A fixed-capacity batch of parameter values, applied atomically at a
/// block boundary. Capacity matches [`MAX_PLUGIN_PARAMS`].
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde-types", derive(Serialize, Deserialize))]
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

    /// Iterate `(index, value)` in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = ParamValue> + '_ {
        self.values[..self.len as usize].iter().copied()
    }
}

impl Default for PluginParams {
    fn default() -> Self {
        Self::empty()
    }
}

/// The shape of one declared parameter (host-side mirror of what the
/// plugin's descriptor reports).
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde-types", derive(Serialize, Deserialize))]
pub struct ParamDescriptor {
    /// Stable parameter index (0..param_count).
    pub index: u32,
    /// Human-readable label.
    pub label: String,
    /// Default value.
    pub default: f32,
    /// Minimum value.
    pub min: f32,
    /// Maximum value.
    pub max: f32,
}

/// Convenience helpers for validating host-side parameter input.
impl ParamDescriptor {
    /// Clamp a value into the declared range.
    pub fn clamp(&self, value: f32) -> f32 {
        value.clamp(self.min, self.max)
    }

    /// `true` when `value` is within the declared range.
    pub fn contains(&self, value: f32) -> bool {
        value >= self.min && value <= self.max
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
