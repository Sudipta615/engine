//! Graph 2.0 nodes and ports (v3.27).
//!
//! A [`NodeDef`] is the atomic unit of the general-purpose topology: an
//! identity, a built-in [`NodeKind`], a list of **explicit input/output
//! ports** — each a [`PortSpec`] carrying typed-bus metadata (signal type +
//! channel count) — and a [`NodeParams`] payload. Ports are the seam the
//! existing track/bus-centered graph never had: in `dsp::graph` the chain is
//! implicit in the arena order; here a node declares what it consumes and
//! what it emits, and edges (not ordering) define the signal flow.

use crate::spatial::math::Vec3;
use serde::{Deserialize, Serialize};

/// Stable identity of a graph node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(pub u32);

/// Stable identity of one port on a node (index into the node's port list).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PortId(pub u32);

impl PortId {
    /// The canonical *output* port of single-output nodes.
    pub const OUT: PortId = PortId(0);
    /// The canonical *input* port of single-input nodes.
    pub const IN: PortId = PortId(0);
}

/// Whether a port feeds a node or is fed by one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortDirection {
    Input,
    Output,
}

/// The signal class a port carries — the **typed bus** dimension of Graph
/// 2.0. An edge is only legal when both endpoints agree on [`SignalType`];
/// audio and control can never cross.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignalType {
    Audio,
    Control,
}

/// The static contract of one port: direction, signal class, and channel
/// count (`0` = "any" — the wildcard used on fan-in/fan-out ports whose
/// channel count is inherited from their neighbour at validation time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortSpec {
    pub direction: PortDirection,
    pub signal: SignalType,
    /// Fixed channel count, or `0` for "any" (Mix inputs, Sink input, Split
    /// outputs). 1:1 nodes (Gain/Delay) fix their ports at 1 channel.
    pub channels: u8,
}

impl PortSpec {
    pub fn input(signal: SignalType, channels: u8) -> Self {
        Self {
            direction: PortDirection::Input,
            signal,
            channels,
        }
    }

    pub fn output(signal: SignalType, channels: u8) -> Self {
        Self {
            direction: PortDirection::Output,
            signal,
            channels,
        }
    }
}

/// The built-in node operations the Graph 2.0 offline executor understands.
/// The set is deliberately small but **topology-complete**: it contains a
/// generator (Source), a consumer (Sink), a 1:1 transform (Gain, Delay), a
/// fan-in (Mix) and a fan-out (Split), so any processing network expressible
/// with dry/wet buses, parallel branches and broadcast can be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    /// Generates a test signal (impulse / sine / silence) on its output.
    Source,
    /// Captures its input into an accumulated buffer for inspection.
    Sink,
    /// 1:1 scalar gain (input port 0 → output port 0).
    Gain,
    /// 1:1 `N`-sample delay (input port 0 → output port 0).
    Delay,
    /// N-in → 1-out sum (input ports `0..N`, output port 0).
    Mix,
    /// 1-in → N-out broadcast (input port 0, output ports `0..N`).
    Split,
    /// 1:1 room-response node: the input plane passes through (direct) plus
    /// one delayed, gain-scaled copy per baked propagation path of the
    /// source position — the acoustic world as a graph-routable primitive.
    /// An optional scene id selects a named baked scene (per-listener),
    /// so several `Acoustic` nodes can render distinct rooms and be mixed.
    Acoustic,
    /// A recorded-clip source (0-in / N-out, one mono port per channel):
    /// plays an embedded multi-channel clip one-shot (silence after the
    /// end) or looping — the graph's **audio-input** primitive. An
    /// external track wins when attached to the executor (the aelog
    /// replay path): the global track
    /// (`OfflineExecutor::set_external_input`) feeds unaddressed nodes,
    /// and per-clip tracks (`OfflineExecutor::set_external_clip`) feed
    /// only the nodes bearing that clip address (`NodeParams::Buffer`
    /// `clip`).
    Buffer,
    /// 1:1 FIR convolver: convolves its input with an embedded kernel and
    /// emits with one kernel-length of pipeline delay (the algorithmic
    /// latency a block-partitioned convolver pays). Reports `kernel.len()`
    /// taps to the latency pass.
    Convolution,
    /// 1-in / 2-out binaural filter: convolves mono input with per-ear
    /// HRIRs and emits both ears with the longer IR's pipeline delay, so
    /// the stereo pair stays mutually aligned. Reports `max(len)` taps.
    HRTF,
    /// 1:1 sample-rate-conversion node that reports its taps: resamples the
    /// mono input by `ratio` (≥ 1) with a bandlimited (windowed-sinc)
    /// interpolator and emits with `quality` samples of pipeline delay — so
    /// its reported `quality` taps and rendered timing agree exactly, the
    /// same convention as `Delay` / `Convolution`. The fixed-frame offline
    /// executor resamples onto its own frame grid (a rate/pitch remap), the
    /// latency-pass hook the v3.30 roadmap names.
    Resampler,
}

