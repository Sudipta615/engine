//! Speaker geometry and layouts (spec Part IV §19–20).
//!
//! Speakers are pure **geometry** plus **calibration**, never a hard-coded
//! channel-name table: a 5.1, 7.1 or 7.1.4 system is simply a named preset
//! of a geometric [`SpeakerLayout`], and custom arrays are first-class.
//! Calibration (level trim, delay/alignment) is applied on top of geometry,
//! kept separate from the speaker's immutable identity.
//!
//! The coordinate convention is the spatial layer's single documented frame
//! (see [`crate::spatial::math`]): `+X = right, +Y = front, +Z = up`,
//! positions in metres.

use super::math::Vec3;
use crate::decode::ChannelId;

/// Stable identifier for a speaker within a [`SpeakerLayout`].
///
/// A bare index is surface-stable across the named presets (index 0 = first
/// speaker of the preset, etc.) so hosts can build per-speaker calibration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpeakerId(pub usize);

impl SpeakerId {
    pub fn into_index(self) -> usize {
        self.0
    }
}

/// A single speaker: geometric position in metres from the listener
/// reference, plus calibration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Speaker {
    pub id: SpeakerId,
    /// Position in metres relative to the layout's reference position.
    pub position: Vec3,
    /// Non-spatial (non-LFE) gain applied to this speaker, default 1.0.
    pub gain: f32,
    /// Static delay applied to this speaker when rendering (time
    /// alignment), in seconds.
    pub delay_secs: f32,
    /// Calibration trim in dB relative to the reference level.
    pub trim_db: f32,
    /// Whether this speaker is active in the render. Disabled speakers
    /// receive exactly zero from every object.
    pub enabled: bool,
    /// Semantic role, when the layout maps onto a conventional channel.
    /// `None` for arbitrary custom positions.
    pub role: Option<ChannelId>,
    /// True if this speaker is the LFE effects path (spec Part X) — never
    /// a spatial panning target.
    pub is_lfe: bool,
}

impl Speaker {
    pub fn new(position: Vec3) -> Self {
        Self {
            id: SpeakerId(0),
            position,
            gain: 1.0,
            delay_secs: 0.0,
            trim_db: 0.0,
            enabled: true,
            role: None,
            is_lfe: false,
        }
    }

    fn with_id(mut self, id: usize) -> Self {
        self.id = SpeakerId(id);
        self
    }
}

/// A collection of speakers plus the listener reference and calibration.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerLayout {
    pub speakers: Vec<Speaker>,
    /// Listener reference position for the layout (metres).
    pub reference_position: Vec3,
    /// Session-level calibration.
    pub calibration: LayoutCalibration,
}

impl SpeakerLayout {
    /// A front stereo pair at ±30° elevation 0, radius 2 m.
    pub fn stereo() -> Self {
        Self::custom_from_roles(vec![ChannelId::FrontLeft, ChannelId::FrontRight])
    }

    /// A 5.1 layout (ITU-R BS.775): FL/FR ±30°, C 0°, SL/SR ±110°, LFE.
    pub fn five_point_one() -> Self {
        Self::custom_from_roles(vec![
            ChannelId::FrontLeft,
            ChannelId::FrontRight,
            ChannelId::Center,
            ChannelId::Lfe,
            ChannelId::SideLeft,
            ChannelId::SideRight,
        ])
    }

    /// A 7.1 layout: 5.1 plus rear RL/RR at ±135°.
    pub fn seven_point_one() -> Self {
        Self::custom_from_roles(vec![
            ChannelId::FrontLeft,
            ChannelId::FrontRight,
            ChannelId::Center,
            ChannelId::Lfe,
            ChannelId::SideLeft,
            ChannelId::SideRight,
            ChannelId::RearLeft,
            ChannelId::RearRight,
        ])
    }

    /// A 7.1.4 layout: 7.1 plus four overheads (TFL/TFR/TRL/TRR at +35° to
    /// ~+45° elevation).
    pub fn seven_point_one_four() -> Self {
        Self::custom_from_roles(vec![
            ChannelId::FrontLeft,
            ChannelId::FrontRight,
            ChannelId::Center,
            ChannelId::Lfe,
            ChannelId::SideLeft,
            ChannelId::SideRight,
            ChannelId::RearLeft,
            ChannelId::RearRight,
            ChannelId::TopFrontLeft,
            ChannelId::TopFrontRight,
            ChannelId::TopRearLeft,
            ChannelId::TopRearRight,
        ])
    }

