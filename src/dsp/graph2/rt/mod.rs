//! # Graph 2.0 realtime executor (v3.50, Phase 45)
//!
//! The **realtime lowering substrate**: a compiled [`ExecutionOrder`] made
//! executable on the audio thread with zero allocation, without touching
//! the engine yet (the Phase-46/47 port follows).
//!
//! ## Design (per the plan)
//!
//! - **Preallocated per-port plane pools** sized from the compiled plan.
//!   Each edge owns one single-channel plane of exactly `block` frames —
//!   never resized, never reallocated after plan build.
//! - **Fixed scratch**: every intermediate buffer an op needs (convolution
//!   overlap, resampler history, source planes, mix fan-in) is allocated
//!   at plan build and reused every block.
//! - **Enum-dispatch per block, never trait objects**: the audio thread
//!   walks the compiled steps and matches on [`NodeKind`] — a closed set
//!   the compiler can devirtualize, unlike dynamic dispatch (the
//!   production plan discipline).
//! - **Immutable [`RtPlan`] behind an atomic pointer publish**: building
//!   the plan and pools is control-thread work. The audio thread receives
//!   the finished plan via the Phase-2 generation-swap discipline
//!   (publish / swap / retire), so the swap itself performs no allocation
//!   and the audio thread never allocates or frees.
//! - **Shared kernels**: node processing calls the *same* functions the
//!   offline executor uses (`super::exec::ops`), so offline and
//!   realtime renders share one arithmetic path — divergence is
//!   structurally impossible, and the fidelity suite pins it bit-exactly.
//!
//! ## Realtime contract
//!
//! [`RtExecutor::render_block`] and the whole `rt` hot path perform **no
//! heap allocation, no locks, no syscalls** — only preallocated memory and
//! plain arithmetic. `tests/fidelity/realtime_allocation.rs` enforces this
//! with the counting allocator; `tests/fidelity/graph2_rt_offline_equivalence.rs`
//! pins RT == offline bit-exactly.
//!
//! ## Scope boundaries (deliberate, until Phase 46/47)
//!
//! - **Resampler nodes must use `ratio == 1`.** The offline executor's
//!   fixed-grid windowed-sinc reader holds a growing input history for
//!   `ratio > 1` (the reachable window advances slower than the append),
//!   which cannot be preallocated. [`RtPlan::build`] rejects such nodes
//!   with [`RtPlanError::ResamplerRatioUnsupported`]; the production
//!   streaming resampler (Rubato-backed, fixed-memory) lands with the
//!   Phase-46 node port.
//! - **Offline-only conveniences are absent**: tempo-mapped gain
//!   automation, external-track clip addressing (the aelog replay path),
//!   live scene swaps / listener drives (a plan rebuild covers a swap),
//!   and long-kernel partitioned-FFT convolution (the direct path serves
//!   Phase-45 parity; the engine-backed node ports with Phase 46).
//! - **Sinks sum into the caller's output buffer** (`out`): the offline
//!   executor accumulates unbounded per-sink captures; a realtime render
//!   mixes every sink's input into one master `out` plane. Single-sink
//!   graphs — the equivalence-suite topology — match offline captures
//!   exactly.

use std::collections::HashMap;

use super::exec::buffers::{
    AcousticState, ConvState, DelayState, HrtfState, ResamplerState, SourceState,
};
use super::node::{NodeId, NodeKind, NodeParams, PortId};
use super::sort::ExecutionOrder;
use super::Graph2;
use crate::spatial::acoustic::bake::{spectral_taps, BakedScene, ACOUSTIC_IR_LEN};
use crate::spatial::hrtf::{Ear, HrtfDataset};
use crate::spatial::math::Vec3;

mod ops;

pub use ops::RtExecutor;

/// The scene bundle an [`RtPlan`] is built against: the active (global)
/// baked scene, the named per-listener scenes, and the live listener
/// position — snapshotted on the control thread at plan build. A scene
/// swap / listener drive is a **plan rebuild + publish**, never a mutation
/// of a live plan.
#[derive(Debug, Clone, Default)]
pub struct RtScenes {
    /// The active (global) scene unaddressed `Acoustic` nodes render.
    pub active: Option<BakedScene>,
    /// Named scenes (`NodeParams::Acoustic::scene`) for per-listener
    /// acoustic nodes.
    pub named: HashMap<String, BakedScene>,
    /// The live listener position overriding every node's baked position
    /// (the trajectory drive); `None` = each node's own position.
    pub listener: Option<Vec3>,
}

