//! Spatial voice management (spec §76).
//!
//! A large scene may contain more objects than can be rendered at maximum
//! quality within the real-time budget. The renderer is free to **hide** or
//! **degrade** voices, but the decision must be deterministic and never
//! unbounded. This module provides:
//!
//! - [`VoicePriority`] — the priority policy (fixed order, distance-weighted,
//!   gain-weighted, or user-defined).
//! - [`VoiceBudget`] — a deterministic control-path scheduler that, given the
//!   active objects and a hard capacity, returns the set of slots that render
//!   at full quality vs. degraded, and which slots are dropped entirely.
//!
//! The scheduler is the spec's *voice policy*; renderers keep a hard ceiling
//! (`MAX_SPATIAL_OBJECTS`) and consume the budget's [`VoicePlan`] in a single
//! allocation-free pass. Budgeting accuracy is bounded by the plan's
//! monotonic heuristics and never allocates per block.

use super::math::Vec3;

/// How voices are ranked for the budget (spec §76).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VoicePriority {
    /// Fixed scene/authored order (slot order wins).
    #[default]
    Fixed,
    /// Closer objects are prioritised (distance ascends).
    DistanceWeighted,
    /// Louder objects are prioritised (gain·typical-distance descends).
    GainWeighted,
    /// Host-provided priority order (highest `priority()` wins).
    UserDefined,
}

/// One object's admission decision for a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceAdmission {
    /// Rendered at full configured quality.
    Full,
    /// Rendered but a candidate for quality degradation (e.g. fewer spread
    /// samples, cheaper room).
    Degraded,
    /// Silenced for this block. Deterministic; its smoothed state still
    /// advances so re-admission does not click.
    Dropped,
}

/// A deterministic per-block voice plan.
#[derive(Debug, Clone)]
pub struct VoicePlan {
    /// `admission[slot]` for every object slot `0..n_slots`.
    pub admission: Vec<VoiceAdmission>,
    /// Number of full-quality voices admitted.
    pub full_count: usize,
    /// Number of degraded voices.
    pub degraded_count: usize,
}

impl VoicePlan {
    pub fn dropped_count(&self) -> usize {
        self.admission
            .iter()
            .filter(|a| **a == VoiceAdmission::Dropped)
            .count()
    }
}

/// Inputs the scheduler uses to rank a candidate object.
#[derive(Debug, Clone, Copy)]
pub struct BudgetCandidate {
    pub index: usize,
    pub gain: f32,
    pub distance: f32,
    pub priority: u32, // host authored priority (UserDefined)
}

/// A control-path voice budget scheduler.
#[derive(Debug, Clone)]
pub struct VoiceBudget {
    /// Hard per-scene capacity (voices active simultaneously, any quality).
    pub capacity: usize,
    /// Hard full-quality sub-capacity (≥ 0; voices beyond this are degraded).
    pub full_quality_capacity: usize,
    /// How to rank candidates.
    pub policy: VoicePriority,
}

impl Default for VoiceBudget {
    fn default() -> Self {
        Self {
            capacity: 48,
            full_quality_capacity: 24,
            policy: VoicePriority::default(),
        }
    }
}

impl VoiceBudget {
    /// Rank a candidate object by the policy. Higher = higher priority.
    /// Deterministic; ties break by index.
    fn score(&self, p: &BudgetCandidate) -> (u32, f32, usize) {
        // Returns (wall_group, score_desc, index); ordering sorts descending.
        let (group, score) = match self.policy {
            VoicePriority::Fixed => (0u32, f32::MAX - p.index as f32),
            VoicePriority::DistanceWeighted => (0u32, -p.distance),
            VoicePriority::GainWeighted => (0u32, p.gain),
            VoicePriority::UserDefined => (0u32, p.priority as f32),
        };
        (group, score, p.index)
    }

