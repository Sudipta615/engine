//! Formal spatial representations (§4.3, Item 21).
//!
//! Provides a formal representation abstraction separating content description
//! from physical channel layout assumptions:
//!
//! ```text
//! SpatialRepresentation
//! ├── ChannelBased (speaker layout, e.g. stereo, 5.1, 7.1.4)
//! ├── ObjectBased (discrete localized/extended audio sources)
//! ├── HOA (Higher Order Ambisonics spherical harmonics)
//! ├── Binaural (2-channel ear-rendered head model)
//! └── Hybrid (unified objects + beds + fields mixer)
//! ```
//!
//! # Objective
//! Prevent physical-channel assumptions from leaking into spatial representations.
//! Renderers operate on representation-independent inputs with explicit, testable
//! conversion semantics.

use serde::{Deserialize, Serialize};

use super::speaker::SpeakerLayout;
use crate::standards::spatial::{AmbisonicNormalizationStandard, AmbisonicOrderingStandard};

/// Discriminant category for spatial audio representations (§4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpatialRepresentationKind {
    /// Discrete physical speaker layout.
    ChannelBased,
    /// Coordinate-based discrete dynamic point or extended sources.
    ObjectBased,
    /// Continuous spherical-harmonic soundfield (Higher-Order Ambisonics).
    Hoa,
    /// 2-channel head-model binaural representation.
    Binaural,
    /// Mixed hybrid containing objects, beds, and diffuse fields.
    Hybrid,
}

/// Representation abstraction representing the structure and semantics of spatial audio.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SpatialRepresentation {
    /// Fixed or custom multichannel speaker reproduction layout.
    ChannelBased {
        /// Total channel count.
        channels: usize,
        /// Optional layout display name or descriptor.
        #[serde(default)]
        layout_name: Option<String>,
    },
    /// Discrete spatial audio objects with dynamic 3D positions/extents.
    ObjectBased {
        /// Number of currently active discrete objects.
        object_count: usize,
        /// Hard allocation capacity ceiling for objects.
        max_objects: usize,
    },
    /// Higher-Order Ambisonic (HOA) soundfield representation.
    Hoa {
        /// Ambisonic order $N \ge 0$.
        order: u8,
        /// Total spherical harmonic channels: $(N + 1)^2$.
        channels: usize,
        /// Channel indexing standard (e.g. ACN, FuMa).
        ordering: AmbisonicOrderingStandard,
        /// Spherical harmonic normalization standard (e.g. SN3D, N3D).
        normalization: AmbisonicNormalizationStandard,
    },
    /// Two-channel binaural presentation (intended for headphones).
    Binaural,
    /// Hybrid spatial presentation combining objects, beds, and diffuse fields.
    Hybrid {
        /// Active object source count.
        objects: usize,
        /// Active channel-based beds count.
        beds: usize,
        /// Active diffuse fields count.
        fields: usize,
    },
}

impl SpatialRepresentation {
    /// Create a channel-based representation from a [`SpeakerLayout`].
    pub fn channel_based(layout: &SpeakerLayout) -> Self {
        let channels = layout.speakers.len();
        Self::ChannelBased {
            channels,
            layout_name: None,
        }
    }

    /// Create a channel-based representation with explicit channel count and name.
    pub fn channel_based_named(channels: usize, name: impl Into<String>) -> Self {
        Self::ChannelBased {
            channels,
            layout_name: Some(name.into()),
        }
    }

    /// Create an object-based representation.
    pub fn object_based(object_count: usize, max_objects: usize) -> Self {
        Self::ObjectBased {
            object_count,
            max_objects: max_objects.max(object_count),
        }
    }

    /// Create a Higher-Order Ambisonic representation for order `order`.
    pub fn hoa(
        order: u8,
        ordering: AmbisonicOrderingStandard,
        normalization: AmbisonicNormalizationStandard,
    ) -> Self {
        let channels = ((order as usize) + 1) * ((order as usize) + 1);
        Self::Hoa {
            order,
            channels,
            ordering,
            normalization,
        }
    }

    /// Default Higher-Order Ambisonic representation using ITU-R BS.2076 conventions (ACN / SN3D).
    pub fn hoa_acn_sn3d(order: u8) -> Self {
        Self::hoa(
            order,
            AmbisonicOrderingStandard::Acn,
            AmbisonicNormalizationStandard::Sn3d,
        )
    }

    /// Create a binaural representation.
    pub const fn binaural() -> Self {
        Self::Binaural
    }

    /// Create a hybrid representation.
    pub fn hybrid(objects: usize, beds: usize, fields: usize) -> Self {
        Self::Hybrid {
            objects,
            beds,
            fields,
        }
    }

    /// Get the discriminant kind.
    pub fn kind(&self) -> SpatialRepresentationKind {
        match self {
            Self::ChannelBased { .. } => SpatialRepresentationKind::ChannelBased,
            Self::ObjectBased { .. } => SpatialRepresentationKind::ObjectBased,
            Self::Hoa { .. } => SpatialRepresentationKind::Hoa,
            Self::Binaural => SpatialRepresentationKind::Binaural,
            Self::Hybrid { .. } => SpatialRepresentationKind::Hybrid,
        }
    }

    /// Report total channel count for this representation.
    pub fn channel_count(&self) -> usize {
        match self {
            Self::ChannelBased { channels, .. } => *channels,
            Self::ObjectBased { object_count, .. } => *object_count,
            Self::Hoa { channels, .. } => *channels,
            Self::Binaural => 2,
            Self::Hybrid {
                objects,
                beds,
                fields,
            } => objects + beds + fields,
        }
    }