impl RtScenes {
    /// An empty bundle: no scenes (Acoustic nodes pass through), no
    /// listener drive.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mirror the offline executor's `set_baked_scene` state for a plan
    /// build: active scene + no named scenes.
    pub fn from_active(scene: BakedScene) -> Self {
        Self {
            active: Some(scene),
            named: HashMap::new(),
            listener: None,
        }
    }
}

/// Why [`RtPlan::build`] refused a topology.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RtPlanError {
    /// A `Resampler` node with `ratio > 1` cannot run allocation-free on
    /// the fixed-grid interpolator (its reachable history grows without
    /// bound). Rebuild with `ratio == 1`, or wait for the Phase-46
    /// production resampler port.
    #[error("resampler ratio {0} is unsupported on the realtime plan (use ratio 1)")]
    ResamplerRatioUnsupported(f32),
    /// The plan needs a scene the bundle does not carry (an `Acoustic`
    /// node naming a missing scene falls back to pass-through, so this is
    /// never produced today; reserved for strict builds).
    #[error("scene {0:?} is not in the bundle")]
    MissingScene(String),
}

/// Per-`HRTF`-node plan state: the resolved per-ear IRs (dataset-
/// interpolated at build) plus the shared-delay pipeline state.
type RtHrtfState = Option<(Vec<f32>, Vec<f32>, HrtfState)>;

/// One immutable, fully-preallocated snapshot of a compiled Graph2
/// topology, ready for the audio thread.
///
/// Built on the control thread (allocation is fine there); once published,
/// never mutated. The audio thread renders blocks through
/// [`RtExecutor::render_block`], which walks `steps` and enum-dispatches.
pub struct RtPlan {
    /// The compiled step order (topological, deterministic).
    pub(crate) steps: Vec<NodeId>,
    /// Per-node kind + params snapshot, indexed by `NodeId.0`.
    pub(crate) nodes: Vec<Option<(NodeKind, NodeParams)>>,
    /// One `block`-frame plane per edge, indexed by edge order (see
    /// `wires`). Zeroed at each block start.
    pub(crate) planes: Vec<Vec<f32>>,
    /// For each plane: `(source node, source port, target node, target
    /// port)` — the wire the plane carries.
    pub(crate) wires: Vec<(NodeId, PortId, NodeId, PortId)>,
    /// Per-node input adjacency: `(port, plane index)` entries, sorted by
    /// port. Precomputed at build so the audio thread never scans edges.
    pub(crate) inputs: Vec<Vec<(u32, usize)>>,
    /// Per-node output adjacency: `(port, plane indices)` entries.
    pub(crate) outputs: Vec<Vec<(u32, Vec<usize>)>>,
    /// The block size the planes were allocated for; `render_block` must
    /// be called with exactly this many frames.
    pub(crate) block: usize,
    /// The sample rate baked into node state (sine sources).
    pub(crate) sample_rate: f32,
    // ── Per-node preallocated state (indexed by NodeId.0) ──
    pub(crate) delays: Vec<Option<DelayState>>,
    pub(crate) sources: Vec<Option<SourceState>>,
    /// Direct-path `Convolution` nodes: the kernel + pipeline state, both
    /// sized for emit = `block`.
    pub(crate) convolutions: Vec<Option<(Vec<f32>, ConvState)>>,
    /// `HRTF` nodes: per-ear resolved IRs (dataset-interpolated at build)
    /// + the shared-delay pipeline state.
    pub(crate) hrtfs: Vec<RtHrtfState>,
    /// `Resampler` nodes (ratio 1 only): fixed-capacity streaming state.
    pub(crate) resamplers: Vec<Option<ResamplerState>>,
    /// `Buffer` nodes: the embedded clip planes resolved at build
    /// (channel-major) + loop flag.
    pub(crate) buffers: Vec<Option<(Vec<Vec<f32>>, bool)>>,
    /// Per-`Buffer`-node shared playback cursor (all channels in lockstep).
    pub(crate) buffer_cursors: Vec<usize>,
    /// `Acoustic` nodes: precompiled per-path kernels + the fixed
    /// raw-history ring.
    pub(crate) acoustics: Vec<Option<AcousticState>>,
    /// `Acoustic` nodes: the baked direct-path gain (1.0 = pass-through
    /// for unbaked nodes; the baked `direct().gain` otherwise).
    pub(crate) direct_gains: Vec<f32>,
    // ── Fixed scratch (reused every block, never reallocated) ──
    /// Canonical single-block output scratch for 1:1 node ops.
    pub(crate) scratch_out: Vec<f32>,
    /// Input-copy scratch: pass-through ops copy their input plane here so
    /// the broadcast below can write `planes` without holding a borrow.
    pub(crate) scratch_in: Vec<f32>,
    /// Convolution working buffer: `block + max_kernel − 1` frames.
    pub(crate) scratch_conv: Vec<f32>,
    /// An all-zero plane read as "silence" for unconnected inputs.
    pub(crate) zero_plane: Vec<f32>,
}

