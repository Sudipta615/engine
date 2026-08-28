//! Acceptance suite for spectral HRTFs / elevation (spec Phase 18 / roadmap
//! Phase 18, Part VII §47–48, §62).
//!
//! The contract this suite pins down:
//!
//! - **Dataset** — a grid of per-ear impulse responses (azimuth ×
//!   elevation) with bilinear interpolation: exact at grid points, linear
//!   between them, continuous across the 360° azimuth seam, and validated
//!   on load (bad grids are rejected, never rendered).
//! - **FIR ear path** — when a dataset is loaded, the direct object paths
//!   convolve the interpolated IR (which carries the ITD and the spectral
//!   cues): an on-grid front-center impulse reproduces the dataset IR
//!   exactly.
//! - **Elevation cues** — both the dataset and the analytic fallback
//!   (pinna notch) attenuate high frequencies as a source rises: a raised
//!   source measures a deeper null at the notch frequency than a horizontal
//!   one. 0° elevation is an exact passthrough (the Phase-9 head model is
//!   unchanged).
//! - **Mirror symmetry** — mirroring a source swaps the ears exactly, in
//!   the dataset path and the analytic path.
//! - **Determinism** — identical scenes through fresh renderers are
//!   bit-for-bit identical and finite.

use engine::spatial::math::Vec3;
use engine::spatial::render::{HybridBlockInputs, SpatialRenderer};
use engine::spatial::{
    BinauralRenderer, Ear, ElevationNotch, HrtfDataset, SpatialScene, SpeakerLayout,
};
use std::f32::consts::PI;
use std::sync::Arc;

const SR: u32 = 48_000;

fn dft_magnitude_at(ir: &[f32], freq_hz: f32, fs: f32) -> f32 {
    let w = std::f32::consts::TAU * freq_hz / fs;
    let (mut re, mut im) = (0.0f32, 0.0f32);
    for (k, &v) in ir.iter().enumerate() {
        let phase = w * k as f32;
        re += v * phase.cos();
        im -= v * phase.sin();
    }
    (re * re + im * im).sqrt()
}

/// Render one impulse through a binaural renderer with an object at `pos`,
/// returning the interleaved output.
fn render_impulse(
    r: &mut BinauralRenderer,
    scene: &SpatialScene,
    pos: Vec3,
    frames: usize,
) -> Vec<f32> {
    let mut sc = scene.clone();
    let id = sc.create_audio_object(pos).unwrap();
    sc.object_mut(id).unwrap().distance_model = engine::spatial::DistanceModel::Linear;
    let mut input = vec![0.0f32; frames];
    input[0] = 1.0;
    let refs = [input.as_slice()];
    let inputs = HybridBlockInputs {
        objects: &refs,
        beds: &[],
        fields: &[],
    };
    let mut out = vec![0.0f32; 2 * frames];
    r.process_hybrid_block(&sc, &inputs, frames, &mut out)
        .unwrap();
    out
}

#[test]
fn bilinear_interpolation_is_exact_and_continuous() {
    let ds = HrtfDataset::synthetic(SR, 64, 15.0, 15.0);
    let mut out = [0.0f32; 64];
    let el0 = ds.elevations().iter().position(|e| e.abs() < 1e-3).unwrap();
    // Exact at a grid point.
    ds.bilinear_interpolate(0.0, 0.0, Ear::Left, &mut out);
    assert_eq!(out, ds.ir(0, el0, Ear::Left));
    // 50/50 between adjacent azimuth columns.
    ds.bilinear_interpolate(7.5, 0.0, Ear::Left, &mut out);
    let ir0 = ds.ir(0, el0, Ear::Left);
    let ir1 = ds.ir(1, el0, Ear::Left);
    for (k, &v) in out.iter().enumerate() {
        let want = 0.5 * ir0[k] + 0.5 * ir1[k];
        assert!((v - want).abs() < 1e-4, "midpoint tap {k}");
    }
    // Continuous across the 360° seam.
    let mut a = [0.0f32; 64];
    let mut b = [0.0f32; 64];
    ds.bilinear_interpolate(-0.1, 0.0, Ear::Left, &mut a);
    ds.bilinear_interpolate(359.9, 0.0, Ear::Left, &mut b);
    for (k, (&va, &vb)) in a.iter().zip(b.iter()).enumerate() {
        assert!((va - vb).abs() < 1e-6, "seam tap {k}");
    }
    // Interpolated responses are valid IRs (finite, bounded).
    assert!(out.iter().all(|v| v.is_finite()));
}

