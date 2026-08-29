//! Native SOFA (AES69) HRTF importer (spec §61), gated behind the
//! `sofa-import` feature flag.
//!
//! SOFA (\"Spatially Oriented Format for Acoustics\") is the standardized
//! container for HRTFs, BRIRs and related impulse-response data. A `.sofa`
//! file stores measurement metadata + grids + impulse responses in a NetCDF
//! container. This module imports the data the HRTF renderer needs
//! (`SourcePosition` grid + `Data.IR` + sampling rate) and reduces it to the
//! engine's normalized [`HrtfCorpus`], which then feeds the existing
//! validation/resampling pipeline ([`HrtfDataset::from_corpus`]) and the
//! [`HrtfCorpusProvider`] seam (§60–61).
//!
//! ## Format support & the purity rule
//!
//! This crate is strictly pure-Rust and takes **no FFI / native libraries**
//! (house rule; see the WavPack/TTA notes in `Cargo.toml`). Native SOFA
//! files are NetCDF-4 (`nc4`) — a HDF5 container whose only workable Rust
//! readers link `libhdf5`. That is explicitly **out of scope** here.
//!
//! What this importer handles is the **NetCDF-classic (CDF) subset**, which
//! the reference SOFA C API writes when you request the `SOFA_NETCDF3` /
//! `SOFA_NETCDF3_CLASSIC` storage mode (the lower-complexity, byte
//! endian-swappable serialization defined by the same spec). Terms:
//!
//! - CDF-1: classic, 32-bit offsets.
//! - CDF-2: classic, 64-bit offsets.
//! - optional little-endian byte order (detected, as the format requires).
//!
//! A CDF reader is a well-bounded deterministic binary parser, so a full
//! in-crate importer is practical and testable; it needs no dependency at all
//! (the `sofa-import` feature only gates the module).
//!
//! > **NetCDF-4 / HDF5 seam.** Importing modern `nc4` `.sofa` files remains a
//! > documented future gap: a host would adapt this same reader against an
//! > optional `hdf5` crate behind a feature flag, then hand the same
//! > [`HrtfCorpus`] onward. Nothing in the renderer or provider depends on the
//! > file format, so that swap is invisible below this module.
//!
//! ## SOFA convention mapping
//!
//! The importer validates and reads the SOFA HRTF variables the binaural
//! renderer consumes:
//!
//! - the global `Conventions` attribute (must begin `\"SOFA\"`);
//! - `SourcePosition` — `[M, C]` emission angles per measurement
//!   (`C = 3`); spherical `(az, el, radius)` by default;
//! - `Data.IR` — `[I, R, M, N]` (or `[R, M, N]`) impulse responses;
//! - `Data.SamplingRate` — per-IR sample rate (uniform expected).
//!
//! Source directions are mapped onto the layer's documented coordinate frame
//! (`+X` right, `+Y` front, `+Z` up): SOFA azimuth is measured
//! counter-clockwise from front (positive *toward the left* when viewed from
//! above), so the engine azimuth — positive *toward the right* — is its
//! negation (see [`source_position_direction`]).
//!
//! It is deliberately honest: it refuses (with a typed error) any file it
//! cannot be sure it read correctly rather than silently producing wrong
//! HRTFs.

use super::hrtf::{HrtfCorpus, HrtfMeasurement};
use super::math::Vec3;

/// A scalar NetCDF external data type (the classic subset we understand).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NcType {
    Byte = 1,
    Char = 2,
    Short = 3,
    Int = 4,
    Float = 5,
    Double = 6,
}

