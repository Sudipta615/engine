//! Versioned state and preset serialization framework (§9.2, Item 18).
//!
//! Provides schema-versioned envelopes and automated forward-migration pipelines
//! for core engine, graph, DSP node, plugin, spatial scene, and output profile states.
//!
//! # Guarantee
//! Old valid configurations fail gracefully or migrate automatically (`v1 → v2 → v3`)
//! rather than silently corrupting state or crashing the engine.

use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};

use super::{AudioBackend, DsdOutput, EngineConfig, SpatialSceneConfig};

/// Canonical schema version for all persisted state envelopes.
pub const STATE_SCHEMA_VERSION: u32 = 1;

/// Engine version producing this schema.
pub const CURRENT_ENGINE_VERSION: &str = "5.3.0";

/// Errors encountered during state loading, validation, or schema migration.
#[derive(Debug, Clone, PartialEq)]
pub enum StateMigrationError {
    /// The stored schema version is newer than supported by this engine build.
    UnsupportedSchema { found: u32, max_supported: u32 },
    /// JSON syntax or deserialization failure.
    CorruptedJson(String),
    /// Required migration transform failed.
    MigrationFailed(String),
}

impl std::fmt::Display for StateMigrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateMigrationError::UnsupportedSchema { found, max_supported } => {
                write!(
                    f,
                    "unsupported state schema version {found} (maximum supported: {max_supported})"
                )
            }
            StateMigrationError::CorruptedJson(e) => write!(f, "corrupted state JSON: {e}"),
            StateMigrationError::MigrationFailed(msg) => write!(f, "state migration failed: {msg}"),
        }
    }
}

impl std::error::Error for StateMigrationError {}

/// Versioned envelope encapsulating persisted state payloads (§9.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VersionedEnvelope<T> {
    /// Stored schema version (defaults to current version for legacy payloads).
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Semantic engine release version string (e.g. "5.3.0").
    #[serde(default = "default_engine_version")]
    pub engine_version: String,
    /// Component-specific version (e.g. plugin or node format version).
    #[serde(default = "default_component_version")]
    pub component_version: u32,
    /// UNIX timestamp in seconds when the state was captured.
    #[serde(default)]
    pub timestamp_secs: u64,
    /// The actual state payload.
    pub state: T,
}

fn default_schema_version() -> u32 {
    STATE_SCHEMA_VERSION
}

fn default_engine_version() -> String {
    CURRENT_ENGINE_VERSION.to_string()
}

fn default_component_version() -> u32 {
    1
}

impl<T: Serialize + for<'de> Deserialize<'de>> VersionedEnvelope<T> {
    /// Create a new versioned envelope wrapping a current state payload.
    pub fn new(state: T, component_version: u32) -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            engine_version: CURRENT_ENGINE_VERSION.to_string(),
            component_version,
            timestamp_secs: 0,
            state,
        }
    }

    /// Serialize envelope to a pretty-printed JSON string.
    pub fn to_json_pretty(&self) -> Result<String, StateMigrationError> {
        serde_json::to_string_pretty(self).map_err(|e| StateMigrationError::CorruptedJson(e.to_string()))
    }

    /// Load and validate a persisted state JSON string.
    pub fn from_json(json: &str) -> Result<Self, StateMigrationError> {
        // First parse into generic Value to check schema_version
        let val: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| StateMigrationError::CorruptedJson(e.to_string()))?;

        let schema = val
            .get("schema_version")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(1);

        if schema > STATE_SCHEMA_VERSION {
            return Err(StateMigrationError::UnsupportedSchema {
                found: schema,
                max_supported: STATE_SCHEMA_VERSION,
            });
        }

        // Apply migrations if schema < STATE_SCHEMA_VERSION
        let migrated_val = migrate_json_value(val, schema, STATE_SCHEMA_VERSION)?;

        serde_json::from_value(migrated_val)
            .map_err(|e| StateMigrationError::CorruptedJson(e.to_string()))
    }
}