    /// Whether this representation is rendered straight to binaural stereo.
    pub fn is_binaural(&self) -> bool {
        matches!(self, Self::Binaural)
    }

    /// Whether this representation describes a coordinate-free continuous soundfield.
    pub fn is_soundfield(&self) -> bool {
        matches!(self, Self::Hoa { .. })
    }
}

/// Explicit conversion paths between spatial audio representations (§4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepresentationConversion {
    /// Object-to-Ambisonics encoding via spherical harmonics basis functions.
    ObjectToHoa,
    /// Object-to-Speakers panning via VBAP or equal-power distribution.
    ObjectToChannel,
    /// Ambisonic decoding to physical loudspeaker feeds via decoder matrix.
    HoaToChannel,
    /// Ambisonic binaural rendering via virtual loudspeaker or SH filters.
    HoaToBinaural,
    /// Physical loudspeaker layout binauralized via virtual speaker simulation.
    ChannelToBinaural,
    /// Loudspeaker layout encoded into Ambisonics soundfield.
    ChannelToHoa,
    /// Complete hybrid scene rendering to multichannel or binaural.
    HybridToChannel,
    HybridToBinaural,
    /// Identity passthrough.
    Passthrough,
}

impl RepresentationConversion {
    /// Determine the valid conversion path from input to output representation.
    pub fn between(from: &SpatialRepresentation, to: &SpatialRepresentation) -> Option<Self> {
        match (from.kind(), to.kind()) {
            (a, b) if a == b => Some(Self::Passthrough),
            (SpatialRepresentationKind::ObjectBased, SpatialRepresentationKind::Hoa) => {
                Some(Self::ObjectToHoa)
            }
            (SpatialRepresentationKind::ObjectBased, SpatialRepresentationKind::ChannelBased) => {
                Some(Self::ObjectToChannel)
            }
            (SpatialRepresentationKind::Hoa, SpatialRepresentationKind::ChannelBased) => {
                Some(Self::HoaToChannel)
            }
            (SpatialRepresentationKind::Hoa, SpatialRepresentationKind::Binaural) => {
                Some(Self::HoaToBinaural)
            }
            (SpatialRepresentationKind::ChannelBased, SpatialRepresentationKind::Binaural) => {
                Some(Self::ChannelToBinaural)
            }
            (SpatialRepresentationKind::ChannelBased, SpatialRepresentationKind::Hoa) => {
                Some(Self::ChannelToHoa)
            }
            (SpatialRepresentationKind::Hybrid, SpatialRepresentationKind::ChannelBased) => {
                Some(Self::HybridToChannel)
            }
            (SpatialRepresentationKind::Hybrid, SpatialRepresentationKind::Binaural) => {
                Some(Self::HybridToBinaural)
            }
            _ => None,
        }
    }
}

/// Representation-independent input descriptor (§4.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpatialInputDescriptor {
    /// The structural representation of this stream.
    pub representation: SpatialRepresentation,
    /// Audio sample rate in Hz.
    pub sample_rate: u32,
    /// Worst-case frame block size.
    pub block_size: usize,
}

impl SpatialInputDescriptor {
    pub fn new(representation: SpatialRepresentation, sample_rate: u32, block_size: usize) -> Self {
        Self {
            representation,
            sample_rate,
            block_size,
        }
    }

    /// Check whether this input descriptor can be rendered directly to target representation.
    pub fn conversion_to(
        &self,
        target: &SpatialRepresentation,
    ) -> Option<RepresentationConversion> {
        RepresentationConversion::between(&self.representation, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn representation_kinds_and_channels() {
        let hoa3 = SpatialRepresentation::hoa_acn_sn3d(3);
        assert_eq!(hoa3.kind(), SpatialRepresentationKind::Hoa);
        assert_eq!(hoa3.channel_count(), 16); // (3+1)^2

        let hoa9 = SpatialRepresentation::hoa_acn_sn3d(9);
        assert_eq!(hoa9.channel_count(), 100); // (9+1)^2

        let bin = SpatialRepresentation::binaural();
        assert_eq!(bin.kind(), SpatialRepresentationKind::Binaural);
        assert_eq!(bin.channel_count(), 2);
        assert!(bin.is_binaural());

        let obj = SpatialRepresentation::object_based(8, 64);
        assert_eq!(obj.kind(), SpatialRepresentationKind::ObjectBased);
        assert_eq!(obj.channel_count(), 8);
    }

    #[test]
    fn conversion_matrix_validity() {
        let obj = SpatialRepresentation::object_based(4, 32);
        let hoa = SpatialRepresentation::hoa_acn_sn3d(1);
        let bin = SpatialRepresentation::binaural();
        let ch = SpatialRepresentation::channel_based(&SpeakerLayout::stereo());

        assert_eq!(
            RepresentationConversion::between(&obj, &hoa),
            Some(RepresentationConversion::ObjectToHoa)
        );
        assert_eq!(
            RepresentationConversion::between(&obj, &ch),
            Some(RepresentationConversion::ObjectToChannel)
        );
        assert_eq!(
            RepresentationConversion::between(&hoa, &ch),
            Some(RepresentationConversion::HoaToChannel)
        );
        assert_eq!(
            RepresentationConversion::between(&hoa, &bin),
            Some(RepresentationConversion::HoaToBinaural)
        );
        assert_eq!(
            RepresentationConversion::between(&ch, &bin),
            Some(RepresentationConversion::ChannelToBinaural)
        );
        assert_eq!(
            RepresentationConversion::between(&hoa, &hoa),
            Some(RepresentationConversion::Passthrough)
        );
    }
}
