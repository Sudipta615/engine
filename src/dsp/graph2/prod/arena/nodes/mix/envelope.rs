//! The TrackMixer-compatible transition pair law.
//!
//! `envelope_gains` is the *only* place the crossfade/fade curve math lives
//! in the bus; it reuses `TrackMixer`'s static gain functions verbatim so a
//! 2-input bus reproduces the pipeline's crossfade path bit-for-bit (pinned
//! by `tests/fidelity/graph_pipeline_equivalence`). Do not reorder or
//! re-formulate these expressions — the f32/f64 sums in [`super::sum`]
//! depend on the exact shape.

use crate::dsp::crossfade::{CrossfadeCurve, MixerState, TrackMixer};

use super::MixBusNode;

impl MixBusNode {
    /// Envelope gains `(input0, input1)` at normalized position `t`, using
    /// the exact `TrackMixer` math.
    #[inline]
    pub(crate) fn envelope_gains(state: MixerState, t: f32, curve: CrossfadeCurve) -> (f32, f32) {
        match state {
            MixerState::PlayingCurrent => (1.0, 0.0),
            MixerState::PlayingNext => (0.0, 1.0),
            MixerState::Silent => (0.0, 0.0),
            MixerState::Crossfading => TrackMixer::compute_gains_for_curve(t, curve),
            MixerState::Fading => TrackMixer::compute_fade_gains(t, curve),
        }
    }
}
