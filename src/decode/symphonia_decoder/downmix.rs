//! Multichannel-to-stereo downmixing using ITU-R BS.775 semantics.

use crate::decode::ChannelLayout;

/// Downmix an interleaved PCM slice to stereo using ITU-R BS.775 semantic channel matrix.
pub fn downmix_interleaved_to_stereo(
    samples: &[f32],
    layout: &ChannelLayout,
    src_channels: usize,
    plane_l: &mut [f32],
    plane_r: &mut [f32],
    frames: usize,
) -> usize {
    let actual_frames = (samples.len() / src_channels.max(1))
        .min(frames)
        .min(plane_l.len())
        .min(plane_r.len());

    if src_channels == 2 {
        for frame in 0..actual_frames {
            let idx = frame * 2;
            plane_l[frame] = samples[idx];
            plane_r[frame] = samples[idx + 1];
        }
        return actual_frames;
    }

    if src_channels == 1 {
        for frame in 0..actual_frames {
            let s = samples[frame];
            plane_l[frame] = s;
            plane_r[frame] = s;
        }
        return actual_frames;
    }

    if src_channels > 2 {
        use crate::decode::ChannelId;
        let fl_idx = layout.position_of(ChannelId::FrontLeft).unwrap_or(0);
        let fr_idx = layout.position_of(ChannelId::FrontRight).unwrap_or(1);
        let c_idx = layout.position_of(ChannelId::Center);
        let sl_idx = layout.position_of(ChannelId::SideLeft);
        let sr_idx = layout.position_of(ChannelId::SideRight);
        let rl_idx = layout.position_of(ChannelId::RearLeft);
        let rr_idx = layout.position_of(ChannelId::RearRight);
        let bc_idx = layout.position_of(ChannelId::BackCenter);

        const SQRT_HALF: f32 = std::f32::consts::FRAC_1_SQRT_2;
        for frame in 0..actual_frames {
            let base = frame * src_channels;
            let fl = if base + fl_idx < samples.len() {
                samples[base + fl_idx]
            } else {
                0.0
            };
            let fr = if base + fr_idx < samples.len() {
                samples[base + fr_idx]
            } else {
                0.0
            };

            let center = c_idx
                .and_then(|i| (base + i < samples.len()).then(|| samples[base + i]))
                .unwrap_or(0.0);
            let sl = sl_idx
                .and_then(|i| (base + i < samples.len()).then(|| samples[base + i]))
                .unwrap_or(0.0);
            let sr = sr_idx
                .and_then(|i| (base + i < samples.len()).then(|| samples[base + i]))
                .unwrap_or(0.0);
            let rl = rl_idx
                .and_then(|i| (base + i < samples.len()).then(|| samples[base + i]))
                .unwrap_or(0.0);
            let rr = rr_idx
                .and_then(|i| (base + i < samples.len()).then(|| samples[base + i]))
                .unwrap_or(0.0);
            // 6.1-style back center folds into both channels (BS.775 rear
            // scaling). LFE is intentionally excluded from the fold.
            let bc = bc_idx
                .and_then(|i| (base + i < samples.len()).then(|| samples[base + i]))
                .unwrap_or(0.0);

            // Fold in front, center, side surrounds, back center and rear surrounds
            let l = fl + SQRT_HALF * center + SQRT_HALF * sl + 0.5 * bc + 0.5 * rl;
            let r = fr + SQRT_HALF * center + SQRT_HALF * sr + 0.5 * bc + 0.5 * rr;

            plane_l[frame] = l;
            plane_r[frame] = r;
        }
        return actual_frames;
    }

    actual_frames
}