impl RtPlan {
    /// Build a realtime plan for a compiled graph. **Control-thread work**:
    /// allocates every plane, adjacency table, state cell and scratch
    /// buffer the render will need, and resolves every control-side input
    /// (HRTF dataset IRs, acoustic scene kernels) into fixed buffers.
    ///
    /// `graph` must have been compiled (the `order` must cover it); the
    /// plan snapshots node kinds/params and preallocates state so the
    /// audio thread never does.
    pub fn build(
        graph: &Graph2,
        order: &ExecutionOrder,
        block: usize,
        sample_rate: f32,
        scenes: Option<&RtScenes>,
        hrtf_dataset: Option<&HrtfDataset>,
    ) -> Result<Self, RtPlanError> {
        let node_count = graph.nodes.len();
        let edge_count = graph.edges.len();
        let mut plan = Self {
            steps: order.steps.clone(),
            nodes: (0..node_count).map(|_| None).collect(),
            planes: (0..edge_count).map(|_| vec![0.0; block]).collect(),
            wires: Vec::with_capacity(edge_count),
            inputs: (0..node_count).map(|_| Vec::new()).collect(),
            outputs: (0..node_count).map(|_| Vec::new()).collect(),
            block,
            sample_rate,
            delays: (0..node_count).map(|_| None).collect(),
            sources: (0..node_count).map(|_| None).collect(),
            convolutions: (0..node_count).map(|_| None).collect(),
            hrtfs: (0..node_count).map(|_| None).collect(),
            resamplers: (0..node_count).map(|_| None).collect(),
            buffers: (0..node_count).map(|_| None).collect(),
            buffer_cursors: vec![0; node_count],
            acoustics: (0..node_count).map(|_| None).collect(),
            direct_gains: vec![1.0; node_count],
            scratch_out: vec![0.0; block],
            scratch_in: vec![0.0; block],
            scratch_conv: vec![0.0; block],
            zero_plane: vec![0.0; block],
        };

        // ── Wire table + adjacency (edge order = EdgeId order) ──
        let mut by_id: Vec<Option<(NodeId, PortId, NodeId, PortId)>> =
            (0..edge_count).map(|_| None).collect();
        for e in graph.edges.values() {
            by_id[e.id.0 as usize] =
                Some((e.source.node, e.source.port, e.target.node, e.target.port));
        }
        for (edge_idx, wire) in by_id.into_iter().enumerate() {
            let Some((src, src_port, dst, dst_port)) = wire else {
                continue; // gaps in the EdgeId space (removed edges)
            };
            plan.wires.push((src, src_port, dst, dst_port));
            plan.inputs[dst.0 as usize].push((dst_port.0, edge_idx));
            let out = &mut plan.outputs[src.0 as usize];
            match out.iter_mut().find(|(p, _)| *p == src_port.0) {
                Some((_, planes)) => planes.push(edge_idx),
                None => out.push((src_port.0, vec![edge_idx])),
            }
        }
        for inputs in &mut plan.inputs {
            inputs.sort();
        }

        // ── Node snapshots + preallocated state ──
        let mut max_kernel = 1usize;
        for n in graph.nodes.values() {
            let idx = n.id.0 as usize;
            plan.nodes[idx] = Some((n.kind, n.params.clone()));
            match n.kind {
                NodeKind::Delay => {
                    let NodeParams::Delay { samples } = n.params else {
                        continue;
                    };
                    plan.delays[idx] = Some(DelayState {
                        buf: vec![0.0; samples as usize],
                        pos: 0,
                    });
                }
                NodeKind::Source => {
                    plan.sources[idx] = Some(SourceState::default());
                }
                NodeKind::Convolution => {
                    let NodeParams::Convolution { kernel } = &n.params else {
                        continue;
                    };
                    max_kernel = max_kernel.max(kernel.len());
                    plan.convolutions[idx] = Some((
                        kernel.clone(),
                        ConvState::with_emit_capacity(
                            kernel.len(),
                            block,
                            kernel.len().saturating_sub(1),
                        ),
                    ));
                }
                NodeKind::HRTF => {
                    // Resolve the per-ear IRs at build time: inline tabs as
                    // they are, or the dataset's measured HRIRs interpolated
                    // at the node's direction (mirroring the offline op's
                    // resolution exactly, just control-side).
                    let (left, right) = match &n.params {
                        NodeParams::HRTF {
                            left,
                            right,
                            source,
                        } => {
                            match source {
                                super::node::HrtfSource::Inline => (left.clone(), right.clone()),
                                super::node::HrtfSource::Dataset {
                                    azimuth_deg,
                                    elevation_deg,
                                    taps,
                                } => match hrtf_dataset {
                                    Some(ds) => {
                                        let mut ml = vec![0.0f32; ds.taps()];
                                        let mut mr = vec![0.0f32; ds.taps()];
                                        ds.bilinear_interpolate(
                                            *azimuth_deg,
                                            *elevation_deg,
                                            Ear::Left,
                                            &mut ml,
                                        );
                                        ds.bilinear_interpolate(
                                            *azimuth_deg,
                                            *elevation_deg,
                                            Ear::Right,
                                            &mut mr,
                                        );
                                        (
                                            to_ir_taps_owned(ml, *taps as usize),
                                            to_ir_taps_owned(mr, *taps as usize),
                                        )
                                    }
                                    // No dataset attached: fall back to the
                                    // inline tabs (usually empty →
                                    // passthrough), like the offline op.
                                    None => (left.clone(), right.clone()),
                                },
                            }
                        }
                        _ => continue,
                    };
                    let delay = left.len().max(right.len());
                    max_kernel = max_kernel.max(delay);
                    plan.hrtfs[idx] =
                        Some((left, right, HrtfState::with_emit_capacity(delay, block)));
                }
                NodeKind::Resampler => {
                    let NodeParams::Resampler { ratio, quality } = n.params else {
                        continue;
                    };
                    if ratio > 1.0 {
                        return Err(RtPlanError::ResamplerRatioUnsupported(ratio));
                    }
                    plan.resamplers[idx] =
                        Some(ResamplerState::with_block(ratio, quality as usize, block));
                }
                NodeKind::Buffer => {
                    let NodeParams::Buffer {
                        samples, looping, ..
                    } = &n.params
                    else {
                        continue;
                    };
                    plan.buffers[idx] = Some((samples.clone(), *looping));
                }
                NodeKind::Acoustic => {
                    let Some(scenes) = scenes else {
                        continue; // no scene bundle: the node passes through
                    };
                    let NodeParams::Acoustic { position, scene } = &n.params else {
                        continue;
                    };
                    let scene_ref = match scene {
                        Some(name) => scenes.named.get(name),
                        None => scenes.active.as_ref(),
                    };
                    let Some(scene_ref) = scene_ref else {
                        continue; // unbaked/missing: pass-through node
                    };
                    let lookup = scenes.listener.unwrap_or(*position);
                    let Some(obj) = scene_ref.get(lookup) else {
                        continue;
                    };
                    // Compile the per-path spectral kernels control-side and
                    // prefill the fixed raw-history ring (all zeros — the
                    // offline session-start state). The direct-path gain is
                    // a separate scalar the render applies to the input
                    // before the per-path adds.
                    let paths = scene_ref.spectral_taps(obj, ACOUSTIC_IR_LEN);
                    plan.direct_gains[idx] = obj.direct().map(|d| d.gain).unwrap_or(1.0);
                    plan.acoustics[idx] = Some(AcousticState {
                        key: Some(obj.key),
                        epoch: 0,
                        paths,
                        raw: vec![0.0; super::exec::ACOUSTIC_HISTORY],
                        pos: 0,
                    });
                }
                _ => {}
            }
        }
        let _ = spectral_taps; // (import used by doc links; keep paths above)
        plan.scratch_conv = vec![0.0; block + max_kernel.saturating_sub(1)];
        Ok(plan)
    }

    /// The block size this plan's planes were allocated for.
    pub fn block_size(&self) -> usize {
        self.block
    }

    /// The compiled step order.
    pub fn steps(&self) -> &[NodeId] {
        &self.steps
    }
}

/// The offline `to_ir_taps` padding, as an owned build-side helper.
fn to_ir_taps_owned(mut ir: Vec<f32>, taps: usize) -> Vec<f32> {
    let taps = taps.max(1);
    ir.resize(taps, 0.0);
    ir
}
