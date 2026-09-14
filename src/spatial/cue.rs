//! Scene animation **cues** (v4.4.0): named, composable trigger
//! events with parameter-curve payloads, evaluated on the spatial node.
//!
//! A cue is the *event* half of scene animation: object automation drives
//! parameters on a global scene clock, while a cue fires **by name** at an
//! arbitrary moment (`EngineCommand::TriggerSpatialCue`, the timeline
//! scheduler, or a host) and drives its target object **relative to the
//! firing instant** — sample-accurate on the block boundary at which it
//! lands, then evaluated block-by-block like every other automation curve.
//!
//! ```text
//! host: TriggerSpatialCue { name: "whoosh" }
//!   └─ control thread: cue bank lookup (plain data)
//!       └─ block boundary: activate (snapshot clock t0)
//!           └─ per block: overlay evaluate(t − t0) onto the target
//!               └─ finish (or loop): release / hold
//! ```
//!
//! **Realtime discipline.** The bank is built on the control thread (a
//! `Vec` of plain-data cues, swapped in whole — the same contract as the
//! plugin parameter batch); the audio thread only ever reads it and
//! advances a fixed-size activation cursor. Evaluation reuses the
//! allocation-free [`CurveScalar`] / [`CurveVec3`] kernels, so the audio
//! path stays allocation-free and lock-free.
//!
//! Cue semantics:
//!
//! - **Overlay** — an active cue's curves *override* the target's authored
//!   parameters for the parameters it carries (the same semantics as
//!   object automation: absent curves leave the parameter untouched).
//! - **Single activation per target** — firing a new cue on a target
//!   replaces that target's active cue (last-wins; the plan step is
//!   branch-free).
//! - **looping** — the cue-relative clock wraps at the cue duration; the
//!   cue never finishes until stopped.
//! - **hold** — at the end, the final keyframe value persists instead of
//!   releasing back to the authored parameters (an automation-style tail).
//!
//! The `whoosh` / `door` presets live in the config crate
//! ([`config::SpatialCueConfig::whoosh`] / `::door`); this module defines
//! the runtime twin.

use super::automation::{CurveScalar, CurveVec3};
use super::math::Vec3;

/// The maximum number of simultaneously **active** cue targets (one per
/// program object — a stereo master has two; the cap covers future
/// multichannel scene routing). Fixed so activation state is a plain
/// array, allocation-free on the audio path.
pub const MAX_ACTIVE_CUES: usize = 8;

/// The runtime twin of [`config::SpatialCueConfig`]: a named, plain-data
/// cue with owned curves. Built on the control thread, read on the audio
/// thread.
#[derive(Debug, Clone, PartialEq)]
pub struct SpatialCue {
    pub name: String,
    /// The program object the cue drives (`0` = L, `1` = R).
    pub target: usize,
    pub gain: Option<CurveScalar>,
    pub spread: Option<CurveScalar>,
    pub position: Option<CurveVec3>,
    pub looping: bool,
    pub hold: bool,
}

impl SpatialCue {
    /// Build the runtime cue from the scene-file model (control path;
    /// allocates). Returns `None` for a cue with no curves (validation
    /// rejects those, but the constructor stays total).
    pub fn from_config(cfg: &config::SpatialCueConfig) -> Option<Self> {
        if !cfg.has_any() {
            return None;
        }
        Some(Self {
            name: cfg.name.clone(),
            target: cfg.target,
            gain: cfg
                .gain
                .as_ref()
                .and_then(|c| CurveScalar::from_points(&c.points)),
            spread: cfg
                .spread
                .as_ref()
                .and_then(|c| CurveScalar::from_points(&c.points)),
            position: cfg.position.as_ref().and_then(|c| {
                CurveVec3::from_points(
                    &c.points
                        .iter()
                        .map(|(t, p)| (*t, Vec3::new(p[0], p[1], p[2])))
                        .collect::<Vec<_>>(),
                )
            }),
            looping: cfg.looping,
            hold: cfg.hold,
        })
    }

    /// The cue's duration in seconds (the last keyframe across its
    /// curves; `0.0` when a curve holds a single point).
    pub fn duration_secs(&self) -> f32 {
        let end = |c: &CurveScalar| c.keyframes().last().map(|k| k.0).unwrap_or(0.0);
        let end3 = |c: &CurveVec3| c.keyframes().last().map(|k| k.0).unwrap_or(0.0);
        self.gain.as_ref().map(end).unwrap_or(0.0).max(
            self.spread
                .as_ref()
                .map(end)
                .unwrap_or(0.0)
                .max(self.position.as_ref().map(end3).unwrap_or(0.0)),
        )
    }