/// Perform step-wise JSON tree migration from `from_ver` to `target_ver`.
fn migrate_json_value(
    mut val: serde_json::Value,
    from_ver: u32,
    target_ver: u32,
) -> Result<serde_json::Value, StateMigrationError> {
    let mut current = from_ver;
    while current < target_ver {
        match current {
            // Schema 0 -> 1 migration placeholder
            0 => {
                if let Some(obj) = val.as_object_mut() {
                    obj.insert("schema_version".to_string(), serde_json::json!(1));
                }
                current = 1;
            }
            other => {
                return Err(StateMigrationError::MigrationFailed(format!(
                    "no migration defined from schema {other} to {}",
                    other + 1
                )));
            }
        }
    }
    Ok(val)
}

// ── Concrete State Models ───────────────────────────────────────────────────

/// Versioned state for the top-level AudioEngine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineState {
    pub volume: f32,
    pub speed: f32,
    pub bit_perfect: bool,
    pub dop_active: bool,
    pub dsd_output: DsdOutput,
    pub output_backend: AudioBackend,
    pub output_device: Option<String>,
    pub config: EngineConfig,
}

/// Versioned state for the compiled DSP graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphState {
    pub active_nodes: Vec<String>,
    pub routing_layout: String,
    pub sample_rate: u32,
    pub total_latency_samples: usize,
}

/// Versioned state for an individual DSP node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeState {
    pub node_name: String,
    pub enabled: bool,
    pub parameters: BTreeMap<String, f32>,
}

/// Versioned state for a native or hosted audio plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginState {
    pub plugin_id: String,
    pub plugin_name: String,
    pub enabled: bool,
    pub params: Vec<(u32, f32)>,
    #[serde(default)]
    pub custom_chunk: Vec<u8>,
}

/// Versioned state for a complete spatial scene.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpatialSceneState {
    pub scene: SpatialSceneConfig,
    pub active_preset: Option<String>,
}

/// Versioned state for a calibrated output device profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputProfileState {
    pub profile_id: String,
    pub device_name: Option<String>,
    pub channel_count: u16,
    pub calibration_gain_db: f32,
    pub delay_ms: f32,
    pub eq_preset: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trip_and_defaults() {
        let node_state = NodeState {
            node_name: "equalizer".to_string(),
            enabled: true,
            parameters: BTreeMap::from([
                ("freq_band_0".to_string(), 1000.0),
                ("gain_band_0".to_string(), 3.5),
            ]),
        };

        let env = VersionedEnvelope::new(node_state.clone(), 1);
        let json = env.to_json_pretty().unwrap();

        let loaded: VersionedEnvelope<NodeState> = VersionedEnvelope::from_json(&json).unwrap();
        assert_eq!(loaded.schema_version, STATE_SCHEMA_VERSION);
        assert_eq!(loaded.engine_version, CURRENT_ENGINE_VERSION);
        assert_eq!(loaded.component_version, 1);
        assert_eq!(loaded.state, node_state);
    }

    #[test]
    fn rejects_unsupported_future_schema() {
        let future_json = r#"{
            "schema_version": 999,
            "engine_version": "99.0.0",
            "component_version": 1,
            "state": {
                "node_name": "test",
                "enabled": true,
                "parameters": {}
            }
        }"#;

        let result: Result<VersionedEnvelope<NodeState>, _> = VersionedEnvelope::from_json(future_json);
        assert!(matches!(
            result,
            Err(StateMigrationError::UnsupportedSchema { found: 999, .. })
        ));
    }

    #[test]
    fn legacy_unversioned_payload_loads_with_defaults() {
        let legacy_json = r#"{
            "state": {
                "node_name": "compressor",
                "enabled": false,
                "parameters": {}
            }
        }"#;

        let loaded: VersionedEnvelope<NodeState> = VersionedEnvelope::from_json(legacy_json).unwrap();
        assert_eq!(loaded.schema_version, 1);
        assert_eq!(loaded.state.node_name, "compressor");
        assert!(!loaded.state.enabled);
    }
}