    /// Build a layout from a list of semantic channel roles, mapping each
    /// role to the engine's conventional geometry. Used to derive named
    /// presets from [`ChannelId`]s so render output slots line up with the
    /// engine's [`crate::decode::ChannelLayout`] channel ordering.
    pub fn custom_from_roles(roles: Vec<ChannelId>) -> Self {
        let mut speakers: Vec<Speaker> = Vec::with_capacity(roles.len());
        for (slot, role) in roles.iter().enumerate() {
            let mut s = Speaker::new(geometric_position(*role));
            s.role = Some(*role);
            s.is_lfe = *role == ChannelId::Lfe;
            if *role == ChannelId::Lfe {
                // The LFE effects path has no spatial pan; flag for the
                // renderer to skip as a panning target.
                s.enabled = false; // re-enabled only via LFE send path
            }
            speakers.push(s.with_id(slot));
        }
        Self {
            speakers,
            reference_position: Vec3::ZERO,
            calibration: LayoutCalibration::default(),
        }
    }

    /// Build a layout from arbitrary custom positions (in metres).
    pub fn custom(positions: Vec<Vec3>) -> Self {
        let speakers = positions
            .into_iter()
            .enumerate()
            .map(|(i, p)| Speaker::new(p).with_id(i))
            .collect();
        Self {
            speakers,
            reference_position: Vec3::ZERO,
            calibration: LayoutCalibration::default(),
        }
    }

    /// Number of speakers (pan-capable speakers only, excluding LFE).
    pub fn pan_speaker_count(&self) -> usize {
        self.speakers
            .iter()
            .filter(|s| !s.is_lfe && s.enabled)
            .count()
    }

    /// Optimise/validate geometry. Returns `Err(DegenerateGeometry)` if the
    /// layout produced no valid pan target. This is the control-path entry
    /// the renderer calls from `prepare`; it never runs on the audio thread.
    pub fn validate(&self) -> Result<(), RenderGeometryError> {
        if self.speakers.is_empty() {
            return Err(RenderGeometryError::InvalidLayout);
        }
        if self.pan_speaker_count() == 0 {
            return Err(RenderGeometryError::DegenerateGeometry);
        }
        Ok(())
    }
}

/// Geometric position (metres, from the listener) for a conventional role.
/// The angles follow ITU-R BS.775 / Atmos-common conventions at a nominal
/// 2 m radius and ~35° elevation for height channels.
fn geometric_position(role: ChannelId) -> Vec3 {
    const R: f32 = 2.0;
    const ELEV: f32 = 0.610_865; // 35° in radians
    let horiz = |azimuth_degrees: f32, elevation: f32| -> Vec3 {
        let az = azimuth_degrees.to_radians();
        let x = R * elevation.cos() * az.sin();
        let y = R * elevation.cos() * az.cos();
        let z = R * elevation.sin();
        Vec3::new(x, y, z)
    };
    match role {
        ChannelId::FrontLeft => horiz(-30.0, 0.0),
        ChannelId::FrontRight => horiz(30.0, 0.0),
        ChannelId::Center => horiz(0.0, 0.0),
        ChannelId::Lfe => Vec3::ZERO, // LFE has no spatial position
        ChannelId::SideLeft => horiz(-110.0, 0.0),
        ChannelId::SideRight => horiz(110.0, 0.0),
        ChannelId::RearLeft => horiz(-135.0, 0.0),
        ChannelId::RearRight => horiz(135.0, 0.0),
        ChannelId::BackCenter => horiz(180.0, 0.0),
        ChannelId::TopFrontLeft => horiz(-30.0, ELEV),
        ChannelId::TopFrontRight => horiz(30.0, ELEV),
        ChannelId::TopRearLeft => horiz(-135.0, ELEV),
        ChannelId::TopRearRight => horiz(135.0, ELEV),
        ChannelId::Unknown(_) => Vec3::ZERO,
    }
}

/// Session calibration applied on top of speaker geometry (spec §20).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LayoutCalibration {
    /// Reference level trim (dB) applied to every speaker before per-speaker
    /// trim.
    pub level_db: f32,
    /// Per-speaker trim (dB keyed by [`SpeakerId`]) — applied separately
    /// from the speaker's immutable geometry.
    pub per_speaker_trim_db: Vec<(SpeakerId, f32)>,
    /// Per-speaker time-alignment delay (seconds keyed by [`SpeakerId`]) —
    /// distinct from the static `Speaker::delay_secs` for session tuning.
    pub per_speaker_delay_secs: Vec<(SpeakerId, f32)>,
}