    /// Whether the cue drives any parameter.
    pub fn has_any(&self) -> bool {
        self.gain.is_some() || self.spread.is_some() || self.position.is_some()
    }
}

/// The runtime cue bank (control-built, audio-read): the scene's named
/// cues plus the fixed-size activation state. All mutation happens at the
/// block boundary (the `NodeCmd` drain); the struct is plain data owned by
/// the spatial node.
#[derive(Debug, Clone, PartialEq)]
pub struct CueBank {
    /// The scene's named cues (control thread writes the whole Vec).
    cues: Vec<SpatialCue>,
    /// One activation per target slot: `(cue index into cues, clock t0)`.
    /// `None` = idle. The audio thread replaces/ clears entries in place.
    active: [Option<(usize, f32)>; MAX_ACTIVE_CUES],
}

impl Default for CueBank {
    fn default() -> Self {
        Self::new()
    }
}

impl CueBank {
    pub fn new() -> Self {
        Self {
            cues: Vec::new(),
            active: [None; MAX_ACTIVE_CUES],
        }
    }

    /// Replace the whole bank (control path; allocates the new Vec on the
    /// control thread, then it is only read).
    pub fn replace(&mut self, cues: Vec<SpatialCue>) {
        // Names are validated unique upstream; keep the bank canonical.
        self.cues = cues;
        self.active = [None; MAX_ACTIVE_CUES];
    }

    /// The number of cues in the bank.
    pub fn len(&self) -> usize {
        self.cues.len()
    }

    /// Whether the bank holds no cues.
    pub fn is_empty(&self) -> bool {
        self.cues.is_empty()
    }

    /// The bank's cue names in index order (control introspection and
    /// scene round-trips).
    pub fn cue_names(&self) -> Vec<&str> {
        self.cues.iter().map(|c| c.name.as_str()).collect()
    }

    /// The named cue's index (control path lookup — the trigger command
    /// resolves the name once and rides the queue as an index).
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.cues.iter().position(|c| c.name == name)
    }

    /// A cue by index (control path).
    pub fn cue(&self, index: usize) -> Option<&SpatialCue> {
        self.cues.get(index)
    }

    /// Activate a cue on its target at the block boundary (audio drain —
    /// allocation-free). Last-wins per target; unknown indices no-op.
    pub fn trigger(&mut self, cue_index: usize, now_secs: f32) {
        let Some(cue) = self.cues.get(cue_index) else {
            return;
        };
        let target = cue.target.min(MAX_ACTIVE_CUES - 1);
        self.active[target] = Some((cue_index, now_secs));
    }

    /// Stop the active cue on a target (audio drain — allocation-free).
    pub fn stop(&mut self, target: usize) {
        if target < MAX_ACTIVE_CUES {
            self.active[target] = None;
        }
    }

    /// Stop every active cue (audio drain).
    pub fn stop_all(&mut self) {
        self.active = [None; MAX_ACTIVE_CUES];
    }

    /// Whether a target currently has an active cue.
    pub fn is_active(&self, target: usize) -> bool {
        target < MAX_ACTIVE_CUES && self.active[target].is_some()
    }

    /// The cue index active on a target (control introspection).
    pub fn active_index(&self, target: usize) -> Option<usize> {
        if target >= MAX_ACTIVE_CUES {
            return None;
        }
        self.active[target].map(|(i, _)| i)
    }

    /// Advance/retire active cues at scene clock `now_secs` (audio path,
    /// allocation-free). Called by the spatial node once per block, after
    /// advancing its cue clock: every finished, non-holding, non-looping
    /// cue releases its target back to the authored parameters.
    pub fn step(&mut self, now_secs: f32) {
        for target in 0..MAX_ACTIVE_CUES {
            let Some((cue_index, t0)) = self.active[target] else {
                continue;
            };
            let Some(cue) = self.cues.get(cue_index) else {
                self.active[target] = None;
                continue;
            };
            if cue.looping {
                continue;
            }
            let rel = now_secs - t0;
            if rel >= cue.duration_secs() && !cue.hold {
                self.active[target] = None;
            }
        }
    }

    /// Evaluate the active cue on a target at scene clock `now_secs`
    /// (audio path, allocation-free). Writes only the parameters the cue
    /// carries into `out`; leaves the rest untouched.
    pub fn evaluate(&self, target: usize, now_secs: f32, out: &mut CueOverlay) {
        let Some((cue_index, t0)) = self.active.get(target).copied().flatten() else {
            return;
        };
        let Some(cue) = self.cues.get(cue_index) else {
            return;
        };
        let duration = cue.duration_secs();
        let mut rel = now_secs - t0;
        if cue.looping && duration > 0.0 {
            rel = rel.rem_euclid(duration);
        } else if rel > duration && cue.hold {
            rel = duration;
        }
        if rel < 0.0 {
            rel = 0.0;
        } else if rel > duration {
            return;
        }
        if let Some(c) = &cue.gain {
            out.gain = Some(c.evaluate(rel));
        }
        if let Some(c) = &cue.spread {
            out.spread = Some(c.evaluate(rel));
        }
        if let Some(c) = &cue.position {
            out.position = Some(c.evaluate(rel));
        }
    }
}

