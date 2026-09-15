//! Hybrid Spatial Renderer (spec §37, §71–75, Part II §6).
//!
//! A unified spatial rendering pipeline combining:
//! 1. Content classes: Spatial Objects, Channel-based Beds, and Diffuse Fields.
//! 2. Acoustic & Bass-Aware Room Modeling (modal standing waves below Schroeder frequency).
//! 3. Spatial Bass Engine (Pure, BassManaged, and BassImmersion modes with multi-slope crossover).
//! 4. Flexible output panning: Multichannel VBAP, Ambisonic HOA, or Binaural HRTF.
//!
//! Realtime guarantees:
//! - All internal scratch planes and filter states pre-allocated during `prepare`.
//! - Zero allocation and lock-free execution during `process_hybrid_block`.

use super::acoustic::bass_room::ModalBassRoom;
use super::ambisonic::AmbisonicRenderer;
use super::bass::SpatialBassEngine;
use super::binaural::BinauralRenderer;
use super::render::{HybridBlockInputs, RenderError, RendererKind, SpatialRenderer};
use super::speaker::SpeakerLayout;
use super::vbap::VbapRenderer;
use super::SpatialScene;
use crate::buffer::MAX_AUDIO_BLOCK_FRAMES;
use config::SpatialBassConfig;

/// Maximum number of speaker channels supported by the hybrid renderer.
pub const MAX_HYBRID_CHANNELS: usize = 16;

/// A unified hybrid spatial audio renderer.
pub struct HybridSpatialRenderer {
    output_kind: RendererKind,
    vbap: VbapRenderer,
    ambisonic: AmbisonicRenderer,
    binaural: BinauralRenderer,
    bass_config: SpatialBassConfig,
    bass_engine: SpatialBassEngine,
    bass_room: Option<ModalBassRoom>,
    sample_rate: u32,
    channel_count: usize,
    prepared: bool,

    // Pre-allocated scratch buffers to preserve the zero-allocation guarantee
    channel_planes: Vec<Vec<f32>>,
    sub_plane: Vec<f32>,
    object_bass_plane: Vec<f32>,
    lfe_plane: Vec<f32>,
}

impl HybridSpatialRenderer {
    /// Construct a new hybrid renderer with the requested output mode and bass configuration.
    pub fn new(output_kind: RendererKind, bass_config: SpatialBassConfig) -> Self {
        let vbap = VbapRenderer::new();
        let ambisonic = AmbisonicRenderer::default();
        let binaural = BinauralRenderer::new(10.0);
        let bass_engine = SpatialBassEngine::new(&bass_config, 48000.0, 2);

        let mut channel_planes = Vec::with_capacity(MAX_HYBRID_CHANNELS);
        for _ in 0..MAX_HYBRID_CHANNELS {
            channel_planes.push(vec![0.0; MAX_AUDIO_BLOCK_FRAMES]);
        }

        Self {
            output_kind,
            vbap,
            ambisonic,
            binaural,
            bass_config,
            bass_engine,
            bass_room: None,
            sample_rate: 48000,
            channel_count: 2,
            prepared: false,
            channel_planes,
            sub_plane: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            object_bass_plane: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            lfe_plane: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
        }
    }

    /// Configure the bass-aware room modal acoustics.
    pub fn set_bass_room(&mut self, room: Option<ModalBassRoom>) {
        self.bass_room = room;
    }

    /// Update spatial bass engine configuration.
    pub fn set_bass_config(&mut self, config: &SpatialBassConfig) {
        self.bass_config = config.clone();
        self.bass_engine
            .update_config(config, self.sample_rate as f32);
    }

    /// Active spatial bass configuration.
    pub fn bass_config(&self) -> &SpatialBassConfig {
        &self.bass_config
    }

    /// Switch output renderer kind (Vbap, Ambisonic, Binaural).
    pub fn set_output_kind(&mut self, kind: RendererKind) {
        self.output_kind = kind;
        self.prepared = false;
    }
}

impl SpatialRenderer for HybridSpatialRenderer {
    fn prepare(&mut self, layout: &SpeakerLayout, sample_rate: u32) -> Result<(), RenderError> {
        self.sample_rate = sample_rate.max(1);
        self.channel_count = layout.speakers.len().min(MAX_HYBRID_CHANNELS);

        // Prepare child renderers
        self.vbap.prepare(layout, sample_rate)?;
        self.ambisonic.prepare(layout, sample_rate)?;
        self.binaural
            .prepare(&SpeakerLayout::stereo(), sample_rate)?;

        // Update bass engine for active channel count, sample rate, and user bass config
        self.bass_engine = SpatialBassEngine::new(
            &self.bass_config,
            self.sample_rate as f32,
            self.channel_count,
        );

        // Ensure internal pre-allocated buffers match MAX_AUDIO_BLOCK_FRAMES
        for plane in &mut self.channel_planes {
            plane.resize(MAX_AUDIO_BLOCK_FRAMES, 0.0);
        }
        self.sub_plane.resize(MAX_AUDIO_BLOCK_FRAMES, 0.0);
        self.object_bass_plane.resize(MAX_AUDIO_BLOCK_FRAMES, 0.0);
        self.lfe_plane.resize(MAX_AUDIO_BLOCK_FRAMES, 0.0);

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
        let inputs = HybridBlockInputs {
            objects: object_inputs,
            beds: &[],
            fields: &[],
        };
        self.process_hybrid_block(scene, &inputs, frames, out)
    }

