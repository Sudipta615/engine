//! Spatial-health diagnostics (spec §103 extension): *why* the spatial
//! render behaves the way it does.
//!
//! The spatial layer already meters levels ([`SpatialMeters`]) and exposes a
//! debug view (geometry). This module turns those raw measurements into an
//! explainable per-source status: for every enabled object it derives
//!
//! - **localization quality** — measured-HRTF grid coverage (or analytic
//!   fallback) plus the blur that angular spread intentionally adds;
//! - **direct-vs-reflected energy ratio** — the source's direct path gain
//!   vs its room send scaled by the wall reflection coefficient;
//! - **occlusion severity** — the applied broadband attenuation + low-pass
//!   cutoff from the source's [`Occlusion`];
//! - **phase risk** — inter-channel decorrelation measured on the master
//!   output (cross-energy), and per-source heuristics (spread / extreme
//!   pan) that predict decorrelation.
//!
//! The layer **explains state, it never alters the signal** (spec: spatial
//! diagnostics are passive). It runs entirely on the **control/telemetry
//! path**: the engine tick calls [`build_health`] on the telemetry cadence
//! from the existing meter snapshot + scene + voice-admission counts. The
//! audio path is untouched — no new work, no allocation, no locks.
//!
//! Every level carries a stable machine-readable code (`HealthLevel::code`)
//! plus a human note, and the snapshot is serde-serializable so a host can
//! render it as JSON or feed a HUD.

use serde::{Deserialize, Serialize};

use super::metering::SpatialMeters;
use super::quality::SpatialQuality;
use super::room::reflection_coefficient;
use super::scene::{ListenerTransform, SpatialScene};

/// A health verdict with a stable machine-readable code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthLevel {
    /// Stage disabled / nothing to evaluate.
    #[default]
    Inactive,
    /// No action needed.
    Good,
    /// Watch: within limits but a degradation is visible.
    Moderate,
    /// Action needed: the factor is materially degraded.
    Poor,
}

impl HealthLevel {
    /// Stable code a host can branch on (do not rename casually).
    pub fn code(self) -> &'static str {
        match self {
            HealthLevel::Inactive => "inactive",
            HealthLevel::Good => "good",
            HealthLevel::Moderate => "moderate",
            HealthLevel::Poor => "poor",
        }
    }

    /// Worst (most severe) of two levels: `Inactive < Good < Moderate < Poor`.
    pub fn worst(self, other: HealthLevel) -> HealthLevel {
        match (self, other) {
            (HealthLevel::Poor, _) | (_, HealthLevel::Poor) => HealthLevel::Poor,
            (HealthLevel::Moderate, _) | (_, HealthLevel::Moderate) => HealthLevel::Moderate,
            (HealthLevel::Good, _) | (_, HealthLevel::Good) => HealthLevel::Good,
            _ => HealthLevel::Inactive,
        }
    }
}

impl std::fmt::Display for HealthLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

/// One explainable factor of the health model: a verdict, a normalized
/// `0..1` score (1 = best), and the human note that explains the verdict.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct HealthFactor {
    pub level: HealthLevel,
    /// Normalized quality `[0, 1]`; `1.0` = ideal for this factor.
    pub score: f32,
    /// Why this verdict: a short, concrete explanation.
    pub note: String,
}

impl HealthFactor {
    fn new(level: HealthLevel, score: f32, note: impl Into<String>) -> Self {
        Self {
            level,
            score: score.clamp(0.0, 1.0),
            note: note.into(),
        }
    }

    /// An all-Inactive factor (spatial stage disabled).
    fn inactive(note: &str) -> Self {
        Self::new(HealthLevel::Inactive, 0.0, note)
    }
}

/// HRTF dataset grid coverage (degrees), used for localization scoring.
/// Built from the loaded dataset's azimuth/elevation tables; `None` means
/// the analytic head model is in use.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HrtfCoverage {
    pub azimuth_min_deg: f32,
    pub azimuth_max_deg: f32,
    pub elevation_min_deg: f32,
    pub elevation_max_deg: f32,
}

impl HrtfCoverage {
    /// Whether `azimuth_deg` (any range) is within the grid, allowing a
    /// `grace_deg` tolerance on each side. Datasets store azimuths
    /// monotonically ascending (the dataset constructor rejects non-monotonic
    /// grids), so `azimuth_min_deg..=azimuth_max_deg` is the full span; the
    /// queried angle is normalized into that span's own 360° window.
    pub fn azimuth_covered(&self, azimuth_deg: f32, grace_deg: f32) -> bool {
        let span = self.azimuth_max_deg - self.azimuth_min_deg;
        let az_norm = (azimuth_deg - self.azimuth_min_deg).rem_euclid(360.0);
        az_norm >= -grace_deg && az_norm <= span + grace_deg
    }