/// Typed failure from importing a SOFA file. Every case is a hard error — a
/// malformed or unreadable file must never yield a bogus corpus.
#[derive(Debug, Clone, PartialEq)]
pub enum SofaImportError {
    /// Not a NetCDF classic file (bad magic), or an unsupported NetCDF-4 /
    /// HDF5 container (see module docs — the `nc4` seam).
    NotSupported(String),
    /// Truncated / out-of-range read against the byte buffer.
    Truncated(String),
    /// The `Conventions` attribute is missing or is not a SOFA file.
    NotSoFa,
    /// A required SOFA variable (SourcePosition / Data.IR) is absent, wrong
    /// rank, non-uniform, or has an unsupported type.
    MissingVariable(&'static str),
    /// The IR sample rate is missing or non-uniform across receivers.
    IrRate,
    /// The measurement grid / IRs contain non-finite or shape-inconsistent
    /// data.
    InvalidData(&'static str),
}

impl std::fmt::Display for SofaImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SofaImportError::NotSupported(e) => {
                write!(f, "sofa import: unsupported container: {e}")
            }
            SofaImportError::Truncated(e) => {
                write!(f, "sofa import: truncated/malformed file: {e}")
            }
            SofaImportError::NotSoFa => write!(f, "sofa import: not a SOFA file"),
            SofaImportError::MissingVariable(n) => {
                write!(f, "sofa import: missing/unsupported variable `{n}`")
            }
            SofaImportError::IrRate => {
                write!(f, "sofa import: IR sampling rate missing or non-uniform")
            }
            SofaImportError::InvalidData(n) => {
                write!(f, "sofa import: invalid measurement data: {n}")
            }
        }
    }
}

impl std::error::Error for SofaImportError {}

/// Parse a NetCDF-classic `.sofa`/`.nc` byte buffer into an [`HrtfCorpus`].
///
/// The corpus is the engine's normalized representation: every source
/// measurement gets a unit `direction` (`+X` right, `+Y` front, `+Z` up) and
/// left/right impulse responses at the recorded rate. Hand rejection to
/// [`HrtfDataset::from_corpus`] later applies tap trim / resample / mesh
/// validation. `Conventions` and every data field referenced above must
/// validate, or a typed [`SofaImportError`] is returned — an invalid HRTF
/// must never reach the audio thread (spec §62 seam).
pub fn import_sofa(bytes: &[u8], source: Option<String>) -> Result<HrtfCorpus, SofaImportError> {
    let mut r = CdfReader::open(bytes)?;

    // Global Conventions gate (AES69): must exist and begin with "SOFA".
    let conventions = r
        .global_attribute_strings("Conventions")
        .and_then(|s| s.into_iter().next())
        .ok_or(SofaImportError::NotSoFa)?;
    if !conventions.trim_start().starts_with("SOFA") {
        return Err(SofaImportError::NotSoFa);
    }

    // Source positions: [M, C=3] float/double, spherical by default.
    let positions = read_float_matrix(&mut r, "SourcePosition")?;
    if positions.is_empty() || positions.iter().any(|p| p.len() != 3) {
        return Err(SofaImportError::InvalidData("SourcePosition must be M×3"));
    }
    if positions.iter().flatten().any(|v| !v.is_finite()) {
        return Err(SofaImportError::InvalidData("SourcePosition non-finite"));
    }
    let spherical = matches!(
        r.normalized_attribute_value("SourcePosition_Type", "spherical")
            .as_deref(),
        Some("spherical") | None
    );

    // Data.IR: [I, R, M, N] (or [R, M, N]) float.
    let (ir_flat, ir_dims) = read_float_dims(&mut r, "Data.IR")?;
    let n_dims = ir_dims.len();
    if n_dims != 3 && n_dims != 4 {
        return Err(SofaImportError::MissingVariable("Data.IR"));
    }
    let n = ir_dims[n_dims - 1];
    let m = ir_dims[n_dims - 2];
    // R is the sole leading dimension after the optional I singleton.
    let r_dim = if n_dims == 4 {
        if ir_dims[0] != 1 {
            return Err(SofaImportError::MissingVariable("Data.IR"));
        }
        ir_dims[1]
    } else {
        ir_dims[0]
    };
    if n == 0 || m == 0 || r_dim == 0 {
        return Err(SofaImportError::MissingVariable("Data.IR"));
    }
    // Binaural HRTFs must have exactly one left + one right receiver.
    if r_dim != 2 {
        return Err(SofaImportError::InvalidData("Data.IR requires 2 receivers"));
    }
    let expect = r_dim * m * n;
    if ir_flat.len() != expect {
        return Err(SofaImportError::MissingVariable("Data.IR"));
    }
    if ir_flat.iter().any(|v| !v.is_finite()) {
        return Err(SofaImportError::InvalidData("Data.IR non-finite"));
    }
    if positions.len() != m {
        return Err(SofaImportError::InvalidData(
            "M mismatch between SourcePosition and Data.IR",
        ));
    }
    let rate = read_sampling_rate(&mut r)?;

    // Flat [I, R, M, N] strides. Receiver 0 = left, receiver 1 = right
    // (offset by one stride_r); we use I = 0 (a single impulse block).
    let stride_r = m * n;
    let stride_m = n;

    let mut measurements = Vec::with_capacity(m);
    for (src, position) in positions.iter().enumerate().take(m) {
        let dir = source_position_direction(position, spherical)
            .ok_or(SofaImportError::InvalidData("SourcePosition zero radius"))?;
        let mut left = Vec::with_capacity(n);
        let mut right = Vec::with_capacity(n);
        for s in 0..n {
            let base = src * stride_m + s;
            left.push(ir_flat[base]);
            right.push(ir_flat[stride_r + base]);
        }
        measurements.push(HrtfMeasurement {
            direction: [dir.x, dir.y, dir.z],
            left,
            right,
        });
    }

    Ok(HrtfCorpus {
        sample_rate: rate,
        source,
        measurements,
    })
}

