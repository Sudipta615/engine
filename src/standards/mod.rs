//! # Standards & Compliance Subsystem
//!
//! Provides a centralized, typed standards framework for all standards-dependent
//! engine components. Eliminates scattered constant definitions and hardcoded
//! magic numbers by formalizing specifications across:
//!
//! - **Loudness**: ITU-R BS.1770-5, EBU R128, ReplayGain 2.0 ([`loudness`])
//! - **True Peak**: ITU-R BS.1770-5 Annex 2, EBU Tech 3341 ([`true_peak`])
//! - **Channel Layouts**: ITU-R BS.775, ITU-R BS.2051-3, SMPTE ST 2036-2 ([`channel_layout`])
//! - **Spatial Coordinates & HOA**: Right-Handed Cartesian, ACN/SN3D/N3D ([`spatial`])
//! - **Audio Definition Model (ADM)**: ITU-R BS.2076-1 / BS.2076-2 ([`adm`])
//! - **Audio Metadata**: BWF, BW64, iXML, ID3v2.4, VorbisComment ([`metadata`])

pub mod adm;
pub mod channel_layout;
pub mod loudness;
pub mod metadata;
pub mod spatial;
pub mod true_peak;

pub use adm::{
    AdmDocument, AdmStandard, AudioBlockFormat, AudioChannelFormat, AudioContent, AudioObject,
    AudioPackFormat, AudioProgramme, AudioStreamFormat, AudioTrackFormat, AudioTypeDefinition,
};
pub use channel_layout::ChannelLayoutStandard;
pub use loudness::LoudnessStandard;
pub use metadata::MetadataStandard;
pub use spatial::{
    AmbisonicNormalizationStandard, AmbisonicOrderingStandard, CoordinateSystemStandard,
};
pub use true_peak::TruePeakStandard;

/// Trait implemented by engine subsystems that declare compliance with a formal standard.
pub trait StandardizedComponent {
    /// The primary standard adhered to by this component.
    fn declared_standard(&self) -> &'static str;

    /// The version of the standard implemented.
    fn standard_version(&self) -> &'static str;

    /// Machine-readable summary of standards compliance.
    fn compliance_summary(&self) -> String {
        format!(
            "{} (version {})",
            self.declared_standard(),
            self.standard_version()
        )
    }
}
