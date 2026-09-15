//! HRTF dataset storage, synthetic generator, and multi-mesh interpolation (spec §47–48, §62).

use super::corpus::{
    resample_impulse, HrtfCorpus, HrtfLoadError, HrtfLoadOptions, HrtfMeshKind, HrtfNormalize,
};
use super::interpolate::SphericalHrtfInterpolator;
use super::quality::MAX_HRTF_TAPS;
use super::{
    ear_delay_sec, head_shadow_alpha, ElevationNotch, HeadShadow, DEFAULT_HEAD_RADIUS,
    DEFAULT_SPEED_OF_SOUND,
};
use crate::spatial::math::Vec3;

/// Which ear a head-model path serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ear {
    Left,
    Right,
}

impl Ear {
    pub const COUNT: usize = 2;

    #[inline]
    pub fn index(self) -> usize {
        match self {
            Ear::Left => 0,
            Ear::Right => 1,
        }
    }

    #[inline]
    pub fn from_index(i: usize) -> Ear {
        if i & 1 == 0 {
            Ear::Left
        } else {
            Ear::Right
        }
    }

    /// +1 toward the right, −1 toward the left (multiplies the azimuth).
    #[inline]
    pub fn side(self) -> f32 {
        match self {
            Ear::Left => -1.0,
            Ear::Right => 1.0,
        }
    }
}

/// A dataset of measured (or synthetic) head-related impulse responses.
/// Supports both regular azimuth × elevation Cartesian grids and arbitrary spherical meshes.
#[derive(Debug, Clone)]
pub struct HrtfDataset {
    azimuths: Vec<f32>,
    elevations: Vec<f32>,
    irs: Vec<f32>,
    taps: usize,
    mesh_kind: HrtfMeshKind,
    interpolator: Option<SphericalHrtfInterpolator>,
}