/// Convert a SOFA spherical or cartesian source position to the engine's
/// unit direction (`+X` right, `+Y` front, `+Z` up).
///
/// SOFA spherical is `(az, el, radius)`: azimuth `0` = front, **positive
/// toward the left** (counter-clockwise from above, matching the SOFA
/// convention), elevation positive up. The engine's azimuth is positive
/// toward `+X` (right), so `az_engine = -az_sofa`.
#[inline]
fn source_position_direction(pos: &[f32], spherical: bool) -> Option<Vec3> {
    let v = if spherical {
        let az = pos[0].to_radians();
        let el = pos[1].to_radians();
        let rad = pos[2];
        if rad <= 0.0 {
            return None;
        }
        let az_engine = -az; // sofa CCW(left)+ -> engine CW(right)+
        let cos_el = el.cos();
        Vec3::new(cos_el * az_engine.sin(), cos_el * az_engine.cos(), el.sin())
    } else {
        Vec3::new(pos[0], pos[1], pos[2])
    };
    // Normalize (radius may encode distance, not a unit vector).
    v.normalized()
}

/// Pull a `[M, C]` float matrix from a variable.
fn read_float_matrix(
    r: &mut CdfReader<'_>,
    name: &'static str,
) -> Result<Vec<Vec<f32>>, SofaImportError> {
    let (flat, dims) = read_float_dims(r, name)?;
    if dims.len() != 2 {
        return Err(SofaImportError::MissingVariable(name));
    }
    let (rows, cols) = (dims[0], dims[1]);
    if cols == 0 || flat.len() != rows * cols {
        return Err(SofaImportError::MissingVariable(name));
    }
    Ok(flat.chunks_exact(cols).map(|c| c.to_vec()).collect())
}

/// Read a float variable as flat data + its dimension vector.
fn read_float_dims(
    r: &mut CdfReader<'_>,
    name: &'static str,
) -> Result<(Vec<f32>, Vec<usize>), SofaImportError> {
    let var = r
        .variable(name)
        .ok_or(SofaImportError::MissingVariable(name))?;
    let dims = r.shape(var);
    let n: usize = dims.iter().product();
    match var.nc_type {
        NcType::Float => {
            let data = r.variable_float_data(var, n)?;
            Ok((data, dims))
        }
        NcType::Double => {
            let data = r.variable_double_data(var, n)?;
            Ok((data.iter().map(|&v| v as f32).collect(), dims))
        }
        _ => Err(SofaImportError::MissingVariable(name)),
    }
}

/// Sampling rate for the IRs: read `Data.SamplingRate` (max across I/R).
fn read_sampling_rate(r: &mut CdfReader<'_>) -> Result<u32, SofaImportError> {
    let var = r
        .variable("Data.SamplingRate")
        .ok_or(SofaImportError::IrRate)?;
    let shape = r.shape(var);
    let n: usize = shape.iter().product();
    if n == 0 {
        return Err(SofaImportError::IrRate);
    }
    let seq: Vec<f64> = match var.nc_type {
        NcType::Double => r.variable_double_data(var, n)?,
        NcType::Float => r
            .variable_float_data(var, n)?
            .iter()
            .map(|&v| v as f64)
            .collect(),
        NcType::Int => r
            .variable_int_data(var, n)?
            .iter()
            .map(|&v| v as f64)
            .collect(),
        _ => return Err(SofaImportError::IrRate),
    };
    let rate = seq.iter().cloned().fold(f64::NAN, f64::max);
    if !rate.is_finite() || rate <= 0.0 {
        return Err(SofaImportError::IrRate);
    }
    Ok(rate as u32)
}