/// One evaluated cue frame for a target (the overlay the node composes
/// onto the target's parameters — same shape as the automation frame).
#[derive(Debug, Clone, Copy, Default)]
pub struct CueOverlay {
    pub gain: Option<f32>,
    pub spread: Option<f32>,
    pub position: Option<Vec3>,
}

impl CueOverlay {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn is_none(&self) -> bool {
        self.gain.is_none() && self.spread.is_none() && self.position.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn swell_cue(name: &str, target: usize, looping: bool, hold: bool) -> SpatialCue {
        SpatialCue {
            name: name.to_string(),
            target,
            gain: CurveScalar::from_points(&[(0.0, 0.0), (0.5, 1.0), (1.0, 0.0)]),
            spread: None,
            position: None,
            looping,
            hold,
        }
    }

    #[test]
    fn trigger_evaluates_relatively_and_releases() {
        let mut bank = CueBank::new();
        bank.replace(vec![swell_cue("swell", 0, false, false)]);
        let idx = bank.index_of("swell").unwrap();
        bank.trigger(idx, 10.0);
        assert!(bank.is_active(0));
        let mut o = CueOverlay::none();
        bank.evaluate(0, 10.0, &mut o);
        assert_eq!(o.gain, Some(0.0));
        bank.evaluate(0, 10.5, &mut o);
        assert_eq!(o.gain, Some(1.0));
        // Past the end + non-holding: step releases; evaluate no-ops.
        bank.step(11.0);
        assert!(!bank.is_active(0));
        let mut o2 = CueOverlay::none();
        bank.evaluate(0, 11.0, &mut o2);
        assert!(o2.is_none());
    }

    #[test]
    fn holding_cue_persists_past_end() {
        let mut bank = CueBank::new();
        bank.replace(vec![swell_cue("hold", 1, false, true)]);
        bank.trigger(0, 0.0);
        bank.step(0.0);
        assert!(bank.is_active(1));
        let mut o = CueOverlay::none();
        bank.evaluate(1, 5.0, &mut o);
        assert_eq!(o.gain, Some(0.0), "held at final keyframe");
        assert!(bank.is_active(1));
    }

    #[test]
    fn looping_cue_wraps_the_clock() {
        let mut bank = CueBank::new();
        bank.replace(vec![swell_cue("loop", 0, true, false)]);
        bank.trigger(0, 0.0);
        let mut o = CueOverlay::none();
        bank.evaluate(0, 1.25, &mut o);
        assert_eq!(o.gain, Some(0.5), "1.25 wraps to 0.25 → half-swelling");
        // looping cues never finish.
        bank.step(5.0);
        assert!(bank.is_active(0));
    }

    #[test]
    fn last_wins_per_target_and_stop_clears() {
        let mut bank = CueBank::new();
        bank.replace(vec![
            swell_cue("a", 0, false, false),
            swell_cue("b", 0, false, false),
        ]);
        bank.trigger(0, 0.0);
        bank.trigger(1, 5.0);
        assert_eq!(bank.active_index(0), Some(1));
        bank.stop(0);
        assert!(!bank.is_active(0));
    }

    #[test]
    fn unknown_trigger_and_oob_target_are_noops() {
        let mut bank = CueBank::new();
        bank.replace(vec![swell_cue("x", 99, false, false)]);
        bank.trigger(7, 0.0); // unknown cue index
        bank.trigger(0, 0.0); // target 99 clamps to the last slot
        assert!(bank.cue(0).is_some());
    }
}
