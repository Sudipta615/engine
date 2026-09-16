//! Strongly-typed channel counts and dimensionality metrics (§4.4, Item 22).
//!
//! Separates physical hardware channel limits from continuous spatial field
//! dimensions, object budgets, and internal bus topologies.
//!
//! Prevents fixed global channel limits (e.g. `MAX_CHANNELS = 16`) from
//! preventing Higher-Order Ambisonics (e.g. Order 9 = 100 channels) or
//! bloating physical stereo/multichannel buffers.

use serde::{Deserialize, Serialize};

/// Maximum physical hardware DAC/speaker channels supported by the physical buffer.
pub const MAX_PHYSICAL_CHANNELS: usize = 32;

/// Maximum Higher-Order Ambisonic order supported by the HOA architecture ($N = 9$).
pub const MAX_SPATIAL_FIELD_ORDER: u8 = 9;

/// Maximum number of Ambisonic channels for Order 9 ($100 = (9+1)^2$).
pub const MAX_HOA_CHANNELS: usize =
    (MAX_SPATIAL_FIELD_ORDER as usize + 1) * (MAX_SPATIAL_FIELD_ORDER as usize + 1);

/// Maximum discrete spatial objects in a single scene.
pub const MAX_OBJECT_COUNT: usize = 64;

/// Strongly-typed count of physical hardware channels (1..=32).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PhysicalChannelCount(u16);

impl PhysicalChannelCount {
    pub const STEREO: Self = Self(2);
    pub const SURROUND_5_1: Self = Self(6);
    pub const SURROUND_7_1: Self = Self(8);
    pub const IMMERSIVE_7_1_4: Self = Self(12);
    pub const IMMERSIVE_9_1_6: Self = Self(16);

    pub fn new(count: u16) -> Option<Self> {
        if count > 0 && (count as usize) <= MAX_PHYSICAL_CHANNELS {
            Some(Self(count))
        } else {
            None
        }
    }

    #[inline]
    pub const fn get(&self) -> usize {
        self.0 as usize
    }

    #[inline]
    pub const fn raw(&self) -> u16 {
        self.0
    }
}

impl Default for PhysicalChannelCount {
    fn default() -> Self {
        Self::STEREO
    }
}

/// Strongly-typed count of internal mix-bus channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BusChannelCount(u16);

impl BusChannelCount {
    pub fn new(count: u16) -> Self {
        Self(count.max(1))
    }

    #[inline]
    pub const fn get(&self) -> usize {
        self.0 as usize
    }
}

impl Default for BusChannelCount {
    fn default() -> Self {
        Self(2)
    }
}

/// Strongly-typed Higher-Order Ambisonic (HOA) field order ($N \in 0..=9$).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SpatialFieldOrder(u8);

impl SpatialFieldOrder {
    pub const ORDER_0: Self = Self(0); // 1 ch (monopole)
    pub const ORDER_1: Self = Self(1); // 4 ch (FOA / B-format)
    pub const ORDER_2: Self = Self(2); // 9 ch (SOA)
    pub const ORDER_3: Self = Self(3); // 16 ch (TOA)
    pub const ORDER_5: Self = Self(5); // 36 ch
    pub const ORDER_7: Self = Self(7); // 64 ch
    pub const ORDER_9: Self = Self(9); // 100 ch (Ultra-HOA)

    /// Construct a spatial field order bounded to `0..=MAX_SPATIAL_FIELD_ORDER`.
    pub fn new(order: u8) -> Option<Self> {
        if order <= MAX_SPATIAL_FIELD_ORDER {
            Some(Self(order))
        } else {
            None
        }
    }

    /// Construct from channel count $M = (N + 1)^2$.
    pub fn from_channel_count(channels: usize) -> Option<Self> {
        let root = (channels as f64).sqrt().round() as usize;
        if root * root == channels && root > 0 {
            let order = (root - 1) as u8;
            Self::new(order)
        } else {
            None
        }
    }

    #[inline]
    pub const fn order(&self) -> u8 {
        self.0
    }

    /// Total spherical harmonic channels: $(N + 1)^2$.
    #[inline]
    pub const fn channels(&self) -> usize {
        let n = (self.0 as usize) + 1;
        n * n
    }
}

impl Default for SpatialFieldOrder {
    fn default() -> Self {
        Self::ORDER_1
    }
}

/// Strongly-typed count of Higher-Order Ambisonics channels $(N+1)^2$ (1..=100) (§4.4, Item 22).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HOAChannelCount(usize);

impl HOAChannelCount {
    pub const FOA: Self = Self(4);
    pub const SOA: Self = Self(9);
    pub const TOA: Self = Self(16);
    pub const ORDER_9: Self = Self(100);

    pub fn new(count: usize) -> Option<Self> {
        if count > 0 && count <= MAX_HOA_CHANNELS {
            let root = (count as f64).sqrt().round() as usize;
            if root * root == count {
                return Some(Self(count));
            }
        }
        None
    }

    pub fn from_order(order: SpatialFieldOrder) -> Self {
        Self(order.channels())
    }

    #[inline]
    pub const fn get(&self) -> usize {
        self.0
    }
}

impl Default for HOAChannelCount {
    fn default() -> Self {
        Self::FOA
    }
}

/// Strongly-typed count of active spatial objects.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct ObjectCount(usize);

impl ObjectCount {
    pub fn new(count: usize) -> Option<Self> {
        if count <= MAX_OBJECT_COUNT {
            Some(Self(count))
        } else {
            None
        }
    }

    #[inline]
    pub const fn get(&self) -> usize {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spatial_field_order_channels() {
        assert_eq!(SpatialFieldOrder::ORDER_0.channels(), 1);
        assert_eq!(SpatialFieldOrder::ORDER_1.channels(), 4);
        assert_eq!(SpatialFieldOrder::ORDER_2.channels(), 9);
        assert_eq!(SpatialFieldOrder::ORDER_3.channels(), 16);
        assert_eq!(SpatialFieldOrder::ORDER_5.channels(), 36);
        assert_eq!(SpatialFieldOrder::ORDER_7.channels(), 64);
        assert_eq!(SpatialFieldOrder::ORDER_9.channels(), 100);

        assert_eq!(
            SpatialFieldOrder::from_channel_count(16),
            Some(SpatialFieldOrder::ORDER_3)
        );
        assert_eq!(
            SpatialFieldOrder::from_channel_count(100),
            Some(SpatialFieldOrder::ORDER_9)
        );
        assert_eq!(SpatialFieldOrder::from_channel_count(15), None);
    }

    #[test]
    fn physical_channel_count_bounds() {
        assert!(PhysicalChannelCount::new(0).is_none());
        assert!(PhysicalChannelCount::new(2).is_some());
        assert!(PhysicalChannelCount::new(32).is_some());
        assert!(PhysicalChannelCount::new(33).is_none());
    }
}