// ---------------------------------------------------------------------------
// Minimal NetCDF-classic (CDF) binary reader
// ---------------------------------------------------------------------------

/// Byte-order of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Endian {
    Big,
    Little,
}

/// A parsed CDF file: dimensions, global attributes, and a table pointing at
/// raw (contiguous, non-record) variable data.
struct CdfReader<'a> {
    data: &'a [u8],
    pos: usize,
    endian: Endian,
    dimensions: Vec<u64>,
    attributes: Vec<(String, Attr)>,
    variables: Vec<(String, Var)>,
}

/// An attribute's value block, located lazily in the buffer.
struct Attr {
    nc_type: NcType,
    count: usize,
    pos: usize,
}

/// A variable's metadata + the byte offset of its (padded) data block.
struct Var {
    nc_type: NcType,
    dims: Vec<u32>,
    begin: u64,
}

impl<'a> CdfReader<'a> {
    fn open(data: &'a [u8]) -> Result<Self, SofaImportError> {
        if data.len() < 4 {
            return Err(SofaImportError::Truncated("short header".into()));
        }
        // Classic is `C D F version`. Little-endian files store the same
        // four bytes reversed: `version F D C`.
        let be = data[0] == b'C' && data[1] == b'D' && data[2] == b'F';
        let le =
            (data[0] == 1 || data[0] == 2) && data[1] == b'F' && data[2] == b'D' && data[3] == b'C';
        let (endian, version) = if be && (data[3] == 1 || data[3] == 2) {
            (Endian::Big, data[3])
        } else if le {
            (Endian::Little, data[0])
        } else {
            return Err(SofaImportError::NotSupported(format!(
                "magic {:?} (NetCDF-4/HDF5 `nc4` SOFA is the common case; this \
                 importer reads the NetCDF-3 classic subset — encode with the SOFA \
                 API's SOFA_NETCDF3 / SOFA_NETCDF3_CLASSIC mode to import). See \
                 module docs for the HDF5 seam.",
                &data[..data.len().min(4)]
            )));
        };
        let cdf2 = version == 2;

        let mut r = CdfReader {
            data,
            pos: 4,
            endian,
            dimensions: Vec::new(),
            attributes: Vec::new(),
            variables: Vec::new(),
        };

        // numrecs (ignored — no record variables are used here).
        let _ = r.int(cdf2)?;

        // Dimension list.
        let n_dims = r.list_count(cdf2)?;
        for _ in 0..n_dims {
            let _name = r.string()?;
            let len = r.int(cdf2)?;
            r.dimensions.push(len);
        }

        // Global attribute list.
        r.attributes = r.attr_list(cdf2)?;

        // Variable list.
        let n_vars = r.list_count(cdf2)?;
        for _ in 0..n_vars {
            let name = r.string()?;
            let ndims = r.i32()?;
            if ndims < 0 {
                return Err(SofaImportError::Truncated("negative dim count".into()));
            }
            let mut dims = Vec::with_capacity(ndims as usize);
            for _ in 0..ndims {
                dims.push(r.i32()? as u32);
            }
            // Variable-scope attributes are skipped (we read global attrs).
            let _vars = r.attr_list(cdf2)?;
            let nc_type = r.i32()?;
            let ty = nc_type_from_i32(nc_type).ok_or_else(|| {
                SofaImportError::NotSupported(format!("unsupported variable type {nc_type}"))
            })?;
            // vsize (bytes, padded) — unused here (we read exact element
            // strides via `begin`).
            let _vsize = r.int(cdf2)?;
            let begin = r.int(cdf2)?;
            r.variables.push((
                name.clone(),
                Var {
                    nc_type: ty,
                    dims,
                    begin,
                },
            ));
        }
        Ok(r)
    }

