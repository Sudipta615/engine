//! Engine state management and persistence utilities (§9.2, Item 18).
//!
//! Provides file-level save/load helpers and snapshotting utilities for
//! versioned engine configurations, graph presets, node parameters, and device profiles.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

pub use config::{
    EngineState, GraphState, NodeState, OutputProfileState, PluginState, SpatialSceneState,
    StateMigrationError, VersionedEnvelope, CURRENT_ENGINE_VERSION, STATE_SCHEMA_VERSION,
};

/// File-level state errors.
#[derive(Debug)]
pub enum StateFileError {
    Io(std::io::Error),
    Migration(StateMigrationError),
}

impl std::fmt::Display for StateFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateFileError::Io(e) => write!(f, "state file I/O error: {e}"),
            StateFileError::Migration(e) => write!(f, "state format error: {e}"),
        }
    }
}

impl std::error::Error for StateFileError {}

impl From<std::io::Error> for StateFileError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<StateMigrationError> for StateFileError {
    fn from(err: StateMigrationError) -> Self {
        Self::Migration(err)
    }
}

/// Save a versioned state envelope to disk formatted as JSON.
pub fn save_versioned_state<T: Serialize + for<'de> Deserialize<'de> + Clone>(
    path: &Path,
    state: &T,
    component_version: u32,
) -> Result<(), StateFileError> {
    let env = VersionedEnvelope::new(state.clone(), component_version);
    let json = env.to_json_pretty()?;
    let mut file = File::create(path)?;
    file.write_all(json.as_bytes())?;
    Ok(())
}

/// Load and migrate a versioned state envelope from disk.
pub fn load_versioned_state<T: Serialize + for<'de> Deserialize<'de>>(
    path: &Path,
) -> Result<VersionedEnvelope<T>, StateFileError> {
    let mut file = File::open(path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let env = VersionedEnvelope::from_json(&contents)?;
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn file_save_and_load_round_trip() {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join(format!("test_state_{}.json", std::process::id()));

        let node_state = NodeState {
            node_name: "parametric_eq".to_string(),
            enabled: true,
            parameters: BTreeMap::from([
                ("band_0_freq".to_string(), 250.0),
                ("band_0_gain".to_string(), -4.5),
            ]),
        };

        save_versioned_state(&path, &node_state, 1).unwrap();
        let loaded: VersionedEnvelope<NodeState> = load_versioned_state(&path).unwrap();

        assert_eq!(loaded.schema_version, STATE_SCHEMA_VERSION);
        assert_eq!(loaded.state, node_state);

        let _ = std::fs::remove_file(path);
    }
}
