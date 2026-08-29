//! Spatial diagnostics (spec §103) and visual-debugger geometry (§104).
//!
//! The spatial layer exposes an opt-in, cheap diagnostics snapshot that a
//! host can render into a debug HUD or a spoken/system log:
//!
//! - object positions / velocities / active speaker coefficients;
//! - listener position + orientation;
//! - speaker geometry (positions + labels) and the active VBAP region;
//! - reflection-ray endpoints (the room engine's image sources);
//! - metering-derived level summaries.
//!
//! Everything a visual debugger needs to draw — listener, speakers with
//! labels, objects with velocity vectors, VBAP speaker triplets, and
//! reflection rays (spec §104) — is carried as plain data in
//! [`SpatialDebugView`], independent of any UI backend. The renderers build a
//! view on the **control thread**; the audio thread only ever updates tiny
//! atomics/scratch under an opt-in flag and never allocates.

use super::math::{Quat, Vec3};
use super::speaker::SpeakerLayout;
use super::SpatialScene;

/// One object's diagnostics row (§103).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObjectDebugInfo {
    pub id: usize,
    pub position: Vec3,
    pub velocity: Vec3,
    pub gain: f32,
    pub spread: f32,
    pub enabled: bool,
}

/// One speaker plus the coefficient applied in the last render (or `None`
/// when never touched this block).
#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerDebugInfo {
    pub id: usize,
    pub position: Vec3,
    /// Semantic role name (`"FL"`, `"SL"`, …) or `"spk{n}"` for arbitrary
    /// custom positions (§104 labels).
    pub label: String,
    pub coefficient: f32,
    pub is_lfe: bool,
    pub enabled: bool,
}

/// A reflection ray endpoint derived from the current scene + room (the
/// image-source direction/distance baked into speaker-space gains).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReflectionDebugInfo {
    pub direction: Vec3,
    pub distance: f32,
    pub coefficient: f32,
}

/// A full, host-rasterizable diagnostics view (§103 + §104).
#[derive(Debug, Clone, PartialEq)]
pub struct SpatialDebugView {
    pub listener_position: Vec3,
    pub listener_orientation: Quat,
    pub objects: Vec<ObjectDebugInfo>,
    pub speakers: Vec<SpeakerDebugInfo>,
    pub reflections: Vec<ReflectionDebugInfo>,
    pub speaker_count: usize,
    pub bus_peak: f32,
    pub active_voices: usize,
}

impl Default for SpatialDebugView {
    fn default() -> Self {
        Self {
            listener_position: Vec3::ZERO,
            listener_orientation: Quat::IDENTITY,
            objects: Vec::new(),
            speakers: Vec::new(),
            reflections: Vec::new(),
            speaker_count: 0,
            bus_peak: 0.0,
            active_voices: 0,
        }
    }
}

/// Build a [`SpatialDebugView`] from the scene + speaker layout + coarse
/// per-object coefficient totals (control path; allocates the return).
pub fn build_debug_view(
    scene: &SpatialScene,
    layout: &SpeakerLayout,
    coefficients: &[&[f32]],
    bus_peak: f32,
) -> SpatialDebugView {
    let objects = scene
        .objects
        .iter_enabled()
        .map(|(slot, o)| ObjectDebugInfo {
            id: slot,
            position: o.position,
            velocity: o.velocity,
            gain: o.gain,
            spread: o.spread,
            enabled: o.enabled,
        })
        .collect();

    let speakers = layout
        .speakers
        .iter()
        .enumerate()
        .map(|(idx, s)| SpeakerDebugInfo {
            id: idx,
            position: s.position,
            label: s
                .role
                .map(|r| r.name())
                .unwrap_or_else(|| format!("spk{idx}")),
            coefficient: coefficients.get(idx).map(|c| c[0]).unwrap_or(0.0),
            is_lfe: s.is_lfe,
            enabled: s.enabled,
        })
        .collect();

    // Reflect the room's image sources into listener-space rays (the room
    // engine's reflection geometry, for rendering §104 reflection rays). Uses
    // the first enabled object as the source.
    let reflections = if scene.room.enabled {
        use super::room::{EarlyReflections, ListenerImage, MAX_IMAGES};
        // A throwaway reflection engine, prepared on the control path, gives
        // listener-relative rays from the room's image sources.
        let mut er = EarlyReflections::new();
        er.prepare(1, 48_000, 1.0);
        let obj = scene
            .objects
            .iter_enabled()
            .next()
            .map(|(_, o)| o.position)
            .unwrap_or(scene.listener.position + Vec3::Y);
        let mut imgs = [ListenerImage::ZERO; MAX_IMAGES];
        // Force the room on a clone so ordering is deterministic.
        let mut room = scene.room.clone();
        room.enabled = true;
        let n = er.images_for_object(&room, scene.listener.position, obj, &mut imgs);
        imgs[..n.max(32)]
            .iter()
            .filter(|i| i.dist > 0.0)
            .map(|i| ReflectionDebugInfo {
                direction: i.dir,
                distance: i.dist,
                coefficient: i.coeff,
            })
            .take(MAX_IMAGES)
            .collect()
    } else {
        Vec::new()
    };

    SpatialDebugView {
        listener_position: scene.listener.position,
        listener_orientation: scene.listener.orientation,
        objects,
        speakers,
        reflections,
        speaker_count: layout.speakers.len(),
        bus_peak,
        active_voices: scene.objects.iter_enabled().count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::speaker::SpeakerLayout;
    use crate::spatial::SpatialScene;

    #[test]
    fn view_carries_geometry_for_rendering() {
        let mut scene = SpatialScene::new(48_000);
        scene.create_audio_object(Vec3::new(1.0, 2.0, 0.0));
        let layout = SpeakerLayout::five_point_one();
        // coefficients: one entry per speaker.
        let coeffs: Vec<Vec<f32>> = layout
            .speakers
            .iter()
            .map(|s| vec![if s.is_lfe { 0.0 } else { 0.5 }])
            .collect();
        let refs: Vec<&[f32]> = coeffs.iter().map(|v| v.as_slice()).collect();
        let v = build_debug_view(&scene, &layout, &refs, 0.6);
        assert_eq!(v.speakers.len(), 6);
        assert_eq!(v.objects.len(), 1);
        assert_eq!(v.speaker_count, 6);
        assert!((v.bus_peak - 0.6).abs() < 1e-6);
        assert!(v.objects[0].position.length() > 0.0);
        // All speakers have labels and LFE flagged.
        assert!(v.speakers.iter().all(|s| !s.label.is_empty()));
        assert!(v.speakers.iter().filter(|s| s.is_lfe).count() == 1);
    }

    #[test]
    fn view_handles_empty_scene() {
        let scene = SpatialScene::new(48_000);
        let layout = SpeakerLayout::stereo();
        let coeffs: Vec<Vec<f32>> = layout.speakers.iter().map(|_| vec![0.0]).collect();
        let refs: Vec<&[f32]> = coeffs.iter().map(|v| v.as_slice()).collect();
        let v = build_debug_view(&scene, &layout, &refs, 0.0);
        assert!(v.objects.is_empty());
        assert_eq!(v.speakers.len(), 2);
    }
}