    /// Whether `elevation_deg` is within the grid, allowing `grace_deg`.
    pub fn elevation_covered(&self, elevation_deg: f32, grace_deg: f32) -> bool {
        elevation_deg >= self.elevation_min_deg - grace_deg
            && elevation_deg <= self.elevation_max_deg + grace_deg
    }
}

/// Per-source spatial-health status: an explainable verdict for one object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceHealth {
    /// The object's store-slot id (stable [`super::ObjectId`] index).
    pub object_id: usize,
    /// The worst per-factor level for this source.
    pub status: HealthLevel,
    pub localization: HealthFactor,
    /// Direct-path gain vs reflected send, in dB (`+∞` = no reflections).
    pub direct_reflected_ratio_db: f32,
    pub reflection: HealthFactor,
    pub occlusion: HealthFactor,
    pub phase_risk: HealthFactor,
    /// Concatenated explanations (the factor notes).
    pub reasons: Vec<String>,
}

/// The full scene-level spatial-health report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpatialHealthSnapshot {
    /// Overall verdict: the worst master factor; `Inactive` when the spatial
    /// stage is disabled or the scene has no enabled sources.
    pub status: HealthLevel,
    pub quality_tier: SpatialQuality,
    pub localization: HealthFactor,
    /// Worst-case direct-vs-reflected dominance across sources.
    pub reflection_dominance: HealthFactor,
    /// Worst-case occlusion severity across sources.
    pub occlusion: HealthFactor,
    /// Measured inter-channel phase risk of the master output.
    pub phase_risk: HealthFactor,
    pub voice_pressure: HealthFactor,
    /// Scene-wide direct-vs-reflected ratio (dB): the minimum across
    /// sources (`+∞` when no source sends to the room).
    pub direct_reflected_ratio_db: f32,
    /// Measured master inter-channel correlation `[-1, 1]` (`0` = unknown).
    pub stereo_correlation: f32,
    pub active_sources: usize,
    pub degraded_voice_count: usize,
    pub dropped_voice_count: usize,
    pub per_source: Vec<SourceHealth>,
}

/// Control-path inputs to [`build_health`], gathered by the engine tick from
/// the existing spatial telemetry (scene, meter snapshot, voice counts).
#[derive(Debug, Clone, Copy)]
pub struct SpatialHealthInputs<'a> {
    pub scene: &'a SpatialScene,
    pub meters: &'a SpatialMeters,
    /// Whether the spatial master stage is enabled.
    pub enabled: bool,
    pub quality: SpatialQuality,
    pub voice_active: bool,
    pub voice_full: usize,
    pub voice_degraded: usize,
    pub voice_dropped: usize,
    /// Loaded HRTF grid coverage, when a dataset is attached.
    pub hrtf: Option<HrtfCoverage>,
}

