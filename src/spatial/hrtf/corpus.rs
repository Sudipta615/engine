use crate::spatial::math::Vec3;

/// Grid classification for measurement directions in an HRTF corpus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HrtfMeshKind {
    /// Measurements form a full Cartesian product of discrete azimuths and elevations.
    #[default]
    RegularGrid,
    /// Measurements are distributed arbitrarily or irregularly on the sphere.
    IrregularMesh,
}

/// A single measured head-related impulse response: unit source direction and
/// paired left/right impulse responses.
#[derive(Debug, Clone, PartialEq)]
pub struct HrtfMeasurement {
    /// Unit source direction `[x, y, z]` (`+X` right, `+Y` front, `+Z` up).
    pub direction: [f32; 3],
    /// Left-ear impulse response, time-ordered, at [`HrtfCorpus::sample_rate`].
    pub left: Vec<f32>,
    /// Right-ear impulse response, time-ordered, at [`HrtfCorpus::sample_rate`].
    pub right: Vec<f32>,
}

/// A measured HRTF corpus (SOFA-style): collection of measurement directions
/// with paired impulse responses, sample rate, provenance, and mesh hint.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HrtfCorpus {
    /// Sample rate the IRs were recorded at (e.g. 44.1 / 48 kHz).
    pub sample_rate: u32,
    /// Optional provenance / corpus name (CIPIC, TU-Berlin, KEMAR, etc.).
    pub source: Option<String>,
    /// The measurements.
    pub measurements: Vec<HrtfMeasurement>,
    /// Optional mesh hint (auto-detected if None).
    pub mesh_hint: Option<HrtfMeshKind>,
}

impl HrtfCorpus {
    /// Detect whether measurement directions form a regular Cartesian grid or an irregular mesh.
    pub fn detect_mesh_kind(&self) -> HrtfMeshKind {
        if let Some(hint) = self.mesh_hint {
            return hint;
        }
        if self.measurements.is_empty() {
            return HrtfMeshKind::RegularGrid;
        }

        let mut azimuths: Vec<f32> = Vec::new();
        let mut elevations: Vec<f32> = Vec::new();

        for m in &self.measurements {
            let d = Vec3::new(m.direction[0], m.direction[1], m.direction[2]);
            let Some(dn) = d.normalized() else {
                return HrtfMeshKind::IrregularMesh;
            };
            let az = dn.azimuth_rad().to_degrees().rem_euclid(360.0);
            let el = dn.elevation_rad().to_degrees();

            if !az.is_finite() || !el.is_finite() {
                return HrtfMeshKind::IrregularMesh;
            }

            if !azimuths.iter().any(|&x| (x - az).abs() < 1e-2) {
                azimuths.push(az);
            }
            if !elevations.iter().any(|&x| (x - el).abs() < 1e-2) {
                elevations.push(el);
            }
        }

        // Must equal full Cartesian product
        if self.measurements.len() != azimuths.len() * elevations.len() {
            return HrtfMeshKind::IrregularMesh;
        }

        let mut seen = std::collections::HashSet::with_capacity(self.measurements.len());
        for m in &self.measurements {
            let d = Vec3::new(m.direction[0], m.direction[1], m.direction[2]);
            let Some(dn) = d.normalized() else {
                return HrtfMeshKind::IrregularMesh;
            };
            let az = dn.azimuth_rad().to_degrees().rem_euclid(360.0);
            let el = dn.elevation_rad().to_degrees();

            let Some(ia) = azimuths.iter().position(|&x| (x - az).abs() < 1e-2) else {
                return HrtfMeshKind::IrregularMesh;
            };
            let Some(ie) = elevations.iter().position(|&x| (x - el).abs() < 1e-2) else {
                return HrtfMeshKind::IrregularMesh;
            };
            let key = ia * elevations.len() + ie;
            if !seen.insert(key) {
                return HrtfMeshKind::IrregularMesh;
            }
        }

        HrtfMeshKind::RegularGrid
    }
}

/// Optional normalization applied to each (left, right) ear pair when loading a corpus.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HrtfNormalize {
    /// No normalization — keep the raw sample amplitudes.
    #[default]
    None,
    /// Divide every ear pair by its overall peak magnitude (unity peak).
    Peak,
}

/// Controls HRTF loading: tap length, target sample rate, and normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HrtfLoadOptions {
    /// Impulse-response length in taps (<= [`MAX_HRTF_TAPS`]); shorter IRs are zero-padded.
    pub taps: usize,
    /// Sample rate to render at; IRs are resampled if `corpus.sample_rate` differs.
    pub target_sample_rate: u32,
    /// Optional peak normalization.
    pub normalize: HrtfNormalize,
}

impl Default for HrtfLoadOptions {
    fn default() -> Self {
        Self {
            taps: 64,
            target_sample_rate: 48_000,
            normalize: HrtfNormalize::None,
        }
    }
}

