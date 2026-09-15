//! Ambisonic decoding and rendering up to order 9.
#![allow(clippy::excessive_precision, clippy::needless_range_loop)]

use super::basis::{
    channel_count, sh_n, AMBISONIC_CHANNELS_MAX, AMBISONIC_ORDER, MAX_AMBISONIC_ORDER,
};
use super::rotation::rotate_bus_frame_n;
use crate::buffer::MAX_AUDIO_BLOCK_FRAMES;
use crate::spatial::math::Vec3;
use crate::spatial::render::{RenderError, SpatialRenderer};
use crate::spatial::scene::{ListenerTransform, SpatialScene};
use crate::spatial::speaker::SpeakerLayout;

/// Published max-rE decoder window coefficients for ambisonic orders 1–9
/// (Zotter–Frank / Kronlachner). Returns a slice of length `order`; entry `k`
/// is the weight `a_{k+1}` applied to all channels of that order. Order-0
/// (`W`) is always 1.0.
pub fn max_re_window(order: u8) -> &'static [f32] {
    match order {
        1 => &[0.866_025_4],
        2 => &[0.905_663_1, 0.682_689_4],
        3 => &[0.766_044_5, 0.653_445_5, 0.571_5],
        4 => &[0.832_718_0, 0.664_463_0, 0.476_741_0, 0.283_654_0],
        5 => &[
            0.873_480_0,
            0.749_743_0,
            0.592_859_0,
            0.424_474_0,
            0.260_321_0,
        ],
        6 => &[
            0.900_001_0,
            0.809_017_0,
            0.669_131_0,
            0.500_000_0,
            0.330_869_0,
            0.190_983_0,
        ],
        7 => &[
            0.919_145_0,
            0.853_553_0,
            0.734_074_0,
            0.577_350_0,
            0.415_627_0,
            0.271_744_0,
            0.155_552_0,
        ],
        8 => &[
            0.932_748_0,
            0.886_227_0,
            0.785_695_0,
            0.639_602_0,
            0.471_397_0,
            0.321_439_0,
            0.200_000_0,
            0.111_350_0,
        ],
        9 => &[
            0.942_809_0,
            0.910_180_0,
            0.826_826_0,
            0.696_707_0,
            0.540_302_0,
            0.380_794_0,
            0.247_404_0,
            0.148_905_0,
            0.080_749_0,
        ],
        _ => &[],
    }
}

/// In-phase decoder window coefficients for ambisonic orders 1–9.
/// The in-phase decoder maximizes the zone of quiet and uses the narrowest
/// possible lobe (all harmonics weighted by `(l+1)/((2l+1) choose l)`).
pub fn in_phase_window(order: u8) -> &'static [f32] {
    match order {
        1 => &[0.333_333_3],
        2 => &[0.200_000_0, 0.066_667_0],
        3 => &[0.142_857_1, 0.047_619_0, 0.009_524_0],
        4 => &[0.111_111_1, 0.037_037_0, 0.007_407_0, 0.000_823_0],
        5 => &[
            0.090_909_1,
            0.030_303_0,
            0.006_061_0,
            0.000_673_0,
            0.000_048_0,
        ],
        6 => &[
            0.076_923_1,
            0.025_641_0,
            0.005_128_0,
            0.000_569_0,
            0.000_041_0,
            0.000_002_0,
        ],
        7 => &[
            0.066_667_0,
            0.022_222_0,
            0.004_444_0,
            0.000_494_0,
            0.000_035_0,
            0.000_002_0,
            0.000_000_1,
        ],
        8 => &[
            0.058_824_0,
            0.019_608_0,
            0.003_922_0,
            0.000_436_0,
            0.000_031_0,
            0.000_001_5,
            0.000_000_1,
            0.000_000_003,
        ],
        9 => &[
            0.052_632_0,
            0.017_544_0,
            0.003_509_0,
            0.000_390_0,
            0.000_028_0,
            0.000_001_3,
            0.000_000_08,
            0.000_000_003,
            0.000_000_000_1,
        ],
        _ => &[],
    }
}

/// Ambisonic decoder policy (spec §36): how the bus maps onto speakers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DecoderPolicy {
    /// Sampling ("basic") decoder: `D = Y(S)ᵀ/N` — every order weighted equally.
    #[default]
    Basic,
    /// Max-rE weights — narrows the decoded lobe for a tighter image.
    /// Orders 1–9: published Zotter–Frank / Kronlachner 2014 window.
    MaxRe,
    /// In-phase decoder — maximally narrow lobe, no side lobes.
    InPhase,
}

