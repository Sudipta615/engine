//! Phase 2 — live graph swap: stable node identity and the swappable
//! generation container.
//!
//! A [`GraphGeneration`] is one complete, ownable graph configuration:
//! the node arena, the compiled [`PlanSet`] referencing that arena, and the
//! stable per-node identities. The audio thread executes the *active*
//! generation; the control thread builds a fresh generation and hands it over
//! through the publish / swap / retire handshake in `controls.rs`:
//!
//! 1. **Publish** (control): build a generation (allocation is fine here) and
//!    store its pointer in `ControlBus::pending`.
//! 2. **Swap** (audio, once per block in `control_tick`): swap `pending` into
//!    `active` and store the previous generation in `ControlBus::retired`.
//! 3. **Retire** (control): reclaim the returned generation (drop it) on the
//!    control path — the audio thread never allocates or frees.
//!
//! At most one swap is in flight at a time (the control side reclaims a
//! returned generation before publishing another, and coalesces a pending,
//! not-yet-swapped generation by replacing it), which bounds live memory to
//! 2 live generations + ≤1 in flight and makes reclamation trivially safe:
//! a generation returned through `retired` is guaranteed unreferenced.

use super::*;

/// Immutable snapshot of the listener-facing user state that a fresh
/// generation inherits: volume / balance / speed targets and the volume-ramp
/// duration. The audio thread mirrors these onto the control bus at every
/// drain; the control side seeds each new generation from a snapshot so a
/// reconfig never snaps the listener's settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UserState {
    /// Linear volume target in [0, 1].
    pub volume: f32,
    /// Balance in [-1, 1].
    pub balance: f32,
    /// Playback speed multiplier.
    pub speed: f32,
    /// Volume-ramp duration in milliseconds.
    pub volume_fade_ms: f32,
}

impl Default for UserState {
    fn default() -> Self {
        Self {
            volume: 1.0,
            balance: 0.0,
            speed: 1.0,
            volume_fade_ms: 10.0,
        }
    }
}

/// Stable identity of one node across generation swaps.
///
/// [`NodeIdx`] addresses a slot *inside one generation* (plans reference it);
/// [`NodeId`] addresses the persistent per-node SPSC control queue, which
/// lives in the graph shell and survives swaps. In the canonical Phase-2
/// layout `node_id` values and `NodeId` coincide numerically, so plans and
/// queues index the same table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct NodeId(pub usize);

impl NodeId {
    /// Queue slot for shell-level (transport / top-level) commands, which do
    /// not target any single node.
    pub const SHELL: NodeId = NodeId(node_id::NODE_COUNT);

    /// Total number of control-queue slots: one per canonical node plus the
    /// shell slot.
    pub const SLOTS: usize = node_id::NODE_COUNT + 1;
}

/// One complete, swappable graph configuration.
///
/// Owned by exactly one party at any time: the control thread while building
/// it and after it is returned via `retired`; the audio thread from the swap
/// until the next swap. The audio thread is the only mutator of the *active*
/// generation (parameter drain + processing), so `run_plan`'s disjoint
/// `plans` / `nodes` borrows remain sound.
///
/// Build a fresh configuration on the control side with
/// [`GraphGeneration::from_config`] and hand it to the audio thread via
/// [`GraphControlHandle::publish_generation`]; the swap itself happens at the
/// next block boundary and performs no allocation on the audio thread.
pub struct GraphGeneration {
    /// Node arena. The canonical 17-slot layout from Phase 1, but the swap
    /// machinery does not assume a fixed length.
    pub(super) nodes: Vec<GraphNode>,
    /// Compiled plans referencing this generation's arena slots.
    pub(super) plans: PlanSet,
    /// Stable identity per node, parallel to `nodes` (queue addressing).
    pub(super) node_ids: Vec<NodeId>,
}

impl GraphGeneration {
    /// The default Phase-2 layout: canonical node order, `NodeId(i)` for
    /// arena slot `i` (matching the `node_id` table).
    pub(super) fn canonical_ids(node_count: usize) -> Vec<NodeId> {
        (0..node_count).map(NodeId).collect()
    }
}