#[test]
fn dataset_validation_rejects_bad_grids() {
    assert!(HrtfDataset::from_planes(vec![], vec![0.0], 16, vec![0.0; 32]).is_err());
    assert!(HrtfDataset::from_planes(vec![0.0, 0.0], vec![0.0], 16, vec![0.0; 64]).is_err());
    assert!(HrtfDataset::from_planes(vec![0.0, 15.0], vec![0.0], 16, vec![0.0; 63]).is_err());
    assert!(HrtfDataset::from_planes(vec![0.0, 15.0], vec![0.0], 16, vec![f32::NAN; 64]).is_err());
    assert!(HrtfDataset::from_planes(vec![0.0, 15.0], vec![0.0], 16, vec![0.0; 64]).is_ok());
}

#[test]
fn fir_path_reproduces_the_dataset_ir_exactly() {
    let ds = HrtfDataset::synthetic(SR, 64, 15.0, 15.0);
    let mut r = BinauralRenderer::new(0.0);
    r.use_dataset(Some(Arc::new(ds.clone())));
    r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
    let scene = SpatialScene::new(SR);
    let out = render_impulse(&mut r, &scene, Vec3::new(0.0, 2.0, 0.0), 256);
    // Front-center object (az 0, el 0): both ears reproduce the dataset IR.
    let el0 = ds.elevations().iter().position(|e| e.abs() < 1e-3).unwrap();
    let expected = ds.ir(0, el0, Ear::Left);
    for k in 0..64 {
        assert!((out[k * 2] - expected[k]).abs() < 1e-4, "L tap {k}");
        assert!((out[k * 2 + 1] - expected[k]).abs() < 1e-4, "R tap {k}");
    }
    // Off-grid direction (az 7.5°, el 0°) reproduces the interpolated IR.
    let mut r = BinauralRenderer::new(0.0);
    r.use_dataset(Some(Arc::new(ds.clone())));
    r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
    let az7 = Vec3::new(7.5f32.to_radians().sin(), 7.5f32.to_radians().cos(), 0.0);
    let out = render_impulse(&mut r, &scene, az7, 256);
    let mut want = [0.0f32; 64];
    ds.bilinear_interpolate(7.5, 0.0, Ear::Left, &mut want);
    for k in 0..64 {
        assert!((out[k * 2] - want[k]).abs() < 1e-3, "off-grid L tap {k}");
    }
}

#[test]
fn elevation_raises_attenuate_high_frequencies() {
    // Dataset path: the el-60 row has a notch near 9.4 kHz; a raised source
    // shows a deeper null than a horizontal one.
    let ds = HrtfDataset::synthetic(SR, 64, 15.0, 15.0);
    let mut r = BinauralRenderer::new(0.0);
    r.use_dataset(Some(Arc::new(ds.clone())));
    r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
    let scene = SpatialScene::new(SR);
    let frames = 256;
    let flat = render_impulse(&mut r, &scene, Vec3::new(0.0, 2.0, 0.0), frames);
    let (s60, c60) = (60f32.to_radians().sin(), 60f32.to_radians().cos());
    let raised = render_impulse(&mut r, &scene, Vec3::new(0.0, 2.0 * c60, 2.0 * s60), frames);
    let f = 6000.0 + 4000.0 * 60f32.to_radians().sin();
    let h_flat: Vec<f32> = (0..frames).map(|f| flat[f * 2]).collect();
    let h_raised: Vec<f32> = (0..frames).map(|f| raised[f * 2]).collect();
    let m_flat = dft_magnitude_at(&h_flat, f, SR as f32);
    let m_raised = dft_magnitude_at(&h_raised, f, SR as f32);
    assert!(
        m_raised < m_flat * 0.7,
        "dataset notch {m_raised} vs {m_flat}"
    );

    // Analytic fallback: the pinna notch does the same without a dataset.
    let mut r = BinauralRenderer::new(0.0);
    r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
    let flat = render_impulse(&mut r, &scene, Vec3::new(0.0, 2.0, 0.0), frames);
    let raised = render_impulse(&mut r, &scene, Vec3::new(0.0, 2.0 * c60, 2.0 * s60), frames);
    let h_flat: Vec<f32> = (0..frames).map(|f| flat[f * 2]).collect();
    let h_raised: Vec<f32> = (0..frames).map(|f| raised[f * 2]).collect();
    let m_flat = dft_magnitude_at(&h_flat, f, SR as f32);
    let m_raised = dft_magnitude_at(&h_raised, f, SR as f32);
    assert!(
        m_raised < m_flat * 0.75,
        "analytic notch {m_raised} vs {m_flat}"
    );
    // The ElevationNotch primitive is an exact passthrough at 0°.
    let mut notch = ElevationNotch::new();
    notch.prepare(SR as f32);
    notch.set_target(0.0, 1.0);
    assert!(!notch.active());
    assert_eq!(notch.process(0.5), 0.5);
}