    fn process_hybrid_block(
        &mut self,
        scene: &SpatialScene,
        inputs: &HybridBlockInputs<'_>,
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

        let channels = match self.output_kind {
            RendererKind::Binaural => 2,
            _ => self.channel_count,
        };

        let total_samples = frames * channels;
        if out.len() < total_samples {
            return Err(RenderError::BufferMismatch {
                expected: total_samples,
                got: out.len(),
            });
        }

        // 1. Clear scratch planes for audio processing
        for plane in &mut self.channel_planes[..channels] {
            plane[..frames].fill(0.0);
        }
        self.sub_plane[..frames].fill(0.0);
        self.object_bass_plane[..frames].fill(0.0);
        self.lfe_plane[..frames].fill(0.0);

        // 2. Render spatial content through the selected spatial renderer
        match self.output_kind {
            RendererKind::Binaural => {
                self.binaural
                    .process_hybrid_block(scene, inputs, frames, out)?;
                // De-interleave binaural stereo out into channel planes 0 and 1
                for f in 0..frames {
                    self.channel_planes[0][f] = out[f * 2];
                    self.channel_planes[1][f] = out[f * 2 + 1];
                }
            }
            RendererKind::Ambisonic => {
                self.ambisonic
                    .process_hybrid_block(scene, inputs, frames, out)?;
                // De-interleave into channel planes
                for f in 0..frames {
                    for c in 0..channels {
                        self.channel_planes[c][f] = out[f * channels + c];
                    }
                }
            }
            RendererKind::Basic | RendererKind::Vbap | _ => {
                self.vbap.process_hybrid_block(scene, inputs, frames, out)?;
                for f in 0..frames {
                    for c in 0..channels {
                        self.channel_planes[c][f] = out[f * channels + c];
                    }
                }
            }
        }

        // 3. Accumulate per-object bass sends and apply bass intent
        for (obj_idx, (_, obj)) in scene.objects.iter_enabled().enumerate() {
            if let Some(plane) = inputs.objects.get(obj_idx) {
                let n = frames.min(plane.len());
                let send = obj.bass_send;
                for i in 0..n {
                    self.object_bass_plane[i] += plane[i] * send;
                }
            }
        }

        // 4. Modal room acoustics processing below Schroeder frequency (if enabled)
        if let Some(ref mut bass_room) = self.bass_room {
            for plane in self.channel_planes[..channels].iter_mut() {
                // Approximate speaker position or listener reference
                let pos = scene.listener.position;
                bass_room.process_modal_bass(pos, pos, &mut plane[..frames], 0.25);
            }
        }

        // 5. Process Spatial Bass Engine (crossover redirection, sub delay, immersion)
        {
            let mut mains_refs: [&mut [f32]; MAX_HYBRID_CHANNELS] =
                core::array::from_fn(|_| &mut [][..]);
            for (i, plane) in self.channel_planes[..channels].iter_mut().enumerate() {
                mains_refs[i] = &mut plane[..frames];
            }

            self.bass_engine.process(
                &mut mains_refs[..channels],
                &mut self.sub_plane[..frames],
                None,
                Some(&self.object_bass_plane[..frames]),
            );
        }

        // 6. Interleave processed channel planes back into out
        for f in 0..frames {
            for c in 0..channels {
                out[f * channels + c] = self.channel_planes[c][f];
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::speaker::SpeakerLayout;
    use config::SpatialBassMode;

    #[test]
    fn hybrid_renderer_prepares_and_processes() {
        let mut renderer =
            HybridSpatialRenderer::new(RendererKind::Binaural, SpatialBassConfig::default());

        let layout = SpeakerLayout::stereo();
        renderer.prepare(&layout, 48000).expect("prepare");

        let scene = SpatialScene::new(48000);
        let mut out = vec![0.0f32; 128 * 2];

        let inputs = HybridBlockInputs::default();
        let res = renderer.process_hybrid_block(&scene, &inputs, 128, &mut out);
        assert!(res.is_ok());
    }

    #[test]
    fn hybrid_renderer_preserves_custom_bass_config_through_prepare() {
        let custom_cfg = SpatialBassConfig {
            mode: SpatialBassMode::BassManaged,
            crossover_hz: 125.0,
            sub_delay_ms: 4.5,
            sub_polarity_invert: true,
            ..Default::default()
        };

        let mut renderer = HybridSpatialRenderer::new(RendererKind::Vbap, custom_cfg.clone());
        assert_eq!(renderer.bass_config().crossover_hz, 125.0);
        assert_eq!(renderer.bass_config().mode, SpatialBassMode::BassManaged);

        let layout = SpeakerLayout::five_point_one();
        renderer.prepare(&layout, 96000).expect("prepare");

        // The configured parameters must survive prepare()
        assert_eq!(renderer.bass_config().crossover_hz, 125.0);
        assert_eq!(renderer.bass_config().mode, SpatialBassMode::BassManaged);
        assert_eq!(renderer.bass_config().sub_delay_ms, 4.5);
        assert!(renderer.bass_config().sub_polarity_invert);

        let scene = SpatialScene::new(96000);
        let channels = layout.speakers.len();
        let mut out = vec![0.0f32; 64 * channels];
        let inputs = HybridBlockInputs::default();
        let res = renderer.process_hybrid_block(&scene, &inputs, 64, &mut out);
        assert!(res.is_ok());
    }
}