    /// Build the per-slot plan for `candidates` over `n_slots` slots. The
    /// top-`capacity` candidates by the policy are admitted; the top
    /// `full_quality_capacity` of those are `Full`, the rest `Degraded`,
    /// everybody else `Dropped`. Allocation-free aside from the returned
    /// plan (control path).
    pub fn plan(&self, candidates: &[BudgetCandidate], n_slots: usize) -> VoicePlan {
        let mut order: Vec<&BudgetCandidate> = candidates.iter().collect();
        order.sort_by(|a, b| {
            let (ga, sa, ia) = self.score(a);
            let (gb, sb, ib) = self.score(b);
            ga.cmp(&gb)
                .reverse()
                .then_with(|| sa.total_cmp(&sb).reverse())
                .then_with(|| ia.cmp(&ib))
        });

        // Drop slots beyond capacity entirely.
        let dropped_slots: Vec<usize> = order[order.len().min(self.capacity)..]
            .iter()
            .map(|c| c.index)
            .collect();
        let full_slots = &order[..order.len().min(self.full_quality_capacity)];
        let degraded_range =
            &order[order.len().min(self.full_quality_capacity)..order.len().min(self.capacity)];

        let mut admission = vec![VoiceAdmission::Dropped; n_slots];
        for c in full_slots {
            if c.index < n_slots {
                admission[c.index] = VoiceAdmission::Full;
            }
        }
        for c in degraded_range {
            if c.index < n_slots {
                admission[c.index] = VoiceAdmission::Degraded;
            }
        }
        let _ = dropped_slots;
        let full_count = admission
            .iter()
            .filter(|a| **a == VoiceAdmission::Full)
            .count();
        let degraded_count = admission
            .iter()
            .filter(|a| **a == VoiceAdmission::Degraded)
            .count();
        VoicePlan {
            admission,
            full_count,
            degraded_count,
        }
    }

    /// Realtime-safe variant of [`Self::plan`]: writes the per-slot
    /// admission directly into a caller-supplied `out` buffer using a
    /// caller-supplied `scratch` index array, with **no heap allocation**.
    ///
    /// - `scratch.len() >= candidates.len()` (used as a sort-staging area).
    /// - `out.len() >= n_slots`; each slot is initially written `Dropped`
    ///   then upgraded to `Degraded` / `Full` as the budget allows.
    ///
    /// Selection-rank ranking (O(n·k)) keeps this allocation-free, so a
    /// renderer can run the voice budget on the audio path (spec §76: voice
    /// management must stay within the real-time budget). The produced plan
    /// is identical to [`Self::plan`]'s.
    pub fn plan_into(
        &self,
        candidates: &[BudgetCandidate],
        n_slots: usize,
        scratch: &mut [usize],
        out: &mut [VoiceAdmission],
    ) {
        let fill = n_slots.min(out.len());
        for a in out[..fill].iter_mut() {
            *a = VoiceAdmission::Dropped;
        }
        let n = candidates.len().min(fill);
        let cap = self.capacity.min(n);
        let fqc = self.full_quality_capacity.min(cap);
        let o = &mut scratch[..cap]; // candidate-array positions, rank-ordered
                                     // Selection rank: each rank takes the max-score candidate not yet
                                     // chosen. Ties break group → score → candidate index, matching `plan`,
                                     // while `o` records candidate-array positions (to re-read slot ids).
        for r in 0..cap {
            let mut best: Option<(u32, f32, usize)> = None;
            for i in 0..n {
                if o[..r].contains(&i) {
                    continue;
                }
                let (g, s, _) = self.score(&candidates[i]);
                let cidx = candidates[i].index;
                let better = match best {
                    None => true,
                    Some((bg, bs, bi)) => {
                        g > bg || (g == bg && (s > bs || (s == bs && cidx < candidates[bi].index)))
                    }
                };
                if better {
                    best = Some((g, s, i));
                }
            }
            if let Some((_, _, pos)) = best {
                o[r] = pos;
            }
        }
        for &pos in &o[..fqc] {
            let slot = candidates[pos].index;
            if let Some(a) = out.get_mut(slot) {
                *a = VoiceAdmission::Full;
            }
        }
        for &pos in &o[fqc..cap] {
            let slot = candidates[pos].index;
            if let Some(a) = out.get_mut(slot) {
                *a = VoiceAdmission::Degraded;
            }
        }
    }

