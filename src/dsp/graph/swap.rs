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

/// One mix-bus slot's listener-facing user state (Phase 4 S1). Slots 0/1 are
/// the transition pair; slots >= 2 are independent lanes. Mirrored from the
/// audio side at drain and replayed into fresh generations so a reconfig
/// never snaps a lane's gain / balance / mute / detachment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlotState {
    /// Linear user gain target in [0, 1].
    pub gain: f32,
    /// Balance in [-1, 1] (0 = center).
    pub balance: f32,
    /// Pan in [-1, 1] (0 = center).
    pub pan: f32,
    /// Muted: the slot contributes silence.
    pub mute: bool,
    /// Detached: the slot contributes nothing and its chains do not advance.
    /// Slot 0 is never detached.
    pub active: bool,
    /// Post-fader master-send gain in [0, 1] (Phase 5 S2).
    pub send_master_gain: f32,
    /// Post-fader aux-send gain in [0, 1] (Phase 5 S2).
    pub send_aux_gain: f32,
    /// Per-channel trim gains (linear, default 1.0), index by channel
    /// (Phase 5 S1).
    pub trim_gains: [f32; MAX_CHANNELS],
    /// Per-channel polarity inversion (Phase 5 S1).
    pub trim_invert: [bool; MAX_CHANNELS],
}

impl Default for SlotState {
    fn default() -> Self {
        Self {
            gain: 1.0,
            balance: 0.0,
            pan: 0.0,
            mute: false,
            active: true,
            send_master_gain: 1.0,
            send_aux_gain: 0.0,
            trim_gains: [1.0; MAX_CHANNELS],
            trim_invert: [false; MAX_CHANNELS],
        }
    }
}

/// Immutable snapshot of a slot's automation track, carried across a
/// generation rebuild (Phase 5 S4). `Copy` data; the audio-side cursor
/// (`SlotAutomation::pos`/`cursor`) starts fresh at 0 on the new generation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlotAutomationData {
    pub target: AutomationTarget,
    pub points: [AutomationPoint; MAX_AUTOMATION_POINTS],
    pub count: usize,
}

/// Immutable snapshot of the listener-facing user state that a fresh
/// generation inherits: volume / balance / speed targets, the volume-ramp
/// duration, and per-slot mix-bus state. The audio thread mirrors these onto
/// the control bus at every drain; the control side seeds each new generation
/// from a snapshot so a reconfig never snaps the listener's settings.
#[derive(Clone, Debug, PartialEq)]
pub struct UserState {
    /// Linear volume target in [0, 1].
    pub volume: f32,
    /// Balance in [-1, 1].
    pub balance: f32,
    /// Playback speed multiplier.
    pub speed: f32,
    /// Volume-ramp duration in milliseconds.
    pub volume_fade_ms: f32,
    /// Per-slot user state, indexed by mix-bus slot. Shorter than the new
    /// generation's slot count is fine (remaining slots keep defaults);
    /// entries beyond the generation's slots are ignored.
    pub slots: Vec<SlotState>,
    /// Whether `slots`/aux carry LIVE bus state (a [`ControlBus`] snapshot)
    /// as opposed to pristine defaults. False at construction, so the
    /// config-applied trims/sends/aux (Phase 5 S1/S2/S3) are authoritative;
    /// true on a reconfig, so live commands applied since the last rebuild
    /// win over the config.
    pub has_live_bus_state: bool,
    /// Aux bus enabled (Phase 5 S2/S3).
    pub aux_enabled: bool,
    /// Aux return gain in [0, 1] (Phase 5 S2/S3).
    pub aux_return_gain: f32,
    /// Phase-6 aux insert: enabled / wet-mix, carried across a rebuild so a
    /// live runtime toggle survives a generation swap.
    pub aux_insert_enabled: bool,
    pub aux_insert_wet_mix: f32,
    /// Program-gated ducking config (Phase 4 S4), carried across a rebuild
    /// so a reconfig never drops a configured duck. `None` = disabled.
    pub duck: Option<DuckState>,
    /// Per-slot automation tracks, indexed by mix-bus slot (Phase 5 S4).
    /// Shorter than the generation's slot count is fine (missing entries
    /// keep no track); entries beyond the generation's slots are ignored.
    pub slot_automation: Vec<Option<SlotAutomationData>>,
}

impl Default for UserState {
    fn default() -> Self {
        Self {
            volume: 1.0,
            balance: 0.0,
            speed: 1.0,
            volume_fade_ms: 10.0,
            slots: Vec::new(),
            has_live_bus_state: false,
            aux_enabled: false,
            aux_return_gain: 1.0,
            aux_insert_enabled: false,
            aux_insert_wet_mix: 0.5,
            duck: None,
            slot_automation: Vec::new(),
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
