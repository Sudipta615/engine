//! Versioned reference-vector registry.
//!
//! A reference vector is the *spec* a component is measured against: a stable
//! `id`, a monotone `version`, and the [`MetricSpec`] expectations for each
//! metric the component opts into. Its identity is a **content address** — a
//! SHA-256 (via the [`crate::dsp::aelog::cache`] substrate, the same hashing
//! that names aelog golden captures) over the canonical expectations + version.
//! A changed expectation changes the address, so a stale or drifting vector is
//! always detectable from the address alone.
//!
//! The registry is versioned and serde-serializable, so the set of reference
//! vectors an engine build evaluates is itself a versioned, shareable artifact.
//! Control/offline path only — never on the audio thread.

use crate::dsp::aelog::cache::{sha256, to_hex};
use serde::{Deserialize, Serialize};

use super::{Expect, MetricKind};

/// One metric's expectation within a reference vector.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MetricSpec {
    pub metric: MetricKind,
    pub expect: Expect,
}

impl MetricSpec {
    pub fn new(metric: MetricKind, expect: Expect) -> Self {
        Self { metric, expect }
    }
}

/// A versioned, content-addressed reference vector for one DSP/spatial
/// component.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceVector {
    pub id: String,
    /// Monotone per-`id` schema/behavior version. A breaking change to a
    /// metric or its expectation bumps this.
    pub version: u32,
    /// Engine crate version that authored the vector (provenance; *not* part
    /// of the content address — behavior should be stable across builds).
    pub engine_version: String,
    pub checks: Vec<MetricSpec>,
    /// SHA-256 over the canonical expectations + version. Do not edit.
    pub address: String,
}

impl ReferenceVector {
    pub fn new(
        id: impl Into<String>,
        version: u32,
        engine_version: impl Into<String>,
        checks: Vec<MetricSpec>,
    ) -> Self {
        let id = id.into();
        let engine_version = engine_version.into();
        let address = vector_address(&checks, version);
        Self {
            id,
            version,
            engine_version,
            checks,
            address,
        }
    }

    /// `"{id}@{version}"` — the compact reference a report cites.
    pub fn display_id(&self) -> String {
        format!("{}@{}", self.id, self.version)
    }
}

/// Deterministic content address of a vector's expectations + version: a
/// SHA-256 (hex) over the canonical JSON. Identical specs + version on any
/// machine address identically.
pub fn vector_address(checks: &[MetricSpec], version: u32) -> String {
    let key = serde_json::json!({ "version": version, "checks": checks });
    let bytes = serde_json::to_vec(&key).unwrap_or_default();
    to_hex(&sha256(&bytes))
}

/// The versioned, serializable set of reference vectors for an engine build.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ReferenceVectorRegistry {
    pub format_version: u32,
    pub vectors: Vec<ReferenceVector>,
}

impl ReferenceVectorRegistry {
    /// Schema version of [`ReferenceVectorRegistry`] itself. Bump on any
    /// breaking layout change to the registry (mirrors `AELOG_VERSION`).
    pub const FORMAT_VERSION: u32 = 1;

    /// The empty registry at the current `format_version`.
    pub fn new() -> Self {
        Self {
            format_version: Self::FORMAT_VERSION,
            vectors: Vec::new(),
        }
    }

    pub fn get(&self, id: &str) -> Option<&ReferenceVector> {
        self.vectors.iter().find(|v| v.id == id)
    }

    /// Register (or update) a vector by `id`. Registering an identical spec
    /// keeps the existing version; a changed spec bumps to `existing + 1` and
    /// recomputes the address.
    pub fn register(&mut self, candidate: ReferenceVector) {
        match self.vectors.iter_mut().find(|v| v.id == candidate.id) {
            Some(existing) => {
                let mut entry = candidate;
                if entry.version <= existing.version {
                    // Retain the caller-facing version; only bump if the spec
                    // actually changed.
                    if entry.checks != existing.checks {
                        entry.version = existing.version + 1;
                        entry.address = vector_address(&entry.checks, entry.version);
                    } else {
                        entry.version = existing.version;
                        entry.address = existing.address.clone();
                    }
                }
                *existing = entry;
            }
            None => self.vectors.push(candidate),
        }
    }