#[test]
fn mirror_symmetry_holds_in_both_paths() {
    let scenes = |pos: Vec3| {
        let mut sc = SpatialScene::new(SR);
        let id = sc.create_audio_object(pos).unwrap();
        sc.object_mut(id).unwrap().distance_model = engine::spatial::DistanceModel::Linear;
        sc
    };
    let frames = 256;
    let render = |r: &mut BinauralRenderer, sc: &SpatialScene| -> Vec<f32> {
        let mut input = vec![0.0f32; frames];
        input[0] = 1.0;
        let refs = [input.as_slice()];
        let inputs = HybridBlockInputs {
            objects: &refs,
            beds: &[],
            fields: &[],
        };
        let mut out = vec![0.0f32; 2 * frames];
        r.process_hybrid_block(sc, &inputs, frames, &mut out)
            .unwrap();
        out
    };
    // Dataset path: mirroring az +60° → −60° swaps the ears. A *fresh*
    // renderer per scene: the FIR ring is a continuous convolution state,
    // so two unrelated impulse renders through one renderer would leak
    // (the stale impulse lands inside the second render's early window).
    let ds = HrtfDataset::synthetic(SR, 64, 15.0, 15.0);
    let (s60, c60) = (60f32.to_radians().sin(), 60f32.to_radians().cos());
    let with_ds = || {
        let mut r = BinauralRenderer::new(0.0);
        r.use_dataset(Some(Arc::new(ds.clone())));
        r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
        r
    };
    let mut ra = with_ds();
    let mut rb = with_ds();
    let a = render(&mut ra, &scenes(Vec3::new(2.0 * s60, 2.0 * c60, 0.0)));
    let b = render(&mut rb, &scenes(Vec3::new(-2.0 * s60, 2.0 * c60, 0.0)));
    for f in 0..frames {
        assert!((a[f * 2] - b[f * 2 + 1]).abs() < 1e-4, "dataset L↔R at {f}");
        assert!((a[f * 2 + 1] - b[f * 2]).abs() < 1e-4, "dataset R↔L at {f}");
    }
    // Analytic path (with elevation — the notch is mirror-invariant).
    let mut r = BinauralRenderer::new(0.0);
    r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
    let pos = Vec3::new(2.0 * s60, 2.0 * c60 * c60, 2.0 * s60 * c60); // az 60, el 30-ish
    let a = render(&mut r, &scenes(pos));
    let mirrored = Vec3::new(-pos.x, pos.y, pos.z);
    let b = render(&mut r, &scenes(mirrored));
    for f in 0..frames {
        assert!(
            (a[f * 2] - b[f * 2 + 1]).abs() < 1e-4,
            "analytic L↔R at {f}"
        );
        assert!(
            (a[f * 2 + 1] - b[f * 2]).abs() < 1e-4,
            "analytic R↔L at {f}"
        );
    }
}

