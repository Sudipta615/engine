//! The plugin host node — Rust-native effect plugins at the
//! master insert seam of the production graph.
//!
//! One [`PluginHostNode`] lives in the arena (slot [`node_id::PLUGIN`])
//! and hosts **one plugin instance chain**: the configured plugin slots
//! are instantiated on the control path at generation build, and the
//! plan step processes every active instance in order. The node is a
//! plain pass-through (bit-exact) when disabled or when instantiation
//! failed — a broken plugin never interrupts playback.
//!
//! ## Realtime contract
//!
//! - `process_block_*` calls [`PluginInstance::process`] — the plugin's
//!   own `process` must not allocate/lock/block (enforced by the
//!   `tests/fidelity/plugin_host.rs` suite and the worst-case reference
//!   plugin).
//! - Parameters travel as plain-data [`NodeCmd::SetPluginParams`] over
//!   the per-node SPSC queue (see `controls.rs`); a failed
//!   `set_param` is sticky-mirrored as dropped, never fatal.
//! - State save/restore and (re)instantiation are control-path only.

use crate::dsp::graph2::prod::arena::node::DspNode;
use crate::dsp::pipeline::{DspStageCapability, StageChannelSupport, StagePrecision};

/// Maximum plugin inserts in the master chain (bounded so the plan step
/// stays a fixed-cost loop).
pub const MAX_PLUGIN_SLOTS: usize = 4;

/// One hosted plugin slot: the instance + its config-derived identity.
struct HostedSlot {
    instance: plugin_abi::PluginInstance,
    /// The configured source (path or `static:<uid>`) — for reports.
    source: String,
    enabled: bool,
    /// The descriptor-reported ring-down tail (samples).
    tail_samples: u32,
}

/// The plugin host plan node.
pub struct PluginHostNode {
    slots: Vec<HostedSlot>,
    sample_rate: f32,
    max_channels: usize,
    /// Cached "any slot enabled" (the plan-step gate; recomputed on the
    /// control path only).
    active: bool,
    /// Sticky enable mirror: a runtime toggle survives swaps.
    runtime_enabled: bool,
    latency_samples: u32,
}

impl PluginHostNode {
    /// Control path: build the node with the given sample rate; plugins
    /// are attached by [`Self::attach`] from the generation builder.
    pub fn new(sample_rate: f32) -> Self {
        Self {
            slots: Vec::new(),
            sample_rate,
            max_channels: 2,
            active: false,
            runtime_enabled: true,
            latency_samples: 0,
        }
    }

    /// Control path (generation build): instantiate one plugin host and
    /// attach it as a slot. `Err` returns the structured load error so
    /// the builder can surface it as a config issue without poisoning
    /// the graph.
    pub fn attach(
        &mut self,
        host: &std::sync::Arc<plugin_abi::PluginHost>,
        source: &str,
        enabled: bool,
        params: &[(u32, f32)],
        state: Option<&[u8]>,
    ) -> Result<(), plugin_abi::PluginAbiError> {
        if self.slots.len() >= MAX_PLUGIN_SLOTS {
            return Err(plugin_abi::PluginAbiError::InvalidDescriptor(format!(
                "plugin host supports at most {MAX_PLUGIN_SLOTS} slots"
            )));
        }
        let mut instance = host.instantiate(self.sample_rate)?;
        instance.prepare(self.max_channels, crate::buffer::MAX_AUDIO_BLOCK_FRAMES)?;
        for &(index, value) in params {
            instance.set_param(index, value)?;
        }
        if let Some(bytes) = state {
            instance.load_state(bytes)?;
        }
        self.latency_samples = self.latency_samples.max(host.descriptor().latency_samples);
        let tail = host.descriptor().tail_samples;
        self.slots.push(HostedSlot {
            instance,
            source: source.to_string(),
            enabled,
            tail_samples: tail,
        });
        self.recompute_active();
        Ok(())
    }

    /// Control path: apply a runtime enable toggle to every slot.
    pub fn set_runtime_enabled(&mut self, enabled: bool) {
        self.runtime_enabled = enabled;
        self.recompute_active();
    }

