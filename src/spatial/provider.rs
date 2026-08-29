//! HRTF provider abstraction (spec §60) and SOFA-data adapters.
//!
//! The binaural renderer consumes an [`HrtfProvider`] rather than a concrete
//! file format or dataset, so measured HRTFs, an analytic model, or a SOFA
//! corpus all plug in behind one seam (spec §118–119: adapters translate into
//! the engine's internal representation, never bypass validation).
//!
//! Implementations:
//! - [`HrtfDatasetProvider`] — wraps the engine's [`HrtfDataset`] (measured
//!   spectral HRIRs, bilinearly interpolated in azimuth/elevation).
//! - [`HrtfCorpusProvider`] — builds a renderable dataset from an
//!   [`HrtfCorpus`] (the shape a genuine SOFA import reduces to) at load
//!   time, then serves it like a dataset.
//!
//! ## Realtime discipline
//!
//! A provider is **read-only and allocation-free on the audio thread**: the
//! lookup writes both ear FIRs into caller-provided scratch slices
//! ([`HrtfProvider::interpolate`]). A provider with no dataset (the analytic
//! head-model path) reports `taps() == 0` and the renderer falls back to the
//! analytic ITD + head-shadow shelf.

use super::hrtf::{HrtfCorpus, HrtfDataset, HrtfLoadOptions};

/// The ear selector used by HRTF lookups (Re-export of `hrtf::Ear`).
pub use super::hrtf::Ear;

/// The provider seam: a direction + distance yields both ear FIRs into
/// caller-provided scratch. Must be `Send`, and `interpolate` must not
/// allocate or lock.
pub trait HrtfProvider: Send {
    /// The sample rate the returned HRTFs are valid at.
    fn sample_rate(&self) -> u32;
    /// Number of FIR taps per ear (0 = no dataset; analytic path).
    fn taps(&self) -> usize;
    /// Interpolate the HRTF for a (unit) source direction in listener space
    /// and a distance in metres, writing `taps()` values into `left_out` and
    /// `right_out` (each must hold ≥ `taps()`). Must not produce NaN.
    fn interpolate(
        &self,
        direction: super::math::Vec3,
        distance: f32,
        left_out: &mut [f32],
        right_out: &mut [f32],
    );
}

/// A provider that serves an already-built [`HrtfDataset`] (measured or the
/// synthetic corpus), interpolated bilinearly in azimuth/elevation (§62).
pub struct HrtfDatasetProvider {
    dataset: HrtfDataset,
}

impl HrtfDatasetProvider {
    pub fn new(dataset: HrtfDataset) -> Self {
        Self { dataset }
    }
    pub fn dataset(&self) -> &HrtfDataset {
        &self.dataset
    }
}

impl HrtfProvider for HrtfDatasetProvider {
    fn sample_rate(&self) -> u32 {
        0 // unknown from the dataset alone; corpus providers record the rate
    }
    fn taps(&self) -> usize {
        self.dataset.taps()
    }
    fn interpolate(
        &self,
        direction: super::math::Vec3,
        _distance: f32,
        left_out: &mut [f32],
        right_out: &mut [f32],
    ) {
        let az = direction.azimuth_rad().to_degrees();
        let el = direction.elevation_rad().to_degrees();
        self.dataset
            .bilinear_interpolate(az, el, Ear::Left, left_out);
        self.dataset
            .bilinear_interpolate(az, el, Ear::Right, right_out);
    }
}

/// Builds a renderable dataset from an [`HrtfCorpus`] (SOFA-style) at load
/// time and serves it. This is the shape a genuine SOFA importer reduces to:
/// validate + resample + trim → dataset → serve (§61, §119).
pub struct HrtfCorpusProvider {
    dataset: HrtfDataset,
    sample_rate: u32,
    source: Option<String>,
}

impl HrtfCorpusProvider {
    pub fn from_corpus(
        corpus: &HrtfCorpus,
        options: &HrtfLoadOptions,
    ) -> Result<Self, super::hrtf::HrtfLoadError> {
        let dataset = HrtfDataset::from_corpus(corpus, options)?;
        let sample_rate = options.target_sample_rate;
        Ok(Self {
            dataset,
            sample_rate,
            source: corpus.source.clone(),
        })
    }

    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }
}

impl HrtfProvider for HrtfCorpusProvider {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn taps(&self) -> usize {
        self.dataset.taps()
    }
    fn interpolate(
        &self,
        direction: super::math::Vec3,
        _distance: f32,
        left_out: &mut [f32],
        right_out: &mut [f32],
    ) {
        let az = direction.azimuth_rad().to_degrees();
        let el = direction.elevation_rad().to_degrees();
        self.dataset
            .bilinear_interpolate(az, el, Ear::Left, left_out);
        self.dataset
            .bilinear_interpolate(az, el, Ear::Right, right_out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::math::Vec3;

    #[test]
    fn dataset_provider_interpolates_both_ears() {
        let ds = HrtfDataset::synthetic(48_000, 32, 30.0, 30.0);
        let p = HrtfDatasetProvider::new(ds);
        assert_eq!(p.taps(), 32);
        let mut l = vec![0.0f32; 32];
        let mut r = vec![0.0f32; 32];
        p.interpolate(Vec3::Y, 2.0, &mut l, &mut r);
        assert!(l.iter().all(|v| v.is_finite()));
        assert!(r.iter().all(|v| v.is_finite()));
        // Front +Y is symmetric: the front IR is equal in both ears.
        let diff: f32 = l.iter().zip(r.iter()).map(|(a, b)| (a - b).abs()).sum();
        assert!(diff < 1e-4, "front symmetric: {diff}");
    }

    #[test]
    fn corpus_provider_validates_and_serves() {
        // A tiny measured corpus at 48k.
        let mut meas = Vec::new();
        for (az, el) in [(0.0f32, 0.0f32), (0.0, 30.0), (90.0, 0.0), (90.0, 30.0)] {
            let az_r = az.to_radians();
            let el_r = el.to_radians();
            let d = Vec3::new(el_r.cos() * az_r.sin(), el_r.cos() * az_r.cos(), el_r.sin());
            meas.push(crate::spatial::hrtf::HrtfMeasurement {
                direction: [d.x, d.y, d.z],
                left: vec![1.0; 16],
                right: vec![1.0; 16],
            });
        }
        let corpus = HrtfCorpus {
            sample_rate: 48_000,
            source: Some("test".into()),
            measurements: meas,
        };
        let opts = HrtfLoadOptions {
            taps: 16,
            target_sample_rate: 48_000,
            normalize: crate::spatial::hrtf::HrtfNormalize::None,
        };
        let p = HrtfCorpusProvider::from_corpus(&corpus, &opts).expect("valid corpus");
        assert_eq!(p.sample_rate(), 48_000);
        assert_eq!(p.taps(), 16);
        assert_eq!(p.source(), Some("test"));
    }
}