    /// The canonical registry for the current engine build: registers every
    /// suite's reference vector so reports and CI compare against a stable,
    /// versioned spec. Deterministic (fixed versions).
    pub fn build() -> Self {
        let mut r = Self::new();
        let ev = env!("CARGO_PKG_VERSION").to_string();
        r.register(super::suites::def_pipeline(ev.clone()));
        r.register(super::suites::def_parametric_eq(ev.clone()));
        r.register(super::suites::def_limiter(ev.clone()));
        r.register(super::suites::def_resampler(ev.clone()));
        r.register(super::suites::def_binaural(ev.clone()));
        r.register(super::suites::def_loudness(ev.clone()));
        r.register(super::suites::def_convolution(ev.clone()));
        r.register(super::suites::def_channel_separation(ev.clone()));
        r.register(super::suites::def_hrtf(ev));
        r
    }

    /// Serialize the registry (compact JSON).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Deserialize a registry, rejecting a foreign `format_version`.
    pub fn from_json(json: &str) -> Result<Self, super::EvalError> {
        let reg: ReferenceVectorRegistry =
            serde_json::from_str(json).map_err(super::EvalError::Serialize)?;
        if reg.format_version != Self::FORMAT_VERSION {
            return Err(super::EvalError::RegistryVersion(reg.format_version));
        }
        Ok(reg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Vec<MetricSpec> {
        vec![MetricSpec::new(
            MetricKind::NoiseFloorDb,
            Expect::AtMost { max: -60.0 },
        )]
    }

    #[test]
    fn address_is_deterministic_and_sensitive_to_expectations_and_version() {
        let a = ReferenceVector::new("eq", 1, "test", spec());
        let b = ReferenceVector::new("eq", 1, "test", spec());
        assert_eq!(a, b);
        assert_eq!(a.address.len(), 64, "SHA-256 hex");

        // A changed expectation changes the address.
        let changed = ReferenceVector::new(
            "eq",
            1,
            "test",
            vec![MetricSpec::new(
                MetricKind::NoiseFloorDb,
                Expect::AtMost { max: -70.0 },
            )],
        );
        assert_ne!(a.address, changed.address);

        // A version bump changes the address too.
        let bumped = ReferenceVector::new("eq", 2, "test", spec());
        assert_ne!(a.address, bumped.address);

        // The engine provenance is NOT in the address.
        let other_build = ReferenceVector::new("eq", 1, "99.0.0", spec());
        assert_eq!(a.address, other_build.address);
    }

    #[test]
    fn register_bumps_version_only_when_spec_changes() {
        let ev = "test".to_string();
        let mut r = ReferenceVectorRegistry::new();
        r.register(ReferenceVector::new("x", 1, ev.clone(), spec()));
        assert_eq!(r.get("x").unwrap().version, 1);

        // Identical spec → same version.
        r.register(ReferenceVector::new("x", 1, ev.clone(), spec()));
        assert_eq!(r.get("x").unwrap().version, 1);

        // Changed spec → bumped version + new address.
        r.register(ReferenceVector::new(
            "x",
            1,
            ev,
            vec![MetricSpec::new(
                MetricKind::NoiseFloorDb,
                Expect::AtMost { max: -90.0 },
            )],
        ));
        let x = r.get("x").unwrap();
        assert_eq!(x.version, 2);
        assert_eq!(x.address, vector_address(&x.checks, 2));
    }

    #[test]
    fn registry_round_trips_and_rejects_foreign_format() {
        let mut r = ReferenceVectorRegistry::new();
        r.register(ReferenceVector::new("eq", 1, "test", spec()));
        let json = r.to_json().unwrap();
        let back = ReferenceVectorRegistry::from_json(&json).unwrap();
        assert_eq!(back.vectors.len(), 1);
        assert_eq!(back.get("eq").unwrap().display_id(), "eq@1");

        let mut foreign = back.clone();
        foreign.format_version = 99;
        let j = foreign.to_json().unwrap();
        assert!(matches!(
            ReferenceVectorRegistry::from_json(&j),
            Err(crate::eval::EvalError::RegistryVersion(99))
        ));
    }

    #[test]
    fn build_registers_the_canonical_nine_vectors() {
        let r = ReferenceVectorRegistry::build();
        for id in [
            "dsp-pipeline",
            "parametric-eq",
            "limiter",
            "resampler",
            "binaural",
            "loudness",
            "convolution",
            "channel-separation",
            "hrtf",
        ] {
            assert!(r.get(id).is_some(), "missing vector for {id}");
        }
        assert_eq!(r.format_version, ReferenceVectorRegistry::FORMAT_VERSION);
    }
}