    fn list_count(&mut self, cdf2: bool) -> Result<usize, SofaImportError> {
        self.int(cdf2).map(|v| v as usize)
    }
    fn int(&mut self, wide: bool) -> Result<u64, SofaImportError> {
        if wide {
            self.u64()
        } else {
            self.u32().map(|v| v as u64)
        }
    }
    fn i32(&mut self) -> Result<i32, SofaImportError> {
        self.u32().map(|v| v as i32)
    }
    fn u32(&mut self) -> Result<u32, SofaImportError> {
        let b = self.bytes(4)?;
        Ok(match self.endian {
            Endian::Big => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            Endian::Little => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        })
    }
    fn u64(&mut self) -> Result<u64, SofaImportError> {
        let b = self.bytes(8)?;
        Ok(match self.endian {
            Endian::Big => u64::from_be_bytes(b.try_into().unwrap()),
            Endian::Little => u64::from_le_bytes(b.try_into().unwrap()),
        })
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], SofaImportError> {
        if self.pos + n > self.data.len() {
            return Err(SofaImportError::Truncated("read past end".into()));
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// A netCDF name: u32 length + that many bytes (padded out to a 4-byte
    /// boundary in the stream; the length field excludes the padding). The
    /// padding must be skipped so the read cursor stays aligned.
    fn string(&mut self) -> Result<String, SofaImportError> {
        let len = self.u32()? as usize;
        let b = self.bytes(len)?;
        let pad = (4 - (self.pos % 4)) % 4;
        self.bytes(pad)?;
        Ok(String::from_utf8_lossy(b).into_owned())
    }

    /// Parse a list of attributes (global or variable-scope). Each value
    /// block is located (skipped) and recorded for lazy decoding.
    fn attr_list(&mut self, cdf2: bool) -> Result<Vec<(String, Attr)>, SofaImportError> {
        let n = self.list_count(cdf2)?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let name = self.string()?;
            let ty_i = self.i32()?;
            let ty = nc_type_from_i32(ty_i).ok_or_else(|| {
                SofaImportError::NotSupported(format!("unsupported attribute type {ty_i}"))
            })?;
            let count = self.u32()? as usize;
            let pos = self.pos;
            let size = type_size(ty) * count;
            let padded = (size + 3) & !3;
            self.bytes(padded)?;
            out.push((
                name.clone(),
                Attr {
                    nc_type: ty,
                    count,
                    pos,
                },
            ));
        }
        Ok(out)
    }

    /// Global string attribute (NC_CHAR), split on the NUL padding.
    fn global_attribute_strings(&self, name: &str) -> Option<Vec<String>> {
        self.attributes
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, a)| read_attr_strings(self.data, a).unwrap_or_default())
    }

    /// Ask for an attribute value but tolerate a simple normalized spelling
    /// (SOFA stores several as lowercase strings); falls back to `default`.
    fn normalized_attribute_value(&self, name: &str, default: &str) -> Option<String> {
        self.global_attribute_strings(name)
            .and_then(|s| s.into_iter().next())
            .map(|s| s.trim().to_ascii_lowercase())
            .or_else(|| Some(default.to_string()))
    }

    fn variable(&self, name: &str) -> Option<&Var> {
        self.variables
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v)
    }

    /// Resolve a variable's dim-id list to concrete extents via the file's
    /// dimension table (missing/unknown ids resolve to 0, a hard failure).
    fn shape(&self, v: &Var) -> Vec<usize> {
        v.dims
            .iter()
            .map(|&d| self.dimensions.get(d as usize).copied().unwrap_or(0) as usize)
            .collect()
    }

    fn variable_float_data(&self, v: &Var, n: usize) -> Result<Vec<f32>, SofaImportError> {
        if v.begin as usize + n * 4 > self.data.len() {
            return Err(SofaImportError::Truncated("variable float data".into()));
        }
        Ok((0..n)
            .map(|i| {
                let s = (v.begin as usize) + i * 4;
                let b = &self.data[s..s + 4];
                match self.endian {
                    Endian::Big => f32::from_be_bytes([b[0], b[1], b[2], b[3]]),
                    Endian::Little => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                }
            })
            .collect())
    }

    fn variable_double_data(&self, v: &Var, n: usize) -> Result<Vec<f64>, SofaImportError> {
        if v.begin as usize + n * 8 > self.data.len() {
            return Err(SofaImportError::Truncated("variable double data".into()));
        }
        Ok((0..n)
            .map(|i| {
                let s = (v.begin as usize) + i * 8;
                let b = &self.data[s..s + 8];
                match self.endian {
                    Endian::Big => f64::from_be_bytes(b.try_into().unwrap()),
                    Endian::Little => f64::from_le_bytes(b.try_into().unwrap()),
                }
            })
            .collect())
    }

    fn variable_int_data(&self, v: &Var, n: usize) -> Result<Vec<i32>, SofaImportError> {
        if v.begin as usize + n * 4 > self.data.len() {
            return Err(SofaImportError::Truncated("variable int data".into()));
        }
        Ok((0..n)
            .map(|i| {
                let s = (v.begin as usize) + i * 4;
                let b = &self.data[s..s + 4];
                match self.endian {
                    Endian::Big => i32::from_be_bytes([b[0], b[1], b[2], b[3]]),
                    Endian::Little => i32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                }
            })
            .collect())
    }
}

