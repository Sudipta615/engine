//! Engine-lifecycle integration tests for spatial scene persistence
//! (Phase 21): the active spatial scene auto-saves on change and at
//! shutdown, and restores across engine sessions.

use config::EngineConfig;

use crate::engine::AudioEngine;

fn headless_engine_with(dir: &std::path::Path) -> AudioEngine {
    let mut config = EngineConfig::default();
    config.endpoints.clear();
    config.spatial_autosave_path = Some(dir.join("spatial_scene.json"));
    AudioEngine::with_sink(config, Box::new(crate::sink::NoopSink)).expect("headless engine")
}

/// The engine's tick applies queued graph controls and persists the spatial
/// scene; a fresh engine then restores it — the full "across sessions"
/// contract.
#[test]
fn spatial_scene_survives_an_engine_restart() {
    let dir = std::env::temp_dir().join(format!(
        "freebuff_engine_spatial_persist_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("spatial_scene.json");

    // Session 1: enable the spatial stage and tune screen / room / listener.
    {
        let mut engine = headless_engine_with(&dir);
        let handle = engine.pipeline_mut().control_handle();
        handle.set_spatial_enabled(true);
        handle.set_spatial_screen(10.0, 35.0, 8.0, 0.85);
        handle.set_spatial_room(true, 12.0, 9.0, 3.2, 0.2, 1, 550.0, 0.3, 0.5);
        handle.set_spatial_listener(20.0, -5.0, 2.0);
        // The engine tick drains queued graph controls and auto-saves the
        // changed scene.
        engine.tick();
        assert!(file.exists(), "tick auto-save must write the scene file");
    }
    // Session 2: a fresh engine restores the scene before audio starts.
    let engine = headless_engine_with(&dir);
    let sp = engine.pipeline().spatial();
    assert!(sp.enabled(), "restored scene must be enabled");
    assert_eq!(sp.screen(), (10.0, 35.0, 8.0, 0.85));
    assert_eq!(sp.room(), (true, 12.0, 9.0, 3.2, 0.2, 1, 550.0, 0.3, 0.5));
    assert_eq!(sp.listener(), (20.0, -5.0, 2.0));

    let _ = std::fs::remove_dir_all(&dir);
}

/// Drop persists the scene even when no tick observed the change.
#[test]
fn drop_persists_the_final_scene() {
    let dir = std::env::temp_dir().join(format!(
        "freebuff_engine_spatial_drop_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("spatial_scene.json");

    {
        let mut engine = headless_engine_with(&dir);
        engine
            .pipeline_mut()
            .control_handle()
            .set_spatial_listener(45.0, 0.0, 0.0);
        // No tick: the drop alone must persist the final scene.
    }
    assert!(file.exists(), "drop must persist the scene file");

    let engine = headless_engine_with(&dir);
    assert_eq!(engine.pipeline().spatial().listener(), (45.0, 0.0, 0.0));

    let _ = std::fs::remove_dir_all(&dir);
}

/// A missing auto-save leaves the configured default in place — persistence
/// must never fail construction.
#[test]
fn missing_autosave_is_a_noop() {
    let dir = std::env::temp_dir().join(format!(
        "freebuff_engine_spatial_none_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let engine = headless_engine_with(&dir);
    assert!(!engine.pipeline().spatial().enabled());

    let _ = std::fs::remove_dir_all(&dir);
}

/// A fresh engine with the default (unset) path must not fail or read the
/// test's temp files.
#[test]
fn default_path_engine_constructs_cleanly() {
    let _ = AudioEngine::new_default().expect("default engine");
}