/// Per-order decoder weights for `policy` (length `channel_count(order)`).
pub fn order_weights(policy: DecoderPolicy, order: u8) -> Vec<f32> {
    let n = channel_count(order);
    let mut w = vec![1.0f32; n];
    match policy {
        DecoderPolicy::Basic => {}
        DecoderPolicy::MaxRe => {
            let win = max_re_window(order);
            for l in 1..=order as usize {
                if let Some(&a) = win.get(l - 1) {
                    let start = l * l;
                    let end = (l + 1) * (l + 1);
                    if end <= n {
                        w[start..end].fill(a);
                    }
                }
            }
        }
        DecoderPolicy::InPhase => {
            let win = in_phase_window(order);
            for l in 1..=order as usize {
                if let Some(&a) = win.get(l - 1) {
                    let start = l * l;
                    let end = (l + 1) * (l + 1);
                    if end <= n {
                        w[start..end].fill(a);
                    }
                }
            }
        }
    }
    w
}

/// The ambisonic decoder: a precomputed per-speaker decode matrix applied to
/// an interleaved bus of any supported order. Realtime-safe after `prepare`.
#[derive(Debug)]
pub struct AmbisonicDecoder {
    order: u8,
    gains: Vec<f32>,
    speakers: Vec<usize>,
    speaker_count: usize,
    policy: DecoderPolicy,
    prepared: bool,
}

impl Default for AmbisonicDecoder {
    fn default() -> Self {
        Self::new(DecoderPolicy::Basic)
    }
}

impl AmbisonicDecoder {
    /// Order-1 decoder (the FOA default).
    pub fn new(policy: DecoderPolicy) -> Self {
        Self::with_order(policy, AMBISONIC_ORDER)
    }

    /// Decoder for any supported order (≤ [`MAX_AMBISONIC_ORDER`]).
    pub fn with_order(policy: DecoderPolicy, order: u8) -> Self {
        assert!(
            order <= MAX_AMBISONIC_ORDER,
            "ambisonic order {order} unsupported (max {MAX_AMBISONIC_ORDER})"
        );
        Self {
            order,
            gains: Vec::new(),
            speakers: Vec::new(),
            speaker_count: 0,
            policy,
            prepared: false,
        }
    }

    pub fn policy(&self) -> DecoderPolicy {
        self.policy
    }

    pub fn order(&self) -> u8 {
        self.order
    }

    pub fn bus_width(&self) -> usize {
        channel_count(self.order)
    }

    /// Control path: build the decode matrix for `layout`.
    pub fn prepare(
        &mut self,
        layout: &SpeakerLayout,
        _sample_rate: u32,
    ) -> Result<(), RenderError> {
        layout.validate()?;
        let w = order_weights(self.policy, self.order);
        let ch = channel_count(self.order);
        let mut speakers = Vec::new();
        for (idx, s) in layout.speakers.iter().enumerate() {
            if s.is_lfe || !s.enabled {
                continue;
            }
            speakers.push(idx);
        }
        if speakers.is_empty() {
            return Err(RenderError::DegenerateGeometry);
        }
        let n = speakers.len() as f32;
        let mut gains = Vec::with_capacity(speakers.len() * ch);
        for idx in &speakers {
            let dir = layout.speakers[*idx]
                .position
                .normalized()
                .unwrap_or(Vec3::Y);
            let mut y = vec![0.0f32; ch];
            sh_n(self.order, dir, &mut y);
            for (k, v) in y.iter().enumerate() {
                gains.push(w[k] * v / n);
            }
        }
        self.gains = gains;
        self.speakers = speakers;
        self.speaker_count = layout.speakers.len();
        self.prepared = true;
        Ok(())
    }

    pub fn channels(&self) -> usize {
        self.speaker_count
    }

    pub fn speakers(&self) -> &[usize] {
        &self.speakers
    }

    pub fn prepared(&self) -> bool {
        self.prepared
    }

    /// Decode a single bus frame into `row[0 .. self.speakers.len()]`.
    pub fn decode_frame(&self, frame: &[f32], row: &mut [f32]) {
        let ch = channel_count(self.order);
        for (k, v) in row.iter_mut().enumerate().take(self.speakers.len()) {
            let g = k * ch;
            let mut acc = 0.0f32;
            for c in 0..ch {
                acc += self.gains[g + c] * frame.get(c).copied().unwrap_or(0.0);
            }
            *v = acc;
        }
    }

    /// Decode an interleaved bus into `out`. Allocation-free.
    pub fn process_bus(&self, bus: &[f32], frames: usize, out: &mut [f32]) {
        if !self.prepared || frames == 0 {
            return;
        }
        let ch = channel_count(self.order);
        let n_spk = out.len().checked_div(frames).unwrap_or(0);
        if n_spk == 0 {
            return;
        }
        for f in 0..frames {
            let mut frame = [0.0f32; AMBISONIC_CHANNELS_MAX];
            for (c, slot) in frame.iter_mut().enumerate().take(ch) {
                *slot = bus.get(f * ch + c).copied().unwrap_or(0.0);
            }
            for (k, &spk) in self.speakers.iter().enumerate() {
                let row = k * ch;
                let mut v = 0.0f32;
                for (c, &fc) in frame.iter().enumerate().take(ch) {
                    v += self.gains[row + c] * fc;
                }
                if v != 0.0 && spk < n_spk {
                    out[f * n_spk + spk] += v;
                }
            }
        }
    }