/// Build the scene-level spatial-health report from the existing metering +
/// scene + voice-admission state. **Control/telemetry path only** — the
/// audio path never calls this. Allocates the return (per-source rows);
/// bounded by [`super::MAX_SPATIAL_OBJECTS`].
#[allow(clippy::too_many_arguments)]
pub fn build_health(inputs: SpatialHealthInputs) -> SpatialHealthSnapshot {
    let SpatialHealthInputs {
        scene,
        meters,
        enabled,
        quality,
        voice_active,
        voice_full,
        voice_degraded,
        voice_dropped,
        hrtf,
    } = inputs;

    if !enabled {
        let inactive = HealthFactor::inactive("spatial stage disabled");
        return SpatialHealthSnapshot {
            status: HealthLevel::Inactive,
            quality_tier: quality,
            localization: inactive.clone(),
            reflection_dominance: inactive.clone(),
            occlusion: inactive.clone(),
            phase_risk: inactive.clone(),
            voice_pressure: inactive,
            direct_reflected_ratio_db: f32::INFINITY,
            stereo_correlation: 0.0,
            active_sources: 0,
            degraded_voice_count: 0,
            dropped_voice_count: 0,
            per_source: Vec::new(),
        };
    }

    let sample_rate = scene.sample_rate as f32;
    let lt = ListenerTransform::from_listener(&scene.listener);
    let room_ref_coeff = if scene.room.enabled {
        reflection_coefficient(&scene.room)
    } else {
        0.0
    };

    let mut per_source = Vec::new();
    let mut min_ratio_db = f32::INFINITY;

    for (slot, obj) in scene.objects.iter_enabled() {
        let local = lt.apply_to_point(obj.position);
        let dist = local.length().max(1e-6);
        let azimuth_deg = local.x.atan2(local.y).to_degrees();
        let elevation_deg = (local.z / dist).clamp(-1.0, 1.0).asin().to_degrees();

        // ── localization ──────────────────────────────────────────────
        let (mut loc_score, mut loc_note) = match hrtf {
            Some(g)
                if g.azimuth_covered(azimuth_deg, 10.0)
                    && g.elevation_covered(elevation_deg, 5.0) =>
            {
                (
                    1.0,
                    format!(
                        "measured HRTF: az {azimuth_deg:.0}° el {elevation_deg:.0}° inside grid"
                    ),
                )
            }
            Some(_) => (
                0.55,
                format!(
                    "HRTF out of measured grid (az {azimuth_deg:.0}° el {elevation_deg:.0}°) — analytic fallback"
                ),
            ),
            None => (
                0.9,
                "analytic head model (no HRTF dataset)".to_string(),
            ),
        };
        if obj.spread > 0.5 {
            loc_score *= 1.0 - 0.4 * obj.spread.min(1.0);
            loc_note = format!("{loc_note}; extended source spread {:.2}", obj.spread);
        }
        let loc_level = if loc_score >= 0.8 {
            HealthLevel::Good
        } else if loc_score >= 0.5 {
            HealthLevel::Moderate
        } else {
            HealthLevel::Poor
        };
        let localization = HealthFactor::new(loc_level, loc_score, loc_note);

        // ── direct vs reflected ───────────────────────────────────────
        let dist_gain = obj
            .distance_model
            .distance_gain(dist, obj.reference_distance);
        let occ_trans = obj.occlusion.transmission(sample_rate);
        let direct_gain = obj.gain * dist_gain * occ_trans.gain();
        let reflected = if scene.room.enabled && obj.room_send > 0.0 {
            obj.room_send * room_ref_coeff
        } else {
            0.0
        };
        let ratio_db = if reflected <= 1e-6 {
            f32::INFINITY
        } else {
            20.0 * (direct_gain / reflected).log10()
        };
        min_ratio_db = min_ratio_db.min(ratio_db);
        let reflection = if !ratio_db.is_finite() {
            HealthFactor::new(
                HealthLevel::Good,
                1.0,
                "no room send — direct-only path".to_string(),
            )
        } else if ratio_db >= 10.0 {
            HealthFactor::new(
                HealthLevel::Good,
                0.9,
                format!("direct dominates reflections by {ratio_db:.1} dB"),
            )
        } else if ratio_db >= 0.0 {
            HealthFactor::new(
                HealthLevel::Moderate,
                (0.5 + ratio_db / 20.0).clamp(0.3, 1.0),
                format!("reflected energy approaching direct ({ratio_db:.1} dB headroom)"),
            )
        } else {
            HealthFactor::new(
                HealthLevel::Poor,
                0.2,
                format!("reflected energy exceeds direct by {:.1} dB", -ratio_db),
            )
        };

        // ── occlusion ─────────────────────────────────────────────────
        let amount = obj.occlusion.amount.clamp(0.0, 1.0);
        let occlusion = if amount <= 0.0 {
            HealthFactor::new(HealthLevel::Good, 1.0, "unoccluded")
        } else if amount < 0.5 {
            HealthFactor::new(
                HealthLevel::Moderate,
                1.0 - amount,
                format!(
                    "mild occlusion: −{:.1} dB, low-pass {:.0} Hz",
                    occ_trans.attenuation_db, occ_trans.cutoff_hz
                ),
            )
        } else {
            HealthFactor::new(
                HealthLevel::Poor,
                1.0 - amount,
                format!(
                    "severe occlusion: −{:.1} dB, low-pass {:.0} Hz",
                    occ_trans.attenuation_db, occ_trans.cutoff_hz
                ),
            )
        };

        // ── per-source phase risk ─────────────────────────────────────
        let phase_risk = if obj.spread >= 0.6 {
            HealthFactor::new(
                HealthLevel::Moderate,
                0.55,
                "wide spread decorrelates the image (intended) — downmix may comb".to_string(),
            )
        } else if azimuth_deg.abs() > 120.0 {
            HealthFactor::new(
                HealthLevel::Moderate,
                0.6,
                format!("extreme pan (az {azimuth_deg:.0}°) → largely opposite ear cues"),
            )
        } else {
            HealthFactor::new(
                HealthLevel::Good,
                0.95,
                "coherent inter-ear cues".to_string(),
            )
        };

        let status = localization
            .level
            .worst(reflection.level)
            .worst(occlusion.level)
            .worst(phase_risk.level);
        let reasons = vec![
            localization.note.clone(),
            reflection.note.clone(),
            occlusion.note.clone(),
            phase_risk.note.clone(),
        ];
        per_source.push(SourceHealth {
            object_id: slot,
            status,
            localization,
            direct_reflected_ratio_db: ratio_db,
            reflection,
            occlusion,
            phase_risk,
            reasons,
        });
    }

    // ── master factors ────────────────────────────────────────────────
    let corr = meters.stereo_correlation;
    let phase_risk = if corr.abs() < 1e-6 {
        HealthFactor::new(
            HealthLevel::Good,
            1.0,
            "no signal yet — correlation unmeasured",
        )
    } else if corr >= 0.9 {
        HealthFactor::new(
            HealthLevel::Good,
            corr,
            format!("strong inter-channel correlation ({corr:.2}) — mono-safe"),
        )
    } else if corr >= 0.7 {
        HealthFactor::new(
            HealthLevel::Moderate,
            corr,
            format!("moderate decorrelation ({corr:.2}) — downmix still phase-OK"),
        )
    } else {
        HealthFactor::new(
            HealthLevel::Poor,
            corr.abs().clamp(0.05, 1.0),
            format!("low inter-channel correlation ({corr:.2}) — mono-compatibility at risk"),
        )
    };

    let voice_pressure = if !voice_active {
        HealthFactor::new(HealthLevel::Good, 1.0, "no voice budget active")
    } else if voice_dropped > 0 {
        HealthFactor::new(
            HealthLevel::Poor,
            0.2,
            format!("{voice_dropped} voice(s) dropped (silenced)"),
        )
    } else if voice_degraded > 0 {
        HealthFactor::new(
            HealthLevel::Moderate,
            0.55,
            format!(
                "{voice_degraded} of {} voice(s) degraded",
                voice_full.saturating_add(voice_degraded)
            ),
        )
    } else {
        HealthFactor::new(
            HealthLevel::Good,
            0.95,
            format!("{voice_full} voice(s) at full quality"),
        )
    };

    let localization = worst_factor(&per_source, |s| &s.localization, "no enabled sources");
    let reflection_dominance = worst_factor(&per_source, |s| &s.reflection, "no enabled sources");
    let occlusion = worst_factor(&per_source, |s| &s.occlusion, "no enabled sources");

    let status = if per_source.is_empty() {
        HealthLevel::Inactive
    } else {
        per_source
            .iter()
            .map(|s| s.status)
            .fold(HealthLevel::Good, HealthLevel::worst)
            .worst(phase_risk.level)
            .worst(voice_pressure.level)
    };

    SpatialHealthSnapshot {
        status,
        quality_tier: quality,
        localization,
        reflection_dominance,
        occlusion,
        phase_risk,
        voice_pressure,
        direct_reflected_ratio_db: min_ratio_db,
        stereo_correlation: corr,
        active_sources: per_source.len(),
        degraded_voice_count: voice_degraded,
        dropped_voice_count: voice_dropped,
        per_source,
    }
}