/// Errors from loading a measured HRTF corpus.
#[derive(Debug, Clone, PartialEq)]
pub enum HrtfLoadError {
    /// The corpus has no measurements.
    Empty,
    /// The impulse-response length is outside the supported range.
    Taps { got: usize, max: usize },
    /// A measurement direction is not a finite unit vector.
    DirectionNonFinite,
    /// The measurement directions do not form a regular Cartesian grid.
    IrregularMesh,
    /// A raw IR contains a non-finite sample.
    NonFiniteIr,
    /// Corpus JSON I/O or parsing failed.
    Json(String),
}

impl std::fmt::Display for HrtfLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "hrtf corpus: no measurements"),
            Self::Taps { got, max } => {
                write!(f, "hrtf corpus: taps {got} out of range (max {max})")
            }
            Self::DirectionNonFinite => {
                write!(f, "hrtf corpus: non-finite or zero measurement direction")
            }
            Self::IrregularMesh => write!(
                f,
                "hrtf corpus: measurement directions are not a regular azimuth × elevation grid",
            ),
            Self::NonFiniteIr => write!(f, "hrtf corpus: non-finite IR sample"),
            Self::Json(e) => write!(f, "hrtf corpus json: {e}"),
        }
    }
}

impl std::error::Error for HrtfLoadError {}

/// Resample a mono impulse response from `src_rate` to `dst_rate` by
/// piecewise-linear interpolation.
pub fn resample_impulse(src: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    let (sr, dr) = (src_rate.max(1) as f64, dst_rate.max(1) as f64);
    if (sr - dr).abs() < 1e-9 {
        return src.to_vec();
    }
    let out_len = ((src.len() as f64) * dr / sr).ceil().max(1.0) as usize;
    let mut out = Vec::with_capacity(out_len);
    for n in 0..out_len {
        let t = n as f64 * sr / dr;
        let i = t.floor() as usize;
        let frac = (t - i as f64) as f32;
        let a = src.get(i).copied().unwrap_or(0.0);
        let b = src.get(i + 1).copied().unwrap_or(0.0);
        out.push(a + frac * (b - a));
    }
    out
}

/// Save a measured HRTF corpus to a compact JSON file.
pub fn save_hrtf_corpus_json(
    path: &std::path::Path,
    corpus: &HrtfCorpus,
) -> Result<(), HrtfLoadError> {
    use serde_json::{json, Value};
    let measurements: Vec<Value> = corpus
        .measurements
        .iter()
        .map(|m| {
            let left: Vec<f64> = m.left.iter().map(|&v| v as f64).collect();
            let right: Vec<f64> = m.right.iter().map(|&v| v as f64).collect();
            json!({ "direction": m.direction, "left": left, "right": right })
        })
        .collect();
    let root = json!({
        "sample_rate": corpus.sample_rate,
        "source": corpus.source,
        "measurements": measurements,
    });
    let bytes = serde_json::to_vec_pretty(&root).map_err(|e| HrtfLoadError::Json(e.to_string()))?;
    std::fs::write(path, bytes).map_err(|e| HrtfLoadError::Json(e.to_string()))
}

/// Load a measured HRTF corpus from a JSON file.
pub fn load_hrtf_corpus_json(path: &std::path::Path) -> Result<HrtfCorpus, HrtfLoadError> {
    use serde_json::Value;
    let text = std::fs::read_to_string(path).map_err(|e| HrtfLoadError::Json(e.to_string()))?;
    let root: Value =
        serde_json::from_str(&text).map_err(|e| HrtfLoadError::Json(e.to_string()))?;
    let sample_rate = root
        .get("sample_rate")
        .and_then(Value::as_u64)
        .map(|r| r as u32)
        .ok_or_else(|| HrtfLoadError::Json("missing sample_rate".into()))?;
    let source = root
        .get("source")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    let ms = root
        .get("measurements")
        .and_then(Value::as_array)
        .ok_or_else(|| HrtfLoadError::Json("missing measurements array".into()))?;
    let mut measurements = Vec::with_capacity(ms.len());
    for (i, m) in ms.iter().enumerate() {
        let dir = m
            .get("direction")
            .and_then(Value::as_array)
            .ok_or_else(|| HrtfLoadError::Json(format!("measurement {i}: missing direction")))?;
        if dir.len() != 3 {
            return Err(HrtfLoadError::Json(format!(
                "measurement {i}: direction not 3D"
            )));
        }
        let direction = [
            dir[0].as_f64().unwrap_or(0.0) as f32,
            dir[1].as_f64().unwrap_or(0.0) as f32,
            dir[2].as_f64().unwrap_or(0.0) as f32,
        ];
        let left = m
            .get("left")
            .and_then(Value::as_array)
            .ok_or_else(|| HrtfLoadError::Json(format!("measurement {i}: missing left")))?
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0) as f32)
            .collect();
        let right = m
            .get("right")
            .and_then(Value::as_array)
            .ok_or_else(|| HrtfLoadError::Json(format!("measurement {i}: missing right")))?
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0) as f32)
            .collect();
        measurements.push(HrtfMeasurement {
            direction,
            left,
            right,
        });
    }
    Ok(HrtfCorpus {
        sample_rate,
        source,
        measurements,
        mesh_hint: None,
    })
}