    #[cfg(test)]
    pub fn plane_wave_gains(&self, dir: Vec3) -> Vec<f32> {
        let ch = channel_count(self.order);
        let mut frame = vec![0.0f32; ch];
        super::encode::encode_plane_wave_n(self.order, dir, 1.0, &mut frame);
        let mut out = vec![0.0f32; self.speaker_count];
        let mut bus = Vec::new();
        bus.extend_from_slice(&frame);
        self.process_bus(&bus, 1, &mut out);
        out
    }
}

/// A standalone ambisonic renderer (spec §23).
#[derive(Debug)]
pub struct AmbisonicRenderer {
    decoder: AmbisonicDecoder,
    out_trim: Vec<f32>,
    bus: Vec<f32>,
    prepared: bool,
}

impl AmbisonicRenderer {
    pub fn new(policy: DecoderPolicy) -> Self {
        Self::with_order(policy, AMBISONIC_ORDER)
    }

    pub fn with_order(policy: DecoderPolicy, order: u8) -> Self {
        Self {
            decoder: AmbisonicDecoder::with_order(policy, order),
            out_trim: Vec::new(),
            bus: vec![0.0; AMBISONIC_CHANNELS_MAX * MAX_AUDIO_BLOCK_FRAMES],
            prepared: false,
        }
    }

    pub fn policy(&self) -> DecoderPolicy {
        self.decoder.policy()
    }

    pub fn order(&self) -> u8 {
        self.decoder.order()
    }

    pub fn set_order(&mut self, order: u8) {
        let policy = self.decoder.policy();
        self.decoder = AmbisonicDecoder::with_order(policy, order.clamp(1, MAX_AMBISONIC_ORDER));
        self.prepared = false;
    }

    pub fn set_quality(&mut self, quality: crate::spatial::quality::SpatialQuality) {
        self.set_order(quality.ambisonic_order());
    }
}

impl Default for AmbisonicRenderer {
    fn default() -> Self {
        Self::new(DecoderPolicy::Basic)
    }
}

impl SpatialRenderer for AmbisonicRenderer {
    fn prepare(&mut self, layout: &SpeakerLayout, sample_rate: u32) -> Result<(), RenderError> {
        self.decoder.prepare(layout, sample_rate)?;
        self.out_trim = layout
            .speakers
            .iter()
            .map(|s| s.gain * layout.calibration.trim_gain(s.id))
            .collect();
        self.prepared = true;
        Ok(())
    }

    fn process_block(
        &mut self,
        scene: &SpatialScene,
        object_inputs: &[&[f32]],
        frames: usize,
        out: &mut [f32],
    ) -> Result<(), RenderError> {
        if !self.prepared {
            return Err(RenderError::InvalidLayout);
        }
        if frames == 0 || frames > MAX_AUDIO_BLOCK_FRAMES {
            return Err(RenderError::BufferMismatch {
                expected: MAX_AUDIO_BLOCK_FRAMES,
                got: frames,
            });
        }
        let need = self.decoder.channels() * frames;
        if out.len() < need {
            return Err(RenderError::BufferMismatch {
                expected: need,
                got: out.len(),
            });
        }
        let ch = self.decoder.bus_width();
        let order = self.decoder.order();
        let xf = ListenerTransform::from_listener(&scene.listener);
        for f in 0..frames {
            let mut frame = [0.0f32; AMBISONIC_CHANNELS_MAX];
            for (c, slot) in frame.iter_mut().enumerate().take(ch) {
                *slot = object_inputs
                    .get(c)
                    .and_then(|plane| plane.get(f))
                    .copied()
                    .unwrap_or(0.0);
            }
            rotate_bus_frame_n(xf.orientation, order, &mut frame);
            for (c, &fc) in frame.iter().enumerate().take(ch) {
                self.bus[f * ch + c] = fc;
            }
        }
        for sample in out[..need].iter_mut() {
            *sample = 0.0;
        }
        self.decoder
            .process_bus(&self.bus[..frames * ch], frames, out);
        let n_spk = self.decoder.channels();
        for f in 0..frames {
            for (spk, &trim) in self.out_trim.iter().enumerate().take(n_spk) {
                out[f * n_spk + spk] *= trim;
            }
        }
        Ok(())
    }
}
