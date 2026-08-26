//! Engine commands for the multi-track lane registry (Phase 4 S6).

use crate::decode::Decoder;
use crate::dsp::graph::nodes::{DuckState, MAX_DUCK_TARGETS};
use crate::engine::lanes::LaneTrack;
use crate::source::AudioSource;

impl super::AudioEngine {
    /// Add a track as an independent lane on the first free mix-bus slot ≥ 2.
    /// Control path: opens the decoder, grows the bus generation if needed
    /// (glitch-free swap), and registers the lane. The decode loop picks it
    /// up at the next block boundary.
    pub(super) fn handle_add_track(&mut self, source: AudioSource) {
        let path = match &source {
            AudioSource::File(p) => p.clone(),
            _ => {
                log::warn!("AddTrack: only file sources are supported for lanes");
                return;
            }
        };
        let decoder = match Decoder::open(&path) {
            Ok(d) => d,
            Err(e) => {
                log::warn!("AddTrack: failed to open '{}': {}", path.display(), e);
                self.emit_event(crate::events::EngineEvent::Error(format!(
                    "Failed to open lane source '{}': {}",
                    path.display(),
                    e
                )));
                return;
            }
        };
        let Some(slot) = self.next_lane_slot() else {
            log::warn!("AddTrack: no free lane slots (bus full)");
            self.emit_event(crate::events::EngineEvent::Error(
                "AddTrack: mix bus has no free lane slots".into(),
            ));
            return;
        };
        // Grow the bus on demand via the glitch-free generation swap so the
        // lane's slot exists in the active generation.
        if self.config.mix_slots <= slot {
            self.config.mix_slots = slot + 1;
            self.graph.reconfigure(&self.config);
        }
        #[cfg(feature = "resample")]
        let lane = LaneTrack::open(
            slot,
            source.clone(),
            decoder,
            self.config.resampler_quality,
            self.output_sample_rate as f32,
            self.speed,
            self.config.precision_mode,
        );
        #[cfg(not(feature = "resample"))]
        let lane = LaneTrack::open(slot, source.clone(), decoder);
        // Make sure the slot is live on the bus.
        self.graph.set_input_active(slot as u8, true);
        self.lanes.push(lane);
        log::info!("Lane added on mix-bus slot {}: {}", slot, source);
    }

    /// Remove the lane on `slot` (if any) and detach the bus slot.
    pub(super) fn handle_remove_track(&mut self, slot: u8) {
        let before = self.lanes.len();
        self.lanes.retain(|l| l.slot != slot as usize);
        if self.lanes.len() != before {
            self.graph.set_input_active(slot, false);
            log::info!("Lane removed from mix-bus slot {slot}");
        }
    }

    /// Set a lane's linear gain in [0, 1] (clamped) and mirror it to the bus.
    pub(super) fn handle_set_track_gain(&mut self, slot: u8, gain: f32) {
        let gain = gain.clamp(0.0, 1.0);
        if let Some(lane) = self.lanes.iter_mut().find(|l| l.slot == slot as usize) {
            lane.gain = gain;
        }
        self.graph.set_input_gain(slot, gain);
    }

    /// Set a lane's pan in [-1, 1] (clamped) and mirror it to the bus.
    pub(super) fn handle_set_track_pan(&mut self, slot: u8, pan: f32) {
        let pan = pan.clamp(-1.0, 1.0);
        if let Some(lane) = self.lanes.iter_mut().find(|l| l.slot == slot as usize) {
            lane.pan = pan;
        }
        self.graph.set_input_pan(slot, pan);
    }

    /// Configure program-gated ducking across lanes (Phase 4 S4). An empty
    /// `targets` list disables ducking. `ms` values are converted to frames
    /// at the output sample rate.
    pub(super) fn handle_duck_tracks(
        &mut self,
        source_slot: u8,
        targets: Vec<u8>,
        threshold_db: f32,
        depth_db: f32,
        attack_ms: f32,
        release_ms: f32,
    ) {
        if targets.is_empty() {
            self.graph.set_duck(None);
            return;
        }
        let sr = self.output_sample_rate as f32;
        let mut fixed = [0usize; MAX_DUCK_TARGETS];
        let count = targets.len().min(MAX_DUCK_TARGETS);
        for (i, t) in targets.iter().take(count).enumerate() {
            fixed[i] = *t as usize;
        }
        let cfg = DuckState {
            source: source_slot as usize,
            threshold_db,
            depth_db,
            attack_frames: (attack_ms * 0.001 * sr) as usize,
            release_frames: (release_ms * 0.001 * sr) as usize,
            targets: fixed,
            target_count: count,
        };
        self.graph.set_duck(Some(cfg));
    }
}
