//! Auto-save / restore of the active spatial scene (Phase 21).
//!
//! The graph's [`SpatialNode`] owns the *active* spatial scene — the master
//! enable flag, the virtual screen, the room, and the listener orientation.
//! That surface is exactly what [`config::SpatialConfig`] models (the same
//! serde model the graph configures from at construction), so persisting it
//! needs no new file format: a snapshot of the node's state is written to a
//! JSON file in the user data directory, and restored on engine
//! construction.
//!
//! **When it writes.** [`SpatialPersistence::maybe_save`] runs once per
//! engine tick *after* queued graph controls have been applied and writes
//! only when the spatial state actually changed (the write is skipped
//! otherwise — a plain field comparison, no I/O on the steady path).
//! [`SpatialPersistence::save_now`] forces the final snapshot at shutdown,
//! so a graceful exit always persists the scene even if no tick saw a
//! change.
//!
//! **Crash safety.** Writes go to a temp file that is atomically renamed
//! over the previous snapshot, so a crash mid-write can never corrupt the
//! last good scene. Restore is strictly best-effort: a missing or unreadable
//! file, invalid JSON, or an over-capacity config silently leaves the
//! engine's configured default in place — persistence must never fail
//! engine construction.
//!
//! All of this runs on the control path (engine thread / construction /
//! drop); the audio thread never touches this module.
//!
//! [`SpatialNode`]: crate::dsp::graph::nodes::SpatialNode

use crate::dsp::graph::DspGraph;
use config::{SpatialConfig, SpatialRoomConfig};
use std::path::{Path, PathBuf};

/// File name of the auto-saved scene (inside the engine data directory).
pub const AUTOSAVE_FILE_NAME: &str = "spatial_scene.json";

/// Absolute path of the auto-save file (best effort — `None` when no user
/// data directory can be resolved, in which case persistence is disabled
/// rather than erroring).
pub fn autosave_path() -> Option<PathBuf> {
    let mut dir = crate::paths::data_local_dir()?;
    dir.push("engine");
    let _ = std::fs::create_dir_all(&dir);
    dir.push(AUTOSAVE_FILE_NAME);
    Some(dir)
}

/// Read the graph's current spatial state as a [`SpatialConfig`] snapshot.
fn snapshot(graph: &DspGraph) -> SpatialConfig {
    let sp = graph.spatial();
    let (center_azimuth_deg, half_width_deg, elevation_deg, gain) = sp.screen();
    let (enabled, width, depth, height, absorption, reflection_order, rt60_ms, late_mix, wet) =
        sp.room();
    let (listener_yaw_deg, listener_pitch_deg, listener_roll_deg) = sp.listener();
    SpatialConfig {
        enabled,
        center_azimuth_deg,
        half_width_deg,
        elevation_deg,
        gain,
        room: SpatialRoomConfig {
            enabled,
            width,
            depth,
            height,
            absorption,
            reflection_order,
            rt60_ms,
            late_mix,
            wet,
            // The node does not expose its room's speed of sound; keep the
            // config default (matching the node's initial Room).
            speed_of_sound: SpatialRoomConfig::default().speed_of_sound,
        },
        listener_yaw_deg,
        listener_pitch_deg,
        listener_roll_deg,
        // Render knobs are host-level (not node state); snapshot the defaults
        // so a persisted file round-trips a complete SpatialConfig.
        quality: Default::default(),
        voice: Default::default(),
        metering: Default::default(),
    }
}

/// Serialize the snapshot to the auto-save file atomically (temp + rename).
fn write_atomic(path: &Path, cfg: &SpatialConfig) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let file = std::fs::File::create(&tmp)?;
    let result = serde_json::to_writer_pretty(&file, cfg).map_err(std::io::Error::other);
    drop(file);
    result?;
    std::fs::rename(&tmp, path)
}

/// Control-path persistence of the active spatial scene.
pub struct SpatialPersistence {
    path: Option<PathBuf>,
    last_saved: Option<SpatialConfig>,
}

impl Default for SpatialPersistence {
    fn default() -> Self {
        Self::new()
    }
}

impl SpatialPersistence {
    pub fn new() -> Self {
        Self {
            path: autosave_path(),
            last_saved: None,
        }
    }

    /// Persistence honoring the config's explicit path (when set), else the
    /// user-data-directory default.
    pub(crate) fn from_config(config: &config::EngineConfig) -> Self {
        Self {
            path: config.spatial_autosave_path.clone().or_else(autosave_path),
            last_saved: None,
        }
    }

