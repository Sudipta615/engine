//! Ambisonics / Higher-Order Ambisonics (spec Part VI §32–37, §55; Phase 3 Point 22).
//!
//! Modular architecture:
//! - [`basis`]: Real spherical-harmonic basis functions up to order 9 (100 channels).
//! - [`encode`]: Plane-wave and higher-order encoding.
//! - [`rotation`]: Exact Wigner rotation blocks and coordinate transformations.
//! - [`decoder`]: Sampling (Basic), Max-rE, and In-Phase decoders and renderers.
//! - [`hoa`]: Advanced HOA pipeline with near-field compensation (NFC) and delay compensation.

pub mod basis;
pub mod decoder;
pub mod encode;
pub mod hoa;
pub mod rotation;

pub use basis::{
    channel_count, sh_foa, sh_n, AMBISONIC_CHANNELS, AMBISONIC_CHANNELS_MAX,
    AMBISONIC_CHANNELS_ORDER_2, AMBISONIC_CHANNELS_ORDER_3, AMBISONIC_ORDER, MAX_AMBISONIC_ORDER,
};
pub use decoder::{
    in_phase_window, max_re_window, AmbisonicDecoder, AmbisonicRenderer, DecoderPolicy,
};
pub use encode::{encode_plane_wave, encode_plane_wave_n, AmbisonicEncoder};
pub use hoa::{
    HoaConfig, HoaDecoder, HoaDecoding, HoaEncoder, NearFieldCompensation, PerSpeakerDelay,
};
pub use rotation::{rotate_bus_frame, rotate_bus_frame_n};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::math::{Quat, Vec3};
    use crate::spatial::speaker::SpeakerLayout;
    use std::f32::consts::FRAC_PI_2;

    const EPS: f32 = 1e-4;

    #[test]
    fn sh_basis_matches_documented_sn3d_convention() {
        let s3 = 3.0f32.sqrt();
        assert_eq!(sh_foa(Vec3::Y), [1.0, s3, 0.0, 0.0]);
        assert_eq!(sh_foa(Vec3::X), [1.0, 0.0, 0.0, s3]);
        assert_eq!(sh_foa(Vec3::Z), [1.0, 0.0, s3, 0.0]);
        let d = Vec3::new(1.0, 2.0, 3.0).normalized().unwrap();
        let y = sh_foa(d);
        assert!((y[0] - 1.0).abs() < EPS);
        assert!((y[1] - s3 * d.y).abs() < EPS);
        assert!((y[2] - s3 * d.z).abs() < EPS);
        assert!((y[3] - s3 * d.x).abs() < EPS);
    }

    #[test]
    fn order2_basis_matches_documented_sn3d_convention() {
        let s15 = 15.0f32.sqrt();
        let s5h = 5.0f32.sqrt() * 0.5;
        let s15h = s15 * 0.5;
        let mut y = [0.0f32; 9];
        sh_n(2, Vec3::Y, &mut y);
        assert!((y[0] - 1.0).abs() < EPS);
        assert!((y[1] - 3.0f32.sqrt()).abs() < EPS);
        assert!((y[6] + s5h).abs() < EPS, "Y₂⁰ at +Y = −√5/2");
        assert!((y[8] + s15h).abs() < EPS, "Y₂² at +Y = −√15/2");
        let mut y = [0.0f32; 9];
        sh_n(2, Vec3::X, &mut y);
        assert!((y[8] - s15h).abs() < EPS, "Y₂² at +X = +√15/2");
        assert!((y[6] + s5h).abs() < EPS);

        let d = Vec3::new(1.0, 2.0, 3.0).normalized().unwrap();
        let mut y = [0.0f32; 9];
        sh_n(2, d, &mut y);
        assert!((y[4] - s15 * d.x * d.y).abs() < EPS);
        assert!((y[5] - s15 * d.y * d.z).abs() < EPS);
        assert!((y[6] - s5h * (3.0 * d.z * d.z - 1.0)).abs() < EPS);
        assert!((y[7] - s15 * d.x * d.z).abs() < EPS);
        assert!((y[8] - s15h * (d.x * d.x - d.y * d.y)).abs() < EPS);

        let mut a = [0.0f32; 4];
        sh_n(1, Vec3::Y, &mut a);
        assert!((a[0] - sh_foa(Vec3::Y)[0]).abs() < EPS);
        assert!((a[1] - sh_foa(Vec3::Y)[1]).abs() < EPS);
    }

    #[test]
    fn plane_wave_encode_then_basic_decode_matches_formula() {
        let layout = SpeakerLayout::seven_point_one_four();
        let mut dec = AmbisonicDecoder::new(DecoderPolicy::Basic);
        dec.prepare(&layout, 48_000).unwrap();
        let dir = Vec3::Y;
        let gains = dec.plane_wave_gains(dir);
        let n = 11usize;
        let mut pan_idx = 0usize;
        for (idx, s) in layout.speakers.iter().enumerate() {
            if s.is_lfe || !s.enabled {
                assert_eq!(gains[idx], 0.0, "LFE/speaker {idx} silent");
                continue;
            }
            let spk_dir = s.position.normalized().unwrap();
            let cos = spk_dir.dot(dir);
            let expected = (1.0 + 3.0 * cos) / n as f32;
            assert!(
                (gains[idx] - expected).abs() < 1e-4,
                "speaker {idx} gain {} want {expected}",
                gains[idx]
            );
            pan_idx += 1;
        }
        assert_eq!(pan_idx, n);
    }

    #[test]
    fn max_re_policy_narrows_the_response() {
        let layout = SpeakerLayout::seven_point_one_four();
        let mut basic = AmbisonicDecoder::new(DecoderPolicy::Basic);
        let mut maxre = AmbisonicDecoder::new(DecoderPolicy::MaxRe);
        basic.prepare(&layout, 48_000).unwrap();
        maxre.prepare(&layout, 48_000).unwrap();
        let bg = basic.plane_wave_gains(Vec3::Y);
        let mg = maxre.plane_wave_gains(Vec3::Y);
        let n = 11usize;
        for (idx, s) in layout.speakers.iter().enumerate() {
            if s.is_lfe || !s.enabled {
                continue;
            }
            let cos = s.position.normalized().unwrap().dot(Vec3::Y);
            let a1 = 0.866_025_4f32;
            let expected = (1.0 + 3.0 * a1 * cos) / n as f32;
            assert!((mg[idx] - expected).abs() < 1e-4, "max-rE speaker {idx}");
            assert!(mg[idx].is_finite() && bg[idx].is_finite());
        }
    }

    #[test]
    fn rotation_commutes_with_the_basis() {
        for q in [
            Quat::from_euler_rad(0.7, 0.3, -0.2),
            Quat::from_euler_rad(FRAC_PI_2, 0.0, 0.0),
            Quat::from_euler_rad(0.0, -1.1, 2.3),
        ] {
            let r = |v: Vec3| q.rotate_vec3(v);
            for d in [Vec3::Y, Vec3::X, Vec3::new(1.0, 2.0, 3.0)] {
                let d = d.normalized().unwrap();
                let rd = r(d);
                let mut y = [0.0f32; 9];
                let mut yr = [0.0f32; 9];
                sh_n(2, d, &mut y);
                sh_n(2, rd, &mut yr);
                let mut rotated = y;
                rotate_bus_frame_n(q, 2, &mut rotated);
                for k in 0..9 {
                    assert!(
                        (rotated[k] - yr[k]).abs() < 1e-3,
                        "q={q:?} d={d:?} channel {k}: {} want {}",
                        rotated[k],
                        yr[k]
                    );
                }
            }
        }
    }

    #[test]
    fn order3_rotation_round_trips_to_identity() {
        let q = Quat::from_euler_rad(0.7, 0.3, -0.2);
        let d = Vec3::new(1.0, 2.0, 3.0).normalized().unwrap();
        let mut frame = [0.0f32; 16];
        encode_plane_wave_n(3, d, 1.0, &mut frame);
        rotate_bus_frame_n(q, 3, &mut frame);
        rotate_bus_frame_n(q.conjugate(), 3, &mut frame);
        let mut orig = [0.0f32; 16];
        encode_plane_wave_n(3, d, 1.0, &mut orig);
        for k in 0..16 {
            assert!((frame[k] - orig[k]).abs() < 2e-3, "channel {k}");
        }
    }

    #[test]
    fn hoa_encoder_decoder_roundtrip_order9() {
        let layout = SpeakerLayout::seven_point_one_four();
        let config = HoaConfig {
            order: 9,
            decoding: HoaDecoding::MaxRe,
        };
        let mut decoder = HoaDecoder::new(config);
        decoder.prepare(&layout, 48_000).expect("prepare");

        let encoder = HoaEncoder::new(9);
        assert_eq!(encoder.channels(), 100);

        let mut bus = vec![0.0f32; 100];
        encoder.encode(Vec3::Y, 1.0, &mut bus);

        let mut out = vec![0.0f32; layout.speakers.len()];
        decoder.decode(&bus, 1, &mut out);

        for &val in &out {
            assert!(val.is_finite());
        }
    }
}
