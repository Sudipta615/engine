//! Versioned persistence for engine configuration (migration framework).
//!
//! Engine configuration has historically been stored as a bare serde JSON
//! object: new fields were added with `#[serde(default)]`, so an old file
//! silently loaded with the new defaults — migration was implicit and
//! unversioned. This module makes that contract explicit so a host can
//! persist state across releases and be told (and shown) what was upgraded.
//!
//! # Design
//!
//! A stored payload is wrapped in a versioned envelope:
//!
//! ```json
//! { "version": 1, "output_backend": "Auto", "mix_slots": 2, ... }
//! ```
//!
//! - `version` is the schema that produced the payload. It **defaults to the
//!   current version**, so a legacy (pre-versioning) payload — written by an
//!   older engine that serialized [`EngineConfig`] directly — reads in as
//!   already-current. That matches the historical guarantee: old files always
//!   load with per-field defaults.
//! - [`VersionedConfig::migrate`] is the forward path: it walks
//!   `payload.version .. CONFIG_VERSION` through [`migrate_step`] (identity
//!   today; the baseline schema is version 1 and the step table is where
//!   future versions append transforms).
//!
//! The envelope carries all of the existing config via `#[serde(flatten)]`,
//! so the **public [`EngineConfig`] type is unchanged** — hosts that build a
//! config programmatically and current on-disk payloads both keep working.
//! `EngineConfig::validate` remains the guard rail that rejects an invalid
//! or un-migratable payload after loading.
//!
//! Everything here is pure data transformation on the control path — no
//! audio thread is ever involved.

use serde::{Deserialize, Serialize};

use super::EngineConfig;

/// The current persisted schema version. Bump this when a forward migration
/// is added, and append the matching transform to [`migrate_step`].
pub const CONFIG_VERSION: u32 = 1;

/// Parse/load failure for a versioned config payload.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigLoadError {
    /// The JSON did not parse as a config object.
    Json(String),
}

impl std::fmt::Display for ConfigLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigLoadError::Json(e) => write!(f, "config load: {e}"),
        }
    }
}

impl std::error::Error for ConfigLoadError {}

/// A versioned envelope around [`EngineConfig`] for persistent storage.
///
/// `version` defaults to the current schema so a legacy unversioned payload
/// (an older engine's bare `EngineConfig` JSON) deserializes without error —
/// the established backward-compatibility guarantee. The config itself is
/// flattened so no extra nesting is introduced and no existing serializer /
/// consumer needs to change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VersionedConfig {
    /// The schema version the config was written under (defaults to current).
    #[serde(default = "current_version")]
    pub version: u32,
    #[serde(flatten)]
    pub config: EngineConfig,
}

impl VersionedConfig {
    /// Wrap a freshly-built config at the current schema version.
    pub fn new(config: EngineConfig) -> Self {
        Self {
            version: current_version(),
            config,
        }
    }

    /// Parse a persisted payload (optionally versioned). A legacy bare
    /// [`EngineConfig`] JSON parses identically — `version` takes its default.
    pub fn load(json: &str) -> Result<Self, ConfigLoadError> {
        serde_json::from_str(json).map_err(|e| ConfigLoadError::Json(e.to_string()))
    }

    /// Serialize to JSON (pretty, for human diffing) at the current version.
    pub fn save_pretty(&self) -> Result<String, ConfigLoadError> {
        serde_json::to_string_pretty(self).map_err(|e| ConfigLoadError::Json(e.to_string()))
    }

    /// Run the forward migrations from the payload's stored version up to
    /// [`CONFIG_VERSION`]. Idempotent: a payload already at the current
    /// version passes through unchanged. Returns the upgraded config.
    pub fn migrate(mut self) -> Self {
        while self.version < CONFIG_VERSION {
            self.config = migrate_step(self.version, self.config);
            self.version += 1;
        }
        self
    }
}

/// One forward migration step from schema `from_version` to `from_version + 1`.
///
/// Version 1 is the baseline (the current schema), so an attempt to migrate
/// from it is a no-op. Future schema changes append `2 => …`, `3 => …`, each
/// a deterministic, validated transform of [`EngineConfig`] (e.g. renaming a
/// field, splitting a knob, clamping a newly-bounded value). Unknown versions
/// are treated as identity so an excessively-new payload degrades to the
/// per-field defaults rather than erroring mid-migration.
pub fn migrate_step(from_version: u32, config: EngineConfig) -> EngineConfig {
    match from_version {
        // Baseline schema; no transform to reach version 1 (it already exists).
        1 => config,
        // A payload claiming a future schema: leave it be (serde defaults
        // already filled any structurally-absent new top-level keys).
        _ => config,
    }
}

#[inline]
fn current_version() -> u32 {
    CONFIG_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> EngineConfig {
        EngineConfig::default()
    }

    #[test]
    fn new_is_at_current_version_and_migrate_is_identity() {
        let vc = VersionedConfig::new(sample());
        assert_eq!(vc.version, CONFIG_VERSION);
        let upgraded = vc.migrate();
        assert_eq!(upgraded.version, CONFIG_VERSION);
        assert_eq!(upgraded.config, sample());
    }

    #[test]
    fn legacy_unversioned_payload_loads_as_current() {
        // A bare EngineConfig JSON has no `version` key — it must read in at
        // the current version (the historical no-op migration guarantee).
        let raw = serde_json::to_string(&sample()).unwrap();
        assert!(!raw.contains("\"version\""));
        let loaded = VersionedConfig::load(&raw).unwrap();
        assert_eq!(loaded.version, CONFIG_VERSION);
        assert_eq!(loaded.config, sample());
        assert!(loaded.migrate().config.validate().is_valid());
    }

    #[test]
    fn round_trips_through_saved_payload() {
        let vc = VersionedConfig::new(sample());
        let json = vc.save_pretty().unwrap();
        // The envelope serializes the version and a human-readable config.
        assert!(json.contains(&format!("\"version\": {CONFIG_VERSION}")));
        let back = VersionedConfig::load(&json).unwrap();
        assert_eq!(back.version, CONFIG_VERSION);
        assert_eq!(back.config, sample());
    }

    #[test]
    fn malformed_payload_is_a_typed_error() {
        assert!(matches!(
            VersionedConfig::load("{ not json "),
            Err(ConfigLoadError::Json(_))
        ));
        // A structurally valid JSON that is not a config object also errors.
        assert!(VersionedConfig::load("{\"version\": 1, \"oops\"}").is_err());
    }
}