impl NodeKind {
    /// Static capability flags per kind (the "per-node capabilities" of the
    /// guide): whether the op is stateless, realtime-safe in principle, and
    /// whether it introduces internal latency (taps) that a later
    /// graph-wide latency pass must account for.
    pub fn capabilities(self) -> NodeCapabilities {
        match self {
            NodeKind::Source | NodeKind::Sink => NodeCapabilities {
                stateful: false,
                realtime_safe: true,
                taps: false,
            },
            NodeKind::Gain => NodeCapabilities {
                stateful: false,
                realtime_safe: true,
                taps: false,
            },
            NodeKind::Delay => NodeCapabilities {
                stateful: true,
                realtime_safe: true,
                taps: true,
            },
            NodeKind::Mix | NodeKind::Split => NodeCapabilities {
                stateful: false,
                realtime_safe: true,
                taps: false,
            },
            // The direct path passes through with zero pipeline latency; the
            // per-path delayed copies are a reverb *tail*, not alignment
            // latency (see `dsp::graph2::latency::node_latency`).
            NodeKind::Acoustic => NodeCapabilities {
                stateful: true,
                realtime_safe: true,
                taps: false,
            },
            NodeKind::Buffer => NodeCapabilities {
                stateful: true,
                realtime_safe: true,
                taps: false,
            },
            // Convolvers, binaural filters and resamplers introduce pipeline
            // latency (kernel / IR length / filter taps) that `node_latency`
            // reports and `compensate` aligns — exactly like Delay.
            NodeKind::Convolution | NodeKind::HRTF | NodeKind::Resampler => NodeCapabilities {
                stateful: true,
                realtime_safe: true,
                taps: true,
            },
        }
    }
}

/// Per-node capability metadata (see [`NodeKind::capabilities`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCapabilities {
    /// The node keeps internal state across blocks (delay lines, filters).
    pub stateful: bool,
    /// The op is allocation/lock-free and could run on a realtime thread.
    pub realtime_safe: bool,
    /// The node introduces signal latency that must be compensated.
    pub taps: bool,
}

/// Where an `HRTF` node draws its per-ear impulse responses from: the node's
/// own hand-authored `left`/`right` tabs, or the real binaural renderer's
/// **measured** head-related impulse responses from a [`HrtfDataset`]
/// (crate::spatial::hrtf::HrtfDataset) attached to the executor
/// (`OfflineExecutor::set_hrtf_dataset`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HrtfSource {
    /// Use the embedded `left` / `right` HRIRs (the classic hand-authored
    /// form; reported taps = `max(left.len, right.len)`).
    Inline,
    /// Use the measured per-ear HRIRs bilinearly interpolated from the
    /// executor's `HrtfDataset` at the source `azimuth_deg` / `elevation_deg`.
    /// The IRs are rendered at `taps` (≤`crate::spatial::hrtf::MAX_HRTF_TAPS`),
    /// which the node reports to the latency pass and delays by — measured
    /// binaural branches align exactly like a `Delay(taps)`. A node without a
    /// dataset attached falls back to passthrough (its inline tabs are empty).
    Dataset {
        azimuth_deg: f32,
        elevation_deg: f32,
        taps: u32,
    },
}

/// The parameter payload of a node, discriminated by [`NodeKind`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NodeParams {
    /// `Source` — which test signal and (for sine) its frequency.
    Source(SourceParams),
    /// `Sink` — no parameters.
    Sink,
    /// `Gain` — linear gain applied per sample.
    Gain { gain: f32 },
    /// `Delay` — delay length in samples.
    Delay { samples: u32 },
    /// `Mix` / `Split` — structure-only, no parameters.
    None,
    /// `Acoustic` — the source position in the baked acoustic world whose
    /// room response this node renders (the listener + response come from
    /// the [`BakedScene`](crate::spatial::acoustic::bake::BakedScene)).
    /// `scene: Some(name)` selects a **named scene** from the executor's
    /// registry (`OfflineExecutor::set_scene`), letting one graph render
    /// distinct room responses for several listeners and mix them in the
    /// topology; `scene: None` uses the executor's active (global) scene.
    Acoustic {
        position: Vec3,
        scene: Option<String>,
    },
    /// `Buffer` — the embedded clip as **channel-major planes** (one
    /// `Vec<f32>` per channel), whether it loops, and an optional **clip
    /// address**. The node exposes one mono output port per channel, so a
    /// stereo/spatial track routes as explicit mono wires (the HRTF
    /// convention). An addressed node (`clip: Some(name)`) plays the
    /// executor's per-clip track registered for `name`
    /// (`OfflineExecutor::set_external_clip`) when one is attached — the
    /// aelog multi-input path; an unaddressed node plays the global
    /// external track (`OfflineExecutor::set_external_input`). Either
    /// falls back to the embedded samples.
    Buffer {
        samples: Vec<Vec<f32>>,
        looping: bool,
        clip: Option<String>,
    },
    /// `Convolution` — the FIR kernel to convolve with. The node emits its
    /// output with `kernel.len()` samples of pipeline delay (the
    /// block-partitioned lookahead convention), so its reported taps and
    /// its rendered timing agree.
    Convolution { kernel: Vec<f32> },
    /// `HRTF` — per-ear head-related impulse responses (mono in, stereo
    /// out). Both ears are delayed by the *longer* IR so the pair stays
    /// aligned; the node reports that length as its taps. `source` selects
    /// whether the IRs are the embedded `left`/`right` tabs or the real
    /// measured responses from an executor [`HrtfDataset`]
    /// (crate::spatial::hrtf::HrtfDataset) at an azimuth/elevation:
    /// `HrtfSource::Inline` reproduces the classic hand-authored form;
    /// `HrtfSource::Dataset` renders measured HRIRs (graph-based binaural
    /// branches using real head-related responses).
    HRTF {
        left: Vec<f32>,
        right: Vec<f32>,
        source: HrtfSource,
    },
    /// `Resampler` — sample-rate conversion by `ratio` (output frames per
    /// input frame, ≥ 1) with a bandlimited windowed-sinc interpolator of
    /// half-span `quality`. The node reports `quality` as its taps and
    /// emits with exactly that many samples of pipeline delay, so the
    /// latency pass aligns branches around it like a `Delay`.
    Resampler { ratio: f32, quality: u32 },
}