    /// Restore the last auto-saved scene into the graph, if one exists.
    /// Best-effort: returns `true` only when a scene was actually applied.
    /// Called at engine construction (control path, before audio starts).
    /// On success the restored state becomes the save baseline, so the next
    /// tick does not rewrite an unchanged scene.
    pub fn restore(&mut self, graph: &mut DspGraph, sample_rate: f32) -> bool {
        let Some(path) = &self.path else {
            return false;
        };
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return false,
        };
        let cfg: SpatialConfig = match serde_json::from_str(&text) {
            Ok(c) => c,
            Err(_) => return false,
        };
        graph.spatial_mut().apply_config(&cfg, sample_rate.max(1.0));
        self.last_saved = Some(cfg);
        true
    }

    /// Write the current spatial state when it differs from the last save.
    /// Runs once per engine tick after queued graph controls are applied.
    pub fn maybe_save(&mut self, graph: &DspGraph) {
        let Some(path) = &self.path else {
            return;
        };
        let current = snapshot(graph);
        if self.last_saved.as_ref() == Some(&current) {
            return;
        }
        if write_atomic(path, &current).is_ok() {
            self.last_saved = Some(current);
        }
    }

    /// Force a final save (engine shutdown).
    pub fn save_now(&mut self, graph: &DspGraph) {
        let Some(path) = &self.path else {
            return;
        };
        let current = snapshot(graph);
        if write_atomic(path, &current).is_ok() {
            self.last_saved = Some(current);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_spatial(graph: &mut DspGraph) {
        graph.control_handle().set_spatial_enabled(true);
        graph
            .control_handle()
            .set_spatial_screen(12.0, 40.0, 5.0, 0.9);
        graph
            .control_handle()
            .set_spatial_room(true, 9.0, 7.0, 3.0, 0.25, 1, 500.0, 0.35, 0.6);
        graph.control_handle().set_spatial_listener(15.0, -3.0, 0.0);
        // The engine's tick drains queued graph controls; replicate that
        // so the node state (and any snapshot) reflects the new values.
        graph.drain_queued_control();
    }

    #[test]
    fn snapshot_round_trips_through_the_autosave_file() {
        let dir =
            std::env::temp_dir().join(format!("shadow_spatial_persist_rt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Build a graph, apply a distinct spatial state, persist it.
        let cfg = crate::EngineConfig::default();
        let mut graph = DspGraph::from_config(&cfg, 48_000.0);
        apply_spatial(&mut graph);
        let mut persister = SpatialPersistence {
            path: Some(dir.join(AUTOSAVE_FILE_NAME)),
            last_saved: None,
        };
        persister.save_now(&graph);
        assert!(dir.join(AUTOSAVE_FILE_NAME).exists());

        // A fresh graph restores the saved state exactly.
        let mut restored = DspGraph::from_config(&cfg, 48_000.0);
        assert!(!restored.spatial().enabled());
        assert!(persister.restore(&mut restored, 48_000.0));
        assert!(restored.spatial().enabled());
        assert_eq!(restored.spatial().screen(), (12.0, 40.0, 5.0, 0.9));
        assert_eq!(
            restored.spatial().room(),
            (true, 9.0, 7.0, 3.0, 0.25, 1, 500.0, 0.35, 0.6)
        );
        assert_eq!(restored.spatial().listener(), (15.0, -3.0, 0.0));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn maybe_save_writes_only_on_change() {
        let dir =
            std::env::temp_dir().join(format!("shadow_spatial_persist_ms_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(AUTOSAVE_FILE_NAME);

        let cfg = crate::EngineConfig::default();
        let mut graph = DspGraph::from_config(&cfg, 48_000.0);
        let mut persister = SpatialPersistence {
            path: Some(file.clone()),
            last_saved: None,
        };

        // Baseline save, then the same state must not rewrite the file.
        persister.save_now(&graph);
        assert!(file.exists());
        let stamp = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        persister.maybe_save(&graph);
        assert_eq!(
            std::fs::metadata(&file).unwrap().modified().unwrap(),
            stamp,
            "unchanged state must not rewrite the file"
        );

        // A change triggers exactly one rewrite.
        apply_spatial(&mut graph);
        std::thread::sleep(std::time::Duration::from_millis(20));
        persister.maybe_save(&graph);
        assert_ne!(std::fs::metadata(&file).unwrap().modified().unwrap(), stamp);
        let stamp2 = std::fs::metadata(&file).unwrap().modified().unwrap();

        // And the rewritten state restores back.
        let mut restored = DspGraph::from_config(&cfg, 48_000.0);
        assert!(persister.restore(&mut restored, 48_000.0));
        assert!(restored.spatial().enabled());
        assert_eq!(restored.spatial().screen(), (12.0, 40.0, 5.0, 0.9));
        // Restore seeded the baseline: no rewrite on the next check.
        std::thread::sleep(std::time::Duration::from_millis(20));
        persister.maybe_save(&graph);
        assert_eq!(
            std::fs::metadata(&file).unwrap().modified().unwrap(),
            stamp2,
            "restore baseline must suppress the next save"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_is_best_effort() {
        let dir =
            std::env::temp_dir().join(format!("shadow_spatial_persist_be_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let cfg = crate::EngineConfig::default();
        let mut graph = DspGraph::from_config(&cfg, 48_000.0);
        let mut p = SpatialPersistence {
            path: Some(dir.join(AUTOSAVE_FILE_NAME)),
            last_saved: None,
        };
        // Missing file → no restore, no panic.
        assert!(!p.restore(&mut graph, 48_000.0));
        // Corrupt JSON → no restore, no panic.
        std::fs::write(dir.join(AUTOSAVE_FILE_NAME), b"{not json").unwrap();
        assert!(!p.restore(&mut graph, 48_000.0));
        assert!(!graph.spatial().enabled());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