    /// The mirrored runtime enable flag.
    pub fn runtime_enabled(&self) -> bool {
        self.runtime_enabled
    }

    /// Apply one parameter batch (audio-side drain or control-path
    /// configure). Errors are dropped (bounded posture) — a plugin that
    /// refuses a param keeps its previous value.
    pub fn apply_params(&mut self, batch: &plugin_abi::PluginParams) {
        let Some(slot) = self.slots.first_mut() else {
            return;
        };
        if !slot.enabled {
            return;
        }
        for pv in batch.iter() {
            let _ = slot.instance.set_param(pv.index, pv.value);
        }
    }

    fn recompute_active(&mut self) {
        self.active = self.runtime_enabled && self.slots.iter().any(|s| s.enabled);
    }

    /// Number of attached slots (diagnostics).
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// The attached slot sources (diagnostics / reports).
    pub fn slot_sources(&self) -> Vec<&str> {
        self.slots.iter().map(|s| s.source.as_str()).collect()
    }

    /// The maximum descriptor-reported ring-down tail among ENABLED
    /// slots, in samples (diagnostics / latency reports).
    pub fn enabled_tail_samples(&self) -> u32 {
        self.slots
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.tail_samples)
            .max()
            .unwrap_or(0)
    }
}

impl DspNode for PluginHostNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "plugin_host",
            channel_support: StageChannelSupport::AllChannels,
            position: "master insert (post-volume, pre-limiter)",
            stateful: true,
            realtime_safe: true,
            bit_perfect_compatible: false,
            sample_rate_sensitive: true,
            precision: StagePrecision::Any,
        }
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn latency_samples(&self) -> usize {
        if self.active {
            self.latency_samples as usize
        } else {
            0
        }
    }

    fn tail_samples(&self) -> usize {
        if self.active {
            self.enabled_tail_samples() as usize
        } else {
            0
        }
    }

    fn reset(&mut self) {
        for slot in &mut self.slots {
            slot.instance.reset();
        }
    }

    fn prepare(&mut self, sample_rate: f32, max_channels: usize) {
        self.sample_rate = sample_rate;
        self.max_channels = max_channels;
        // Re-prepare attached instances (control path; may reallocate
        // plugin-side).
        for slot in &mut self.slots {
            let _ = slot
                .instance
                .prepare(max_channels, crate::buffer::MAX_AUDIO_BLOCK_FRAMES);
        }
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        if !self.active {
            return;
        }
        for slot in &mut self.slots {
            if !slot.enabled {
                continue;
            }
            // SAFETY: the plan runner guarantees plane lengths match the
            // prepared shape (channels × block frames) — the same
            // contract every arena node runs under.
            let _ = unsafe { slot.instance.process(planes) };
        }
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        if !self.active {
            return;
        }
        let frames = planes.first().map(|p| p.len()).unwrap_or(0);
        if frames == 0 || frames > crate::buffer::MAX_AUDIO_BLOCK_FRAMES {
            return;
        }
        // f64 domain: demote the front pair to stack buffers (the ABI is
        // f32-only), process, promote back. Fixed-size scratch — no
        // allocation on the audio path.
        let mut l32 = [0f32; crate::buffer::MAX_AUDIO_BLOCK_FRAMES];
        let mut r32 = [0f32; crate::buffer::MAX_AUDIO_BLOCK_FRAMES];
        if let Some(p) = planes.first() {
            for (d, s) in l32.iter_mut().zip(p.iter()) {
                *d = *s as f32;
            }
        }
        if let Some(p) = planes.get(1) {
            for (d, s) in r32.iter_mut().zip(p.iter()) {
                *d = *s as f32;
            }
        }
        {
            let mut pair: [&mut [f32]; 2] = [&mut l32[..frames], &mut r32[..frames]];
            self.process_block_f32(&mut pair);
        }
        if let Some(dst) = planes.first_mut() {
            for (d, s) in dst.iter_mut().zip(l32[..frames].iter()) {
                *d = *s as f64;
            }
        }
        if let Some(dst) = planes.get_mut(1) {
            for (d, s) in dst.iter_mut().zip(r32[..frames].iter()) {
                *d = *s as f64;
            }
        }
    }
}