impl NodeParams {
    /// A human-readable summary used by graph inspection.
    pub fn describe(&self) -> String {
        match self {
            NodeParams::Source(p) => match p.signal {
                TestSignal::Impulse => "impulse".to_string(),
                TestSignal::Sine => format!("sine {} Hz", p.frequency_hz),
                TestSignal::Silence => "silence".to_string(),
            },
            NodeParams::Sink => "capture".to_string(),
            NodeParams::Gain { gain } => format!("{gain}"),
            NodeParams::Delay { samples } => format!("{samples} samples"),
            NodeParams::None => String::new(),
            NodeParams::Acoustic { position, scene } => {
                let base = format!("({:.2}, {:.2}, {:.2})", position.x, position.y, position.z);
                match scene {
                    Some(s) => format!("{base} scene \"{s}\""),
                    None => base,
                }
            }
            NodeParams::Buffer {
                samples,
                looping,
                clip,
            } => {
                let frames = samples.first().map(|c| c.len()).unwrap_or(0);
                let n = samples.len();
                let body = if n > 1 {
                    format!(
                        "{}ch {frames} samples{}",
                        n,
                        if *looping { " (loop)" } else { "" }
                    )
                } else {
                    format!("{frames} samples{}", if *looping { " (loop)" } else { "" })
                };
                match clip {
                    Some(c) => format!("{body} clip \"{c}\""),
                    None => body,
                }
            }
            NodeParams::Convolution { kernel } => format!("{} taps", kernel.len()),
            NodeParams::HRTF {
                left,
                right,
                source,
            } => match source {
                HrtfSource::Inline => format!("L{} R{}", left.len(), right.len()),
                HrtfSource::Dataset {
                    azimuth_deg,
                    elevation_deg,
                    taps,
                } => format!("dataset az {azimuth_deg:.0}° el {elevation_deg:.0}° {taps} taps"),
            },
            NodeParams::Resampler { ratio, quality } => {
                format!("x{ratio:.2} {quality} taps")
            }
        }
    }
}

/// What a [`NodeKind::Source`] emits.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum TestSignal {
    /// One `1.0` at sample 0 of the very first block, silence after.
    Impulse,
    /// A unit-amplitude sine at `frequency_hz`.
    Sine,
    /// All zeros (useful for wiring checks).
    Silence,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SourceParams {
    pub signal: TestSignal,
    pub frequency_hz: f32,
}

impl Default for SourceParams {
    fn default() -> Self {
        Self {
            signal: TestSignal::Impulse,
            frequency_hz: 440.0,
        }
    }
}

/// One node in a [`Graph2`](super::Graph2) topology. Port lists are
/// fixed at construction by the builder (the canonical shapes per kind);
/// edges reference ports by [`PortId`] and are validated against these
/// specs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeDef {
    pub id: NodeId,
    pub name: String,
    pub kind: NodeKind,
    pub params: NodeParams,
    /// Input ports, in construction order (`PortId(0..)`).
    pub inputs: Vec<PortSpec>,
    /// Output ports, in construction order (`PortId(0..)`).
    pub outputs: Vec<PortSpec>,
}

impl NodeDef {
    pub fn capabilities(&self) -> NodeCapabilities {
        self.kind.capabilities()
    }

    /// Look up an input port spec by index.
    pub fn input(&self, port: PortId) -> Option<&PortSpec> {
        self.inputs.get(port.0 as usize)
    }

    /// Look up an output port spec by index.
    pub fn output(&self, port: PortId) -> Option<&PortSpec> {
        self.outputs.get(port.0 as usize)
    }
}