#[test]
fn dataset_render_is_deterministic_and_finite() {
    let ds = HrtfDataset::synthetic(SR, 64, 15.0, 15.0);
    let render = || -> Vec<f32> {
        let mut r = BinauralRenderer::new(0.0);
        r.use_dataset(Some(Arc::new(ds.clone())));
        r.prepare(&SpeakerLayout::stereo(), SR).unwrap();
        let mut scene = SpatialScene::new(SR);
        let id = scene.create_audio_object(Vec3::new(1.0, 1.0, 1.0)).unwrap();
        scene.object_mut(id).unwrap().distance_model = engine::spatial::DistanceModel::Linear;
        scene.object_mut(id).unwrap().spread = 0.3;
        scene.create_field().unwrap();
        let frames = 256;
        let mut input = vec![0.0f32; frames];
        input[0] = 1.0;
        let refs = [input.as_slice()];
        let field = vec![0.2f32; frames];
        let frefs = [field.as_slice()];
        let inputs = HybridBlockInputs {
            objects: &refs,
            beds: &[],
            fields: &frefs,
        };
        let mut out = vec![0.0f32; 2 * frames];
        r.process_hybrid_block(&scene, &inputs, frames, &mut out)
            .unwrap();
        out
    };
    let a = render();
    let b = render();
    assert_eq!(a, b, "bit-for-bit deterministic");
    assert!(a.iter().all(|v| v.is_finite()));
}

#[test]
fn synthetic_dataset_ir_structure_is_physically_sane() {
    // The synthetic dataset discretizes the analytic model: ITD grows with
    // azimuth, the ear-axis delay is the Woodworth maximum, and the
    // elevation notch moves with elevation.
    let ds = HrtfDataset::synthetic(SR, 64, 15.0, 15.0);
    let el0 = ds.elevations().iter().position(|e| e.abs() < 1e-3).unwrap();
    let argmax = |ir: &[f32]| -> usize {
        ir.iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .map(|(i, _)| i)
            .unwrap()
    };
    // az 0 → no ITD; az 90 → the left (contralateral) ear delayed by the
    // Woodworth maximum ≈ 31.5 samples.
    let az0 = ds.azimuths().iter().position(|a| a.abs() < 1e-3).unwrap();
    let az90 = ds
        .azimuths()
        .iter()
        .position(|a| (*a - 90.0).abs() < 1e-3)
        .unwrap();
    let ir_l0 = ds.ir(az0, el0, Ear::Left);
    let ir_l90 = ds.ir(az90, el0, Ear::Left);
    assert!(argmax(ir_l0) <= 1, "front: no delay");
    let max_itd = engine::spatial::max_itd_sec(
        engine::spatial::DEFAULT_HEAD_RADIUS,
        engine::spatial::DEFAULT_SPEED_OF_SOUND,
    ) * SR as f32;
    assert!(
        (argmax(ir_l90) as f32 - max_itd).abs() <= 3.0,
        "ear-axis delay {} vs {max_itd}",
        argmax(ir_l90)
    );
    // The +60° row's notch center sits above the +0° row's (no notch).
    let el60 = ds
        .elevations()
        .iter()
        .position(|e| (*e - 60.0).abs() < 1e-3)
        .unwrap();
    let f = 6000.0 + 4000.0 * (PI / 3.0).sin();
    let m0 = dft_magnitude_at(ds.ir(az0, el0, Ear::Right), f, SR as f32);
    let m60 = dft_magnitude_at(ds.ir(az0, el60, Ear::Right), f, SR as f32);
    assert!(m60 < m0 * 0.6, "notch at 60°: {m60} vs {m0}");
}