impl LayoutCalibration {
    /// Linear trim multiplier for a speaker, combining session level with
    /// any per-speaker trim in dB.
    pub fn trim_gain(&self, id: SpeakerId) -> f32 {
        let per = self
            .per_speaker_trim_db
            .iter()
            .find(|(sid, _)| *sid == id)
            .map(|(_, db)| db)
            .copied()
            .unwrap_or(0.0);
        db_to_linear(self.level_db + per)
    }

    /// Session time-alignment delay (seconds) for a speaker.
    pub fn delay(&self, id: SpeakerId) -> f32 {
        self.per_speaker_delay_secs
            .iter()
            .find(|(sid, _)| *sid == id)
            .map(|(_, d)| *d)
            .unwrap_or(0.0)
    }
}

#[inline]
pub(crate) fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

/// Geometry validation failure (spec §106).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderGeometryError {
    InvalidLayout,
    DegenerateGeometry,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_presets_have_expected_counts() {
        assert_eq!(SpeakerLayout::stereo().speakers.len(), 2);
        assert_eq!(SpeakerLayout::five_point_one().speakers.len(), 6);
        assert_eq!(SpeakerLayout::seven_point_one().speakers.len(), 8);
        assert_eq!(SpeakerLayout::seven_point_one_four().speakers.len(), 12);
    }

    #[test]
    fn lfe_is_excluded_from_pan_targets() {
        let layout = SpeakerLayout::five_point_one();
        assert_eq!(layout.pan_speaker_count(), 5);
        // Only one LFE speaker flagged.
        let lfe = layout.speakers.iter().filter(|s| s.is_lfe).count();
        assert_eq!(lfe, 1);
    }

    #[test]
    fn stereo_front_speakers_are_left_right_geometry() {
        let layout = SpeakerLayout::stereo();
        let fl = layout.speakers[0];
        let fr = layout.speakers[1];
        assert!(fl.position.x < 0.0 && fr.position.x > 0.0);
        assert!((fl.position.y - fr.position.y).abs() < 1e-5); // symmetric front
    }

    #[test]
    fn validate_rejects_empty_and_lfe_only() {
        let empty = SpeakerLayout::custom(vec![]);
        assert!(matches!(
            empty.validate(),
            Err(RenderGeometryError::InvalidLayout)
        ));
        // A layout with only an LFE is degenerate for spatial panning.
        let lfe_only = SpeakerLayout {
            speakers: vec![Speaker {
                is_lfe: true,
                ..Speaker::new(Vec3::ZERO)
            }
            .with_id(0)],
            reference_position: Vec3::ZERO,
            calibration: LayoutCalibration::default(),
        };
        assert!(matches!(
            lfe_only.validate(),
            Err(RenderGeometryError::DegenerateGeometry)
        ));
    }

    #[test]
    fn calibration_trims_apply_deterministically() {
        // Session level 0, with a -6 dB per-speaker trim on speaker 0 only.
        let cal = LayoutCalibration {
            level_db: 0.0,
            per_speaker_trim_db: vec![(SpeakerId(0), -6.0)],
            per_speaker_delay_secs: vec![],
        };
        // -6 dB trim => ≈0.5012 linear (10^{-6/20}).
        let g = cal.trim_gain(SpeakerId(0));
        assert!((g - 0.5012).abs() < 1e-3);
        // An untrimmed speaker stays at unity (only the session level).
        assert!((cal.trim_gain(SpeakerId(1)) - 1.0).abs() < 1e-3);

        // A -6 dB session level applies to every speaker.
        let cal = LayoutCalibration {
            level_db: -6.0,
            per_speaker_trim_db: vec![(SpeakerId(0), -6.0)],
            per_speaker_delay_secs: vec![],
        };
        assert!((cal.trim_gain(SpeakerId(1)) - 0.5012).abs() < 1e-3);
        // Combined -6 (session) + -6 (per-speaker) = -12 dB ≈ 0.251 on spk 0.
        assert!((cal.trim_gain(SpeakerId(0)) - 0.251).abs() < 1e-3);
    }

    #[test]
    fn calibration_time_alignment_is_reported() {
        let cal = LayoutCalibration {
            level_db: 0.0,
            per_speaker_trim_db: vec![],
            per_speaker_delay_secs: vec![(SpeakerId(2), 0.005)],
        };
        assert_eq!(cal.delay(SpeakerId(2)), 0.005);
        assert_eq!(cal.delay(SpeakerId(0)), 0.0);
    }
}