impl HrtfDataset {
    /// Build a dataset from explicit regular grids and IRs (flat `az × el × 2 × taps`).
    pub fn from_planes(
        azimuths: Vec<f32>,
        elevations: Vec<f32>,
        taps: usize,
        irs: Vec<f32>,
    ) -> Result<Self, &'static str> {
        if azimuths.is_empty() || elevations.is_empty() {
            return Err("hrtf dataset: empty grid");
        }
        if taps == 0 || taps > MAX_HRTF_TAPS {
            return Err("hrtf dataset: taps out of range");
        }
        for w in azimuths.windows(2) {
            if w[0] >= w[1] {
                return Err("hrtf dataset: azimuth grid not strictly ascending");
            }
        }
        for w in elevations.windows(2) {
            if w[0] >= w[1] {
                return Err("hrtf dataset: elevation grid not strictly ascending");
            }
        }
        if azimuths.iter().any(|a| !(0.0..360.0).contains(a))
            || elevations.iter().any(|e| !(-90.0..=90.0).contains(e))
        {
            return Err("hrtf dataset: grid values out of range");
        }
        let want = azimuths.len() * elevations.len() * Ear::COUNT * taps;
        if irs.len() != want {
            return Err("hrtf dataset: IR length mismatch");
        }
        if irs.iter().any(|v| !v.is_finite()) {
            return Err("hrtf dataset: non-finite IR");
        }
        Ok(Self {
            azimuths,
            elevations,
            irs,
            taps,
            mesh_kind: HrtfMeshKind::RegularGrid,
            interpolator: None,
        })
    }

    /// A synthetic dataset discretizing the analytic model (head-shadow
    /// shelf + elevation notch + Woodworth ITD) on a regular grid.
    pub fn synthetic(sample_rate: u32, taps: usize, az_step_deg: f32, el_step_deg: f32) -> Self {
        let taps = taps.clamp(8, MAX_HRTF_TAPS);
        let mut azimuths = Vec::new();
        let mut a = 0.0f32;
        while a < 360.0 - 1e-4 {
            azimuths.push(a);
            a += az_step_deg;
        }
        let mut elevations = Vec::new();
        let mut e = -90.0f32;
        while e <= 90.0 + 1e-4 {
            elevations.push(e);
            e += el_step_deg;
        }
        let n = azimuths.len() * elevations.len() * Ear::COUNT * taps;
        let mut irs = vec![0.0f32; n];
        let fs = sample_rate.max(1_000) as f32;
        let mut idx = 0usize;
        for &az_deg in &azimuths {
            for &el_deg in &elevations {
                for ear in 0..Ear::COUNT {
                    let e = Ear::from_index(ear);
                    let az = az_deg.to_radians();
                    let el = el_deg.to_radians();
                    let delay =
                        ear_delay_sec(az, e, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND) * fs;
                    let whole = delay.floor() as usize;
                    let frac = delay - whole as f32;
                    let mut sh = HeadShadow::new();
                    sh.prepare(fs, DEFAULT_HEAD_RADIUS, DEFAULT_SPEED_OF_SOUND);
                    sh.set_target(head_shadow_alpha(az, e), 1.0);
                    let mut notch = ElevationNotch::new();
                    notch.prepare(fs);
                    notch.set_target(el, 1.0);
                    for k in 0..taps {
                        let imp = if k == 0 { 1.0 } else { 0.0 };
                        let mut v = sh.process(imp);
                        v = notch.process(v);
                        if k >= whole {
                            irs[idx + k] = v * (1.0 - frac);
                            if k + 1 < taps {
                                irs[idx + k + 1] += v * frac;
                            }
                        }
                    }
                    idx += taps;
                }
            }
        }
        Self {
            azimuths,
            elevations,
            irs,
            taps,
            mesh_kind: HrtfMeshKind::RegularGrid,
            interpolator: None,
        }
    }

    /// Build a dataset from a measured corpus, auto-detecting regular grid vs irregular mesh.
    pub fn from_corpus(
        corpus: &HrtfCorpus,
        options: &HrtfLoadOptions,
    ) -> Result<Self, HrtfLoadError> {
        match corpus.detect_mesh_kind() {
            HrtfMeshKind::RegularGrid => Self::from_corpus_regular(corpus, options),
            HrtfMeshKind::IrregularMesh => Self::from_corpus_irregular(corpus, options),
        }
    }

    /// Build a dataset from a regular grid corpus.
    pub fn from_corpus_regular(
        corpus: &HrtfCorpus,
        options: &HrtfLoadOptions,
    ) -> Result<Self, HrtfLoadError> {
        if corpus.measurements.is_empty() {
            return Err(HrtfLoadError::Empty);
        }
        let taps = options.taps;
        if taps == 0 || taps > MAX_HRTF_TAPS {
            return Err(HrtfLoadError::Taps {
                got: taps,
                max: MAX_HRTF_TAPS,
            });
        }
        let target_rate = options.target_sample_rate.max(1);
        let src_rate = corpus.sample_rate.max(1);
        let need_resample = src_rate != target_rate;

        let mut azimuths: Vec<f32> = Vec::new();
        let mut elevations: Vec<f32> = Vec::new();
        for m in &corpus.measurements {
            let d = Vec3::new(m.direction[0], m.direction[1], m.direction[2]);
            let dn = d.normalized().ok_or(HrtfLoadError::DirectionNonFinite)?;
            let az = dn.azimuth_rad().to_degrees().rem_euclid(360.0);
            let el = dn.elevation_rad().to_degrees();
            if !az.is_finite() || !el.is_finite() {
                return Err(HrtfLoadError::DirectionNonFinite);
            }
            if !azimuths.iter().any(|&x| (x - az).abs() < 1e-3) {
                azimuths.push(az);
            }
            if !elevations.iter().any(|&x| (x - el).abs() < 1e-3) {
                elevations.push(el);
            }
        }
        azimuths.sort_by(|a, b| a.total_cmp(b));
        elevations.sort_by(|a, b| a.total_cmp(b));
        if azimuths.is_empty() || elevations.is_empty() {
            return Err(HrtfLoadError::Empty);
        }

        let mut seen = std::collections::HashSet::with_capacity(corpus.measurements.len());
        let mut slabs: Vec<Vec<(Vec<f32>, Vec<f32>)>> = Vec::new();
        for m in &corpus.measurements {
            let d = Vec3::new(m.direction[0], m.direction[1], m.direction[2]);
            let dn = d.normalized().ok_or(HrtfLoadError::DirectionNonFinite)?;
            let az = dn.azimuth_rad().to_degrees().rem_euclid(360.0);
            let el = dn.elevation_rad().to_degrees();
            let ia = azimuths
                .iter()
                .position(|&x| (x - az).abs() < 1e-3)
                .ok_or(HrtfLoadError::IrregularMesh)?;
            let ie = elevations
                .iter()
                .position(|&x| (x - el).abs() < 1e-3)
                .ok_or(HrtfLoadError::IrregularMesh)?;
            let key = ia * elevations.len() + ie;
            if !seen.insert(key) {
                return Err(HrtfLoadError::IrregularMesh);
            }
            while slabs.len() <= ia {
                slabs.push(Vec::new());
            }
            while slabs[ia].len() <= ie {
                slabs[ia].push((Vec::new(), Vec::new()));
            }
            let (left, right) = if need_resample {
                (
                    resample_impulse(&m.left, src_rate, target_rate),
                    resample_impulse(&m.right, src_rate, target_rate),
                )
            } else {
                (m.left.clone(), m.right.clone())
            };
            slabs[ia][ie] = (left, right);
        }
        drop(seen);
        if slabs.iter().any(|col| col.len() != elevations.len()) {
            return Err(HrtfLoadError::IrregularMesh);
        }

        let mut irs: Vec<f32> =
            Vec::with_capacity(azimuths.len() * elevations.len() * Ear::COUNT * taps);
        for &az in &azimuths {
            let ia = azimuths
                .iter()
                .position(|&x| (x - az).abs() < 1e-3)
                .unwrap();
            for &el in &elevations {
                let ie = elevations
                    .iter()
                    .position(|&x| (x - el).abs() < 1e-3)
                    .unwrap();
                let (l, r) = slabs[ia][ie].clone();
                let (mut left, mut right) = (l, r);
                if left.len() > taps {
                    left.truncate(taps);
                } else {
                    left.resize(taps, 0.0);
                }
                if right.len() > taps {
                    right.truncate(taps);
                } else {
                    right.resize(taps, 0.0);
                }
                if matches!(options.normalize, HrtfNormalize::Peak) {
                    let peak = left
                        .iter()
                        .chain(right.iter())
                        .fold(0.0f32, |m, v| m.max(v.abs()))
                        .max(1e-12);
                    for v in left.iter_mut() {
                        *v /= peak;
                    }
                    for v in right.iter_mut() {
                        *v /= peak;
                    }
                }
                if left.iter().any(|v| !v.is_finite()) || right.iter().any(|v| !v.is_finite()) {
                    return Err(HrtfLoadError::NonFiniteIr);
                }
                irs.extend_from_slice(&left);
                irs.extend_from_slice(&right);
            }
        }
        Ok(HrtfDataset {
            azimuths,
            elevations,
            irs,
            taps,
            mesh_kind: HrtfMeshKind::RegularGrid,
            interpolator: None,
        })
    }

    /// Build a dataset from an irregular mesh corpus using spherical triangulation.
    pub fn from_corpus_irregular(
        corpus: &HrtfCorpus,
        options: &HrtfLoadOptions,
    ) -> Result<Self, HrtfLoadError> {
        if corpus.measurements.is_empty() {
            return Err(HrtfLoadError::Empty);
        }
        let taps = options.taps;
        if taps == 0 || taps > MAX_HRTF_TAPS {
            return Err(HrtfLoadError::Taps {
                got: taps,
                max: MAX_HRTF_TAPS,
            });
        }
        let target_rate = options.target_sample_rate.max(1);
        let src_rate = corpus.sample_rate.max(1);
        let need_resample = src_rate != target_rate;

        let mut directions = Vec::with_capacity(corpus.measurements.len());
        let mut irs = Vec::with_capacity(corpus.measurements.len() * Ear::COUNT * taps);

        for m in &corpus.measurements {
            let d = Vec3::new(m.direction[0], m.direction[1], m.direction[2]);
            let dn = d.normalized().ok_or(HrtfLoadError::DirectionNonFinite)?;
            directions.push(dn);

            let (mut left, mut right) = if need_resample {
                (
                    resample_impulse(&m.left, src_rate, target_rate),
                    resample_impulse(&m.right, src_rate, target_rate),
                )
            } else {
                (m.left.clone(), m.right.clone())
            };

            if left.len() > taps {
                left.truncate(taps);
            } else {
                left.resize(taps, 0.0);
            }
            if right.len() > taps {
                right.truncate(taps);
            } else {
                right.resize(taps, 0.0);
            }

            if matches!(options.normalize, HrtfNormalize::Peak) {
                let peak = left
                    .iter()
                    .chain(right.iter())
                    .fold(0.0f32, |m, v| m.max(v.abs()))
                    .max(1e-12);
                for v in left.iter_mut() {
                    *v /= peak;
                }
                for v in right.iter_mut() {
                    *v /= peak;
                }
            }

            if left.iter().any(|v| !v.is_finite()) || right.iter().any(|v| !v.is_finite()) {
                return Err(HrtfLoadError::NonFiniteIr);
            }

            irs.extend_from_slice(&left);
            irs.extend_from_slice(&right);
        }

        let interpolator = SphericalHrtfInterpolator::new(&directions);

        Ok(Self {
            azimuths: Vec::new(),
            elevations: Vec::new(),
            irs,
            taps,
            mesh_kind: HrtfMeshKind::IrregularMesh,
            interpolator: Some(interpolator),
        })
    }

    pub fn azimuths(&self) -> &[f32] {
        &self.azimuths
    }

    pub fn elevations(&self) -> &[f32] {
        &self.elevations
    }

    pub fn taps(&self) -> usize {
        self.taps
    }

    pub fn mesh_kind(&self) -> HrtfMeshKind {
        self.mesh_kind
    }

    pub fn ir(&self, az_idx: usize, el_idx: usize, ear: Ear) -> &[f32] {
        let base = (az_idx * self.elevations.len() + el_idx) * Ear::COUNT * self.taps
            + ear.index() * self.taps;
        &self.irs[base..base + self.taps]
    }

    /// Interpolate HRTF impulse responses for a given azimuth and elevation (degrees).
    pub fn bilinear_interpolate(&self, az_deg: f32, el_deg: f32, ear: Ear, out: &mut [f32]) {
        debug_assert!(out.len() >= self.taps);
        if self.mesh_kind == HrtfMeshKind::IrregularMesh {
            let az = az_deg.to_radians();
            let el = el_deg.to_radians();
            let horiz = el.cos();
            let dir = Vec3::new(az.sin() * horiz, az.cos() * horiz, el.sin());
            self.interpolate_direction(dir, ear, out);
            return;
        }

        let az = az_deg.rem_euclid(360.0);
        let el = el_deg.clamp(-90.0, 90.0);
        let na = self.azimuths.len();
        let ne = self.elevations.len();
        let (ia, fa) = if az <= self.azimuths[0] {
            (0usize, 0.0f32)
        } else {
            let mut lo = 0usize;
            for (i, w) in self.azimuths.iter().enumerate() {
                if *w <= az {
                    lo = i;
                } else {
                    break;
                }
            }
            let hi = (lo + 1) % na;
            let span = (self.azimuths[hi] - self.azimuths[lo])
                .rem_euclid(360.0)
                .max(1e-6);
            (lo, ((az - self.azimuths[lo]) / span).clamp(0.0, 1.0))
        };
        let (ie, fe) = if el <= self.elevations[0] {
            (0usize, 0.0f32)
        } else if el >= self.elevations[ne - 1] {
            (ne - 1, 0.0f32)
        } else {
            let mut lo = 0usize;
            for (i, w) in self.elevations.iter().enumerate() {
                if *w <= el {
                    lo = i;
                } else {
                    break;
                }
            }
            let hi = lo + 1;
            let span = (self.elevations[hi] - self.elevations[lo]).max(1e-6);
            (lo, ((el - self.elevations[lo]) / span).clamp(0.0, 1.0))
        };
        let ia2 = (ia + 1) % na;
        let ie2 = (ie + 1).min(ne - 1);
        let stride = Ear::COUNT * self.taps;
        let base = |ia: usize, ie: usize| (ia * ne + ie) * stride + ear.index() * self.taps;
        let a00 = &self.irs[base(ia, ie)..base(ia, ie) + self.taps];
        let a10 = &self.irs[base(ia2, ie)..base(ia2, ie) + self.taps];
        let a01 = &self.irs[base(ia, ie2)..base(ia, ie2) + self.taps];
        let a11 = &self.irs[base(ia2, ie2)..base(ia2, ie2) + self.taps];
        crate::dsp::simd::vector_bilinear(out, a00, a10, a01, a11, fa, fe, self.taps);
    }

    /// Interpolate HRTF impulse responses for any continuous 3D unit direction.
    pub fn interpolate_direction(&self, dir: Vec3, ear: Ear, out: &mut [f32]) {
        debug_assert!(out.len() >= self.taps);
        if let Some(ref interp) = self.interpolator {
            let stride = Ear::COUNT * self.taps;
            let ear_offset = ear.index() * self.taps;
            interp.interpolate(dir, &self.irs, self.taps, stride, ear_offset, out);
        } else {
            let dn = dir.normalized().unwrap_or(Vec3::Y);
            let az = dn.azimuth_rad().to_degrees().rem_euclid(360.0);
            let el = dn.elevation_rad().to_degrees();
            self.bilinear_interpolate(az, el, ear, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_validation_rejects_bad_grids() {
        assert!(HrtfDataset::from_planes(vec![], vec![0.0], 16, vec![0.0; 32]).is_err());
        assert!(HrtfDataset::from_planes(vec![0.0, 0.0], vec![0.0], 16, vec![0.0; 64]).is_err());
        assert!(HrtfDataset::from_planes(vec![0.0, 15.0], vec![0.0], 16, vec![0.0; 63]).is_err());
        assert!(
            HrtfDataset::from_planes(vec![0.0, 15.0], vec![0.0], 16, vec![f32::NAN; 64]).is_err()
        );
        assert!(
            HrtfDataset::from_planes(vec![0.0, 15.0], vec![0.0, 15.0], 16, vec![0.0; 128]).is_ok()
        );
    }

    #[test]
    fn synthetic_dataset_interpolates() {
        let ds = HrtfDataset::synthetic(48_000, 64, 15.0, 15.0);
        assert_eq!(ds.taps(), 64);
        assert_eq!(ds.mesh_kind(), HrtfMeshKind::RegularGrid);
        let mut out = [0.0f32; 64];
        ds.bilinear_interpolate(0.0, 0.0, Ear::Left, &mut out);
        for &val in &out {
            assert!(val.is_finite());
        }
    }
}