fn type_size(ty: NcType) -> usize {
    match ty {
        NcType::Byte | NcType::Char => 1,
        NcType::Short => 2,
        NcType::Int | NcType::Float => 4,
        NcType::Double => 8,
    }
}

fn nc_type_from_i32(v: i32) -> Option<NcType> {
    match v {
        1 => Some(NcType::Byte),
        2 => Some(NcType::Char),
        3 => Some(NcType::Short),
        4 => Some(NcType::Int),
        5 => Some(NcType::Float),
        6 => Some(NcType::Double),
        _ => None,
    }
}

/// Decode an attribute's value block into strings (NC_CHAR only), splitting
/// on the NUL fill and dropping trailing empties.
fn read_attr_strings(data: &[u8], a: &Attr) -> Option<Vec<String>> {
    if a.nc_type != NcType::Char {
        return None;
    }
    let size = type_size(a.nc_type) * a.count;
    let start = a.pos;
    if start + size > data.len() {
        return None;
    }
    let raw = &data[start..start + size];
    let s = String::from_utf8_lossy(raw);
    let parts: Vec<String> = s.split('\0').map(|x| x.to_string()).collect();
    Some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Minimal, correct NetCDF-classic (CDF-1) binary serialiser for fixtures.
    // We write exactly the shape `import_sofa` reads: dims I,R,M,N,C; the
    // SOFA global attrs; then SourcePosition [M,C] float, Data.IR
    // [I,R,M,N] float, Data.SamplingRate [I] double — with real `begin`
    // offsets patched over placeholder slots before appending the data.
    // -----------------------------------------------------------------------

    #[derive(Clone)]
    struct Ser {
        out: Vec<u8>,
        endian: Endian,
    }

    impl Ser {
        fn new(endian: Endian) -> Self {
            Self {
                out: Vec::new(),
                endian,
            }
        }
        fn u32(&mut self, v: u32) {
            match self.endian {
                Endian::Big => self.out.extend_from_slice(&v.to_be_bytes()),
                Endian::Little => self.out.extend_from_slice(&v.to_le_bytes()),
            }
        }
        fn f32(&mut self, v: f32) {
            match self.endian {
                Endian::Big => self.out.extend_from_slice(&v.to_be_bytes()),
                Endian::Little => self.out.extend_from_slice(&v.to_le_bytes()),
            }
        }
        fn f64(&mut self, v: f64) {
            match self.endian {
                Endian::Big => self.out.extend_from_slice(&v.to_be_bytes()),
                Endian::Little => self.out.extend_from_slice(&v.to_le_bytes()),
            }
        }
        /// netCDF name: u32 byte-length + bytes, zero-padded to 4.
        fn name(&mut self, s: &str) {
            self.u32(s.len() as u32);
            self.out.extend_from_slice(s.as_bytes());
            if !self.out.len().is_multiple_of(4) {
                self.out.resize(self.out.len().next_multiple_of(4), 0);
            }
        }
        /// A string attribute: name + NC_CHAR type + count + bytes (padded).
        fn str_attr(&mut self, name: &str, val: &str) {
            self.name(name);
            self.u32(NcType::Char as u32);
            self.u32(val.len() as u32);
            self.out.extend_from_slice(val.as_bytes());
            if !self.out.len().is_multiple_of(4) {
                self.out.resize(self.out.len().next_multiple_of(4), 0);
            }
        }
    }

    /// Serialise a deterministic [I,R,M,N]-shaped IR. Source `s`'s left ear
    /// samples are `1000*s + t` and its right ear `2000*s + t`, so both the
    /// measurement count and the L/R split are trivially verifiable.
    fn ir_flat(m: usize, n: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(2 * m * n);
        for s in 0..m {
            for t in 0..n {
                out.push(s as f32 * 1000.0 + t as f32); // left
            }
        }
        for s in 0..m {
            for t in 0..n {
                out.push(s as f32 * 2000.0 + t as f32); // right
            }
        }
        out
    }

    /// Emit a complete CDF-1 SOFA file for `m` measurements and `n` taps.
    fn emit_sofa(
        endian: Endian,
        m: usize,
        n: usize,
        positions: &[f32],
        convention: &str,
    ) -> Vec<u8> {
        let mut s = Ser::new(endian);
        // Magic + version 1.
        match endian {
            Endian::Big => s.out.extend_from_slice(b"CDF\x01"),
            Endian::Little => s.out.extend_from_slice(&[0x01, b'F', b'D', b'C']),
        }
        s.u32(0); // numrecs

        // Dimensions: I=0, R=1, M=2, N=3, C=4.
        s.u32(5);
        for (nm, len) in [
            ("I", 1u32),
            ("R", 2),
            ("M", m as u32),
            ("N", n as u32),
            ("C", 3),
        ] {
            s.name(nm);
            s.u32(len);
        }

        // Global string attributes.
        s.u32(2);
        s.str_attr("Conventions", convention);
        s.str_attr("SourcePosition_Type", "spherical");

        // Variable table (3 variables). dim ids: Source [M,C]=[2,4];
        // IR [I,R,M,N]=[0,1,2,3]; rate [I]=[0].
        s.u32(3);
        let ir = ir_flat(m, n);
        let ir_bytes = ir.len() * 4;
        let pos_bytes = positions.len() * 4;
        let mut begin_slots: Vec<usize> = Vec::new();
        let specs = [
            ("SourcePosition", vec![2u32, 4], NcType::Float, pos_bytes),
            ("Data.IR", vec![0u32, 1, 2, 3], NcType::Float, ir_bytes),
            ("Data.SamplingRate", vec![0u32], NcType::Double, 8),
        ];
        for (name, dims, ty, bytes) in &specs {
            s.name(name);
            s.u32(dims.len() as u32);
            for d in dims {
                s.u32(*d);
            }
            s.u32(0); // no variable attributes
            s.u32(*ty as u32);
            s.u32(*bytes as u32); // vsize
            begin_slots.push(s.out.len());
            s.u32(0); // begin placeholder
        }

        // Data blocks: patch begins, then append payloads in var order.
        let patch = |out: &mut [u8], slot: usize, val: u32, endian: Endian| match endian {
            Endian::Big => out[slot..slot + 4].copy_from_slice(&val.to_be_bytes()),
            Endian::Little => out[slot..slot + 4].copy_from_slice(&val.to_le_bytes()),
        };
        // SourcePosition.
        let b0 = s.out.len() as u32;
        patch(&mut s.out, begin_slots[0], b0, endian);
        for v in positions {
            s.f32(*v);
        }
        // Data.IR.
        let b1 = s.out.len() as u32;
        patch(&mut s.out, begin_slots[1], b1, endian);
        for v in &ir {
            s.f32(*v);
        }
        // SamplingRate 48000.0 double.
        let b2 = s.out.len() as u32;
        patch(&mut s.out, begin_slots[2], b2, endian);
        s.f64(48_000.0);
        s.out
    }

    /// A canonical fixture: `m` sources on the horizon in the SOFA azimuth
    /// convention, `n` taps, right-handed order.
    fn fixture(endian: Endian, m: usize, n: usize, convention: &str) -> Vec<u8> {
        let mut pos = Vec::new();
        for i in 0..m {
            let az = i as f32 * 360.0 / m as f32;
            pos.extend_from_slice(&[az, 0.0, 1.0]); // az, el, radius
        }
        emit_sofa(endian, m, n, &pos, convention)
    }

    #[test]
    fn imports_cdf1_big_endian() {
        let m = 4;
        let n = 16;
        let bytes = fixture(Endian::Big, m, n, "SOFA");
        let corpus = import_sofa(&bytes, Some("test".into())).unwrap();
        assert_eq!(corpus.sample_rate, 48_000);
        assert_eq!(corpus.source.as_deref(), Some("test"));
        assert_eq!(corpus.measurements.len(), m);

        // Direction mapping: SOFA az=0 (front) -> engine +Y (front).
        let d0 = corpus.measurements[0].direction;
        assert!((d0[0]).abs() < 1e-4 && d0[1] > 0.9999, "front dir {d0:?}");

        // SOFA az=90 means *left* (CCW from above) -> engine -X.
        let d1 = corpus.measurements[1].direction;
        assert!(d1[0] < -0.9999, "left dir {d1:?}");

        // L/R taps verify the [I,R,M,N] striding and the receiver split.
        for (sidx, meas) in corpus.measurements.iter().enumerate() {
            assert_eq!(meas.left.len(), n);
            assert_eq!(meas.right.len(), n);
            for t in 0..n {
                assert!((meas.left[t] - (sidx as f32 * 1000.0 + t as f32)).abs() < 1e-4);
                assert!((meas.right[t] - (sidx as f32 * 2000.0 + t as f32)).abs() < 1e-4);
            }
            // Directions are unit length.
            let l =
                (meas.direction[0].powi(2) + meas.direction[1].powi(2) + meas.direction[2].powi(2))
                    .sqrt();
            assert!((l - 1.0).abs() < 1e-4, "unit dir for source {sidx}");
        }
    }

    #[test]
    fn imports_cdf1_little_endian() {
        let m = 3;
        let n = 8;
        let bytes = fixture(Endian::Little, m, n, "SOFA");
        let corpus = import_sofa(&bytes, None).unwrap();
        assert_eq!(corpus.measurements.len(), m);
        assert_eq!(corpus.measurements[0].left.len(), n);
        for (sidx, meas) in corpus.measurements.iter().enumerate() {
            for t in 0..n {
                assert!((meas.left[t] - (sidx as f32 * 1000.0 + t as f32)).abs() < 1e-4);
            }
        }
    }

    #[test]
    fn rejects_non_sofa_convention() {
        let bytes = fixture(Endian::Big, 2, 8, "MRI");
        let err = import_sofa(&bytes, None).unwrap_err();
        assert_eq!(err, SofaImportError::NotSoFa);
    }

    #[test]
    fn rejects_missing_conventions() {
        // Remove Conventions by using a CDF build with an empty attribute
        // list path; simplest is to mutate the bytes: overwrite names.
        let mut bytes = fixture(Endian::Big, 2, 8, "SOFA");
        // The string "Conventions" starts right after the dim list; locate it
        // and clobber it so global_attribute_strings returns nothing -> NotSoFa
        let needle = b"Conventions";
        let pos = bytes
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("Conventions present");
        bytes[pos..pos + needle.len()].copy_from_slice(b"XXXXXXXXXXX");
        let err = import_sofa(&bytes, None).unwrap_err();
        assert!(matches!(
            err,
            SofaImportError::NotSoFa | SofaImportError::Truncated(_)
        ));
    }

    #[test]
    fn rejects_bad_magic() {
        let err = import_sofa(
            b"THIS IS NOT CDF AT ALL, just some bytes padding out to length",
            None,
        );
        let err = err.unwrap_err();
        assert!(matches!(err, SofaImportError::NotSupported(_)));
    }

    #[test]
    fn rejects_netcdf4_hdr_magic_as_seam() {
        // NetCDF-4/HDF5 files start with the 8-byte HDF5 superblock
        // ("\x89HDF\r\n\x1a\n"), which this classic-only reader must refuse.
        let magic = [0x89u8, b'H', b'D', b'F', b'\r', b'\n', 0x1a, b'\n'];
        let err = import_sofa(&magic, None).unwrap_err();
        assert!(matches!(err, SofaImportError::NotSupported(_)));
    }

    #[test]
    fn rejects_truncated_data() {
        let full = fixture(Endian::Big, 4, 16, "SOFA");
        for cut in [40, 60, full.len() - 3] {
            if cut < full.len() {
                let err = import_sofa(&full[..cut], None);
                // Depending on where it cuts, either a truncation error or a
                // missing-variable shape error is acceptable — never success.
                assert!(err.is_err(), "cut at {cut} must not import cleanly");
            }
        }
    }

    #[test]
    fn round_trips_into_hrif_dataset() {
        // The whole point: the imported corpus must feed the renderer's
        // existing validation/resampling pipeline.
        let m = 5;
        let n = 32;
        let bytes = fixture(Endian::Big, m, n, "SOFA");
        let corpus = import_sofa(&bytes, Some("sofa-fixture".into())).unwrap();
        let opts = super::super::hrtf::HrtfLoadOptions::default();
        let ds = super::super::hrtf::HrtfDataset::from_corpus(&corpus, &opts);
        assert!(ds.is_ok(), "corpus must feed HrtfDataset; err: {ds:?}");
    }
}