/// A measured-style corpus (the data model a `.sofa` export reduces to)
/// replaces the synthetic grid: `from_corpus` validates the regular mesh,
/// resamples to the target rate, peak-normalizes, and the renderer
/// reproduces the loaded IR exactly at grid points (spec §62 seam).
#[test]
fn measured_corpus_replaces_the_synthetic_grid_and_renders_exactly() {
    use engine::spatial::{HrtfCorpus, HrtfLoadOptions, HrtfMeasurement, HrtfNormalize};

    // A deliberate, memory-heavy IR: a chirp-like ramp with a distinct
    // left/right signature, recorded at 96 kHz and measured at a regular
    // 2×2 product of azimuth {0°, 90°} × elevation {0°, 45°}.
    let irl = |seed: f32| -> Vec<f32> {
        (0..64)
            .map(|i| {
                let t = i as f32 / 64.0;
                (seed + 0.5) * (t * std::f32::consts::TAU * 4.0).sin() * (1.0 - t) * 0.5
            })
            .collect()
    };
    let measurements = [(0.0f32, 0.0f32), (90.0, 0.0), (0.0, 45.0), (90.0, 45.0)]
        .iter()
        .enumerate()
        .map(|(i, &(az, el))| {
            let s = i as f32 + 1.0;
            let d = unit_vec(az, el);
            HrtfMeasurement {
                direction: [d.x, d.y, d.z],
                left: irl(s),
                right: irl(s + 0.25),
            }
        })
        .collect::<Vec<_>>();
    let corpus = HrtfCorpus {
        sample_rate: 96_000,
        source: Some("acceptance-corpus".into()),
        measurements,
    };
    let ds = HrtfDataset::from_corpus(
        &corpus,
        &HrtfLoadOptions {
            taps: 32,
            target_sample_rate: SR,
            normalize: HrtfNormalize::Peak,
        },
    )
    .expect("regular corpus loads");
    assert_eq!(ds.taps(), 32);
    assert_eq!(ds.azimuths().len(), 2);
    assert_eq!(ds.elevations().len(), 2);

    // Build a binaural renderer around the loaded corpus (the seam a host
    // would use: corpus → dataset → renderer) and verify the rendered
    // impulse at a grid point equals the loaded IR (minus the renderer's
    // path gain, which is 1.0 at zero distance with the Linear model).
    let mut scene = SpatialScene::new(SR);
    let layout = SpeakerLayout::stereo();
    let mut r = BinauralRenderer::new(0.0);
    r.use_dataset(Some(Arc::new(ds.clone())));
    r.prepare(&layout, SR).unwrap();
    let mut input = vec![0.0f32; 256];
    input[0] = 1.0;
    let refs = [input.as_slice()];
    let inputs = HybridBlockInputs {
        objects: &refs,
        beds: &[],
        fields: &[],
    };
    let mut out = vec![0.0f32; 256 * 2];
    let id = scene.create_audio_object(unit_vec(90.0, 45.0)).unwrap();
    scene.object_mut(id).unwrap().distance_model = engine::spatial::DistanceModel::Linear;
    r.process_hybrid_block(&scene, &inputs, 256, &mut out)
        .unwrap();

    // The dataset's (az=90, el=45) grid entry — right ear (the source is
    // to the right). Compare against the renderer output at the earliest
    // strong tap.
    let ia = ds
        .azimuths()
        .iter()
        .position(|a| (*a - 90.0).abs() < 1e-3)
        .unwrap();
    let ie = ds
        .elevations()
        .iter()
        .position(|e| (*e - 45.0).abs() < 1e-3)
        .unwrap();
    let expect = ds.ir(ia, ie, Ear::Right);
    // The renderer's right channel is interleaved output, channel 1. The
    // FIR path has no added latency, so the rendered impulse must equal the
    // loaded IR sample-for-sample (the existing front-center test proves the
    // exactness for az 0; this pins the same contract for a raised,
    // off-front corpus grid point).
    let right: Vec<f32> = (1..out.len()).step_by(2).map(|i| out[i]).collect();
    for k in 0..32 {
        assert!(
            (right[k] - expect[k]).abs() < 2e-2,
            "tap {k}: rendered {} vs loaded {}",
            right[k],
            expect[k]
        );
    }
}

fn unit_vec(az_deg: f32, el_deg: f32) -> Vec3 {
    let (az, el) = (az_deg.to_radians(), el_deg.to_radians());
    let horiz = el.cos();
    Vec3::new(az.sin() * horiz, az.cos() * horiz, el.sin())
}