    /// Convenience: rank an object by a positional policy from a
    /// listener-space position and gain.
    pub fn candidate(
        index: usize,
        gain: f32,
        listener_space_pos: Vec3,
        user_priority: u32,
    ) -> BudgetCandidate {
        BudgetCandidate {
            index,
            gain,
            distance: listener_space_pos.length(),
            priority: user_priority,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cands() -> Vec<BudgetCandidate> {
        vec![
            BudgetCandidate {
                index: 0,
                gain: 0.2,
                distance: 10.0,
                priority: 1,
            },
            BudgetCandidate {
                index: 1,
                gain: 0.9,
                distance: 1.0,
                priority: 5,
            },
            BudgetCandidate {
                index: 2,
                gain: 0.5,
                distance: 3.0,
                priority: 2,
            },
            BudgetCandidate {
                index: 3,
                gain: 0.1,
                distance: 8.0,
                priority: 3,
            },
        ]
    }

    #[test]
    fn fixed_policy_keeps_slot_order() {
        let b = VoiceBudget {
            capacity: 3,
            full_quality_capacity: 2,
            policy: VoicePriority::Fixed,
        };
        let plan = b.plan(&cands(), 4);
        assert_eq!(plan.admission[0], VoiceAdmission::Full);
        assert_eq!(plan.admission[1], VoiceAdmission::Full);
        assert_eq!(plan.admission[2], VoiceAdmission::Degraded);
        assert_eq!(plan.admission[3], VoiceAdmission::Dropped);
        assert_eq!(plan.full_count, 2);
        assert_eq!(plan.degraded_count, 1);
        assert_eq!(plan.dropped_count(), 1);
    }

    #[test]
    fn distance_policy_prioritises_near() {
        let b = VoiceBudget {
            capacity: 2,
            full_quality_capacity: 1,
            policy: VoicePriority::DistanceWeighted,
        };
        let plan = b.plan(&cands(), 4);
        // nearest (idx 1, dist 1) is Full; next (idx 2, dist 3) is Degraded.
        assert_eq!(plan.admission[1], VoiceAdmission::Full);
        assert_eq!(plan.admission[2], VoiceAdmission::Degraded);
        assert_eq!(plan.admission[0], VoiceAdmission::Dropped);
        assert_eq!(plan.admission[3], VoiceAdmission::Dropped);
    }

    #[test]
    fn capacity_zero_drops_everything_but_stays_deterministic() {
        let b = VoiceBudget {
            capacity: 0,
            full_quality_capacity: 0,
            policy: VoicePriority::GainWeighted,
        };
        let plan = b.plan(&cands(), 4);
        assert_eq!(plan.dropped_count(), 4);
        assert_eq!(plan.full_count, 0);
        // Repeatable.
        let plan2 = b.plan(&cands(), 4);
        assert_eq!(plan.admission, plan2.admission);
    }

    #[test]
    fn plan_into_matches_plan_no_allocation() {
        let budgets = [
            VoiceBudget {
                capacity: 3,
                full_quality_capacity: 2,
                policy: VoicePriority::Fixed,
            },
            VoiceBudget {
                capacity: 2,
                full_quality_capacity: 1,
                policy: VoicePriority::DistanceWeighted,
            },
            VoiceBudget {
                capacity: 2,
                full_quality_capacity: 2,
                policy: VoicePriority::UserDefined,
            },
        ];
        let cs = cands();
        for b in &budgets {
            let plan = b.plan(&cs, 4);
            let mut scratch = [0usize; 8];
            let mut out = [VoiceAdmission::Dropped; 8];
            b.plan_into(&cs, 4, &mut scratch[..cs.len()], &mut out[..]);
            for (s, (oi, pi)) in out[..4].iter().zip(plan.admission.iter()).enumerate() {
                assert_eq!(
                    oi, pi,
                    "policy {:?} slot {s}: {:?} vs {:?}",
                    b.policy, pi, oi
                );
            }
        }
    }

    #[test]
    fn user_defined_respects_priority_field() {
        let b = VoiceBudget {
            capacity: 2,
            full_quality_capacity: 2,
            policy: VoicePriority::UserDefined,
        };
        let plan = b.plan(&cands(), 4);
        // idx1 (priority 5) and idx3 (priority 3) admitted full.
        assert_eq!(plan.admission[1], VoiceAdmission::Full);
        assert_eq!(plan.admission[3], VoiceAdmission::Full);
        assert_eq!(plan.admission[0], VoiceAdmission::Dropped);
        assert_eq!(plan.admission[2], VoiceAdmission::Dropped);
    }
}
