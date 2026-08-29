//! Graph 2.0 nodes and ports (v3.27).
//!
//! A [`NodeDef`] is the atomic unit of the general-purpose topology: an
//! identity, a built-in [`NodeKind`], a list of **explicit input/output
//! ports** — each a [`PortSpec`] carrying typed-bus metadata (signal type +
//! channel count) — and a [`NodeParams`] payload. Ports are the seam the
//! existing track/bus-centered graph never had: in `dsp::graph` the chain is
//! implicit in the arena order; here a node declares what it consumes and
//! what it emits, and edges (not ordering) define the signal flow.

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