fn severity_rank(level: HealthLevel) -> u8 {
    match level {
        HealthLevel::Inactive => 0,
        HealthLevel::Good => 1,
        HealthLevel::Moderate => 2,
        HealthLevel::Poor => 3,
    }
}

/// Worst per-factor across sources; names the worst source in the note.
fn worst_factor(
    sources: &[SourceHealth],
    pick: impl Fn(&SourceHealth) -> &HealthFactor,
    empty_note: &str,
) -> HealthFactor {
    let mut worst: Option<(&HealthFactor, usize)> = None;
    for s in sources {
        let f = pick(s);
        let replace = match worst {
            None => true,
            Some((cur, _)) => {
                severity_rank(f.level) > severity_rank(cur.level)
                    || (severity_rank(f.level) == severity_rank(cur.level) && f.score < cur.score)
            }
        };
        if replace {
            worst = Some((f, s.object_id));
        }
    }
    match worst {
        Some((f, id)) => {
            let mut out = f.clone();
            if severity_rank(f.level) >= severity_rank(HealthLevel::Moderate) {
                out.note = format!("{} (source {id})", out.note);
            }
            out
        }
        None => HealthFactor::new(HealthLevel::Inactive, 0.0, empty_note),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::math::Vec3;
    use crate::spatial::object::{ObjectAudioRef, ObjectId, SpatialAudioObject};
    use crate::spatial::occlusion::Occlusion;

    fn scene_with_objects() -> SpatialScene {
        let mut scene = SpatialScene::new(48_000);
        scene
            .objects
            .add(SpatialAudioObject::new(
                ObjectId(0),
                ObjectAudioRef::None,
                Vec3::new(0.0, 2.0, 0.0), // dead ahead of the listener
            ))
            .unwrap();
        scene
    }

    fn empty_meters(correlation: f32) -> SpatialMeters {
        SpatialMeters {
            speaker_peak: vec![0.0; 2],
            speaker_rms: vec![0.0; 2],
            bus_peak: 0.0,
            bus_rms: 0.0,
            lfe_peak: 0.0,
            lfe_rms: 0.0,
            active_voices: 0,
            stereo_correlation: correlation,
        }
    }

    fn default_inputs<'a>(
        scene: &'a SpatialScene,
        meters: &'a SpatialMeters,
    ) -> SpatialHealthInputs<'a> {
        SpatialHealthInputs {
            scene,
            meters,
            enabled: true,
            quality: SpatialQuality::Medium,
            voice_active: false,
            voice_full: 0,
            voice_degraded: 0,
            voice_dropped: 0,
            hrtf: None,
        }
    }

    #[test]
    fn disabled_stage_reports_inactive() {
        let scene = scene_with_objects();
        let meters = empty_meters(0.0);
        let mut inputs = default_inputs(&scene, &meters);
        inputs.enabled = false;
        let h = build_health(inputs);
        assert_eq!(h.status, HealthLevel::Inactive);
        assert_eq!(h.localization.level, HealthLevel::Inactive);
        assert!(h.per_source.is_empty());
    }

    #[test]
    fn clean_scene_is_good_and_explained() {
        let scene = scene_with_objects();
        let meters = empty_meters(0.0);
        let h = build_health(default_inputs(&scene, &meters));
        assert_eq!(h.status, HealthLevel::Good);
        assert_eq!(h.localization.level, HealthLevel::Good);
        assert_eq!(h.occlusion.level, HealthLevel::Good);
        assert_eq!(h.reflection_dominance.level, HealthLevel::Good);
        assert_eq!(h.active_sources, 1);
        let src = &h.per_source[0];
        assert_eq!(src.status, HealthLevel::Good);
        assert!(src.reasons.iter().all(|r| !r.is_empty()));
        assert!(src.direct_reflected_ratio_db.is_infinite());
    }

    #[test]
    fn empty_scene_is_inactive() {
        let scene = SpatialScene::new(48_000);
        let meters = empty_meters(0.0);
        let h = build_health(default_inputs(&scene, &meters));
        assert_eq!(h.status, HealthLevel::Inactive);
        assert_eq!(h.active_sources, 0);
    }

    #[test]
    fn occlusion_severity_is_classified() {
        let mut scene = scene_with_objects();
        let obj = scene.objects.get_mut(ObjectId(0)).unwrap();
        obj.occlusion = Occlusion {
            amount: 0.8,
            ..Default::default()
        };
        let meters = empty_meters(0.0);
        let h = build_health(default_inputs(&scene, &meters));
        assert_eq!(h.occlusion.level, HealthLevel::Poor);
        assert_eq!(h.per_source[0].occlusion.level, HealthLevel::Poor);
        assert!(
            h.per_source[0].occlusion.note.contains('−'),
            "note carries the attenuation: {}",
            h.per_source[0].occlusion.note
        );
        assert_eq!(h.status, HealthLevel::Poor);
    }

    #[test]
    fn high_room_send_flags_reflection_dominance() {
        let mut scene = scene_with_objects();
        scene.room.enabled = true;
        let obj = scene.objects.get_mut(ObjectId(0)).unwrap();
        obj.room_send = 0.9; // wet dominates the direct path
        let meters = empty_meters(0.0);
        let h = build_health(default_inputs(&scene, &meters));
        assert_eq!(h.reflection_dominance.level, HealthLevel::Poor);
        assert_eq!(h.per_source[0].reflection.level, HealthLevel::Poor);
        // Ratio is finite and negative: reflected > direct.
        let ratio = h.per_source[0].direct_reflected_ratio_db;
        assert!(ratio.is_finite() && ratio < 0.0, "ratio {ratio}");
        assert!(
            h.reflection_dominance.note.contains("source 0"),
            "{}",
            h.reflection_dominance.note
        );
        assert_eq!(h.status, HealthLevel::Poor);
    }

    #[test]
    fn hrtf_coverage_drives_localization() {
        let scene = scene_with_objects();
        let meters = empty_meters(0.0);
        let mut inputs = default_inputs(&scene, &meters);
        inputs.hrtf = Some(HrtfCoverage {
            azimuth_min_deg: -30.0,
            azimuth_max_deg: 30.0,
            elevation_min_deg: -15.0,
            elevation_max_deg: 15.0,
        });
        // Object at +Y (az 0°, el 0°) → inside grid → Good.
        let h = build_health(inputs);
        assert_eq!(h.per_source[0].localization.level, HealthLevel::Good);

        // Move the object far off to the side → out of coverage.
        let mut scene2 = SpatialScene::new(48_000);
        scene2
            .objects
            .add(SpatialAudioObject::new(
                ObjectId(0),
                ObjectAudioRef::None,
                Vec3::new(8.0, 1.0, 3.0), // az ≈ 83°, el ≈ 20°
            ))
            .unwrap();
        let mut inputs2 = default_inputs(&scene2, &meters);
        inputs2.hrtf = inputs.hrtf;
        let h2 = build_health(inputs2);
        assert_eq!(h2.per_source[0].localization.level, HealthLevel::Moderate);
        assert!(
            h2.per_source[0]
                .localization
                .note
                .contains("out of measured grid"),
            "{}",
            h2.per_source[0].localization.note
        );
    }

    #[test]
    fn hrtf_grid_grace_and_range() {
        // Front hemisphere grid [-90°, 90°], full elevation span.
        let g = HrtfCoverage {
            azimuth_min_deg: -90.0,
            azimuth_max_deg: 90.0,
            elevation_min_deg: -40.0,
            elevation_max_deg: 40.0,
        };
        assert!(g.azimuth_covered(0.0, 0.0));
        assert!(g.azimuth_covered(89.0, 0.0));
        assert!(!g.azimuth_covered(190.0, 0.0));
        assert!(!g.azimuth_covered(-170.0, 0.0));
        assert!(g.azimuth_covered(95.0, 10.0)); // grace
        assert!(g.elevation_covered(38.0, 5.0));
        assert!(!g.elevation_covered(60.0, 5.0));
    }

    #[test]
    fn master_phase_risk_uses_measured_correlation() {
        let scene = scene_with_objects();
        let meters = empty_meters(0.3); // heavily decorrelated
        let h = build_health(default_inputs(&scene, &meters));
        assert_eq!(h.phase_risk.level, HealthLevel::Poor);
        assert!(h.phase_risk.note.contains("0.30"));
    }

    #[test]
    fn dropped_voices_raise_voice_pressure() {
        let scene = scene_with_objects();
        let meters = empty_meters(0.0);
        let mut inputs = default_inputs(&scene, &meters);
        inputs.voice_active = true;
        inputs.voice_full = 2;
        inputs.voice_degraded = 1;
        let h = build_health(inputs);
        assert_eq!(h.voice_pressure.level, HealthLevel::Moderate);
        inputs.voice_dropped = 1;
        let h = build_health(inputs);
        assert_eq!(h.voice_pressure.level, HealthLevel::Poor);
    }

    #[test]
    fn serde_round_trips_with_stable_codes() {
        // A finite ratio so the round-trip is lossless (an infinite ratio
        // serializes as JSON `null`, which is fine for hosts but not a
        // round-trippable f32).
        let mut scene = scene_with_objects();
        scene.room.enabled = true;
        let obj = scene.objects.get_mut(ObjectId(0)).unwrap();
        obj.room_send = 0.05;
        let meters = empty_meters(0.95);
        let h = build_health(default_inputs(&scene, &meters));
        let json = serde_json::to_string(&h).unwrap();
        let back: SpatialHealthSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.status, h.status);
        assert_eq!(back.per_source.len(), h.per_source.len());
        assert_eq!(back.stereo_correlation, h.stereo_correlation);
        assert!(back.direct_reflected_ratio_db.is_finite());
        assert_eq!(HealthLevel::Moderate.code(), "moderate");
        assert_eq!(HealthLevel::Poor.code(), "poor");
    }
}
