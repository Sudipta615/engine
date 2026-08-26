//! Golden-corpus expansion (spec §26): deterministic reference vectors for
//! the signal classes the audit flagged as under-covered.
//!
//! - **DSD wire bytes** (spec §7): known DSD words must produce identical DoP
//!   packing (marker bytes, substitution, parity) and identical native-DSD
//!   byte streams (endianness, channel interleaving, LSB-first ordering).
//! - **Multichannel impulses** (spec §17/§26): an impulse on one channel at a
//!   time must light only that channel — no leakage, no channel reorder.
//! - **Pink noise** (spec §26): a deterministic 1/f generator with a
//!   measured spectral slope, usable as a reference test signal.
//! - **Stepped amplitude** (spec §26): exact step values through a unity
//!   gain chain and through a known −6 dB volume stage.

use engine::decode::dsd::{DopPacker, DsdWireFormat, NativeDsdPacker};
use engine::dsp::gain::GainProcessor;
use engine::dsp::pipeline::DspPipeline;

// ─────────────────────────────────────────────────────────────────────────────
// DSD wire bytes — DoP
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn dop_packing_golden_words_and_marker_parity() {
    // Fresh packer: first frame carries the 0x05 marker.
    let mut p = DopPacker::new();
    // word24 = (0x05 << 16) | 0x1234, left-aligned in i32.
    assert_eq!(p.pack_sample(0x1234), 0x0512_3400);
    // Toggle flips to 0xFA (the marker byte sets bit 31 → negative i32).
    assert_eq!(p.pack_sample(0x0001), 0xFA00_0100u32 as i32);
    // And back to 0x05.
    assert_eq!(p.pack_sample(0xFFFF), 0x05FF_FF00);
}

#[test]
fn dop_marker_substitution_prevents_payload_collision() {
    let mut p = DopPacker::new();
    // Payload high byte 0x05 would collide with the even marker → 0x06.
    assert_eq!(p.pack_sample(0x05AB), 0x0605_AB00);
    // Payload high byte 0xFA would collide with the odd marker → 0xFB.
    assert_eq!(p.pack_sample(0xFACD), 0xFBFA_CD00_u32 as i32);
    // Non-colliding payload keeps the base marker.
    assert_eq!(p.pack_sample(0x1234), 0x0512_3400);
}

#[test]
fn dop_stereo_frames_share_the_marker_and_substitute_per_channel() {
    let mut p = DopPacker::new();
    // Even frame: both channels get 0x05; no substitution needed.
    let (l, r) = p.pack_stereo_frame(0x1234, 0xABCD);
    assert_eq!(l, 0x0512_3400);
    assert_eq!(r, 0x05AB_CD00);

    // Odd frame: 0xFA marker; the right payload's high byte collides → 0xFB.
    let (l, r) = p.pack_stereo_frame(0x00FF, 0xFA00);
    assert_eq!(l, 0xFA00_FF00u32 as i32);
    assert_eq!(r, 0xFBF_A0000u32 as i32);
}

#[test]
fn dop_f32_round_trip_is_exact_for_24_bit_words() {
    for (dl, dr) in [(0x1234u16, 0xABCDu16), (0x05FF, 0xFA00), (0xFFFF, 0x8000)] {
        // Fresh packers start at the same frame parity, so the i32 and f32
        // entries must produce the identical 24-bit word.
        let mut a = DopPacker::new();
        let (il, ir) = a.pack_stereo_frame(dl, dr);
        let mut b = DopPacker::new();
        let (fl, fr) = b.pack_stereo_frame_f32(dl, dr);
        // f32 holds word24/2^23 exactly; ×2^23 must recover the i32 word.
        assert_eq!(((fl * 8_388_608.0) as i32) << 8, il);
        assert_eq!(((fr * 8_388_608.0) as i32) << 8, ir);
    }
}

#[test]
fn dop_reset_restores_frame_parity() {
    let mut p = DopPacker::new();
    assert_eq!(p.pack_sample(0x0001), 0x0500_0100);
    assert_eq!(p.pack_sample(0x0002), 0xFA00_0200u32 as i32);
    p.reset();
    assert_eq!(p.pack_sample(0x0003), 0x0500_0300);
}

#[test]
fn dop_pack_sample_odd_marker_negative_i32() {
    let mut p = DopPacker::new();
    let _ = p.pack_sample(0x0001); // advance to odd parity
    assert_eq!(p.pack_sample(0x0002), 0xFA00_0200u32 as i32);
}

// ─────────────────────────────────────────────────────────────────────────────
// DSD wire bytes — native packing
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn native_dsd_wire_bytes_match_the_format_layout() {
    // Input planes are normalized LSB-first DSD bytes (bit 0 = earliest
    // sample); the packer produces channel-interleaved wire words.
    let mut out = Vec::new();

    // U8: one byte per channel per word, no endianness.
    NativeDsdPacker::pack(DsdWireFormat::U8, &[&[0xA5], &[0x5A]], &mut out);
    assert_eq!(out, vec![0xA5, 0x5A]);

    // U16_LE: bytes stay in order.
    NativeDsdPacker::pack(
        DsdWireFormat::U16Le,
        &[&[0xA5, 0x3C], &[0x5A, 0x1E]],
        &mut out,
    );
    assert_eq!(out, vec![0xA5, 0x3C, 0x5A, 0x1E]);

    // U16_BE: bytes swap within each word (this is also the DoP container
    // byte order).
    NativeDsdPacker::pack(
        DsdWireFormat::U16Be,
        &[&[0xA5, 0x3C], &[0x5A, 0x1E]],
        &mut out,
    );
    assert_eq!(out, vec![0x3C, 0xA5, 0x1E, 0x5A]);

    // U32_LE / U32_BE with two channels.
    let ch0 = [1u8, 2, 3, 4];
    let ch1 = [5u8, 6, 7, 8];
    NativeDsdPacker::pack(DsdWireFormat::U32Le, &[&ch0, &ch1], &mut out);
    assert_eq!(out, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    NativeDsdPacker::pack(DsdWireFormat::U32Be, &[&ch0, &ch1], &mut out);
    assert_eq!(out, vec![4, 3, 2, 1, 8, 7, 6, 5]);

    // The frame rate a driver must be configured at: DSD64 → U8 = 352.8 kHz,
    // U16 = 176.4 kHz, U32 = 88.2 kHz.
    assert_eq!(DsdWireFormat::U8.frame_rate_hz(2_822_400), 352_800);
    assert_eq!(DsdWireFormat::U16Le.frame_rate_hz(2_822_400), 176_400);
    assert_eq!(DsdWireFormat::U32Be.frame_rate_hz(2_822_400), 88_200);
}

// ─────────────────────────────────────────────────────────────────────────────
// Multichannel impulses: one channel active at a time
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn multichannel_impulses_stay_on_their_own_channel() {
    // Default config: no DSP. The multichannel passthrough must neither
    // leak energy between channels nor reorder them.
    let mut pipeline = DspPipeline::from_config(&Default::default(), 48_000.0);
    pipeline.set_bit_perfect(true); // strongest passthrough contract

    let channels = 6usize;
    const FRAMES: usize = 512;
    let energy = |buf: &[f32]| -> f64 { buf.iter().map(|&s| (s as f64) * (s as f64)).sum() };

    for active in 0..channels {
        let mut interleaved = vec![0.0f32; FRAMES * channels];
        interleaved[active] = 1.0; // unit impulse on `active` at frame 0
        pipeline.process_block_multichannel(&mut interleaved, channels);

        for ch in 0..channels {
            let channel: Vec<f32> = (0..FRAMES)
                .map(|f| interleaved[f * channels + ch])
                .collect();
            let e = energy(&channel);
            if ch == active {
                assert!(
                    e > 0.99,
                    "channel {active} must carry the impulse (energy {e:.4})"
                );
            } else {
                assert!(
                    e < 1e-12,
                    "impulse on channel {active} leaked {e:.4} energy onto channel {ch}"
                );
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Pink noise: deterministic 1/f reference generator
// ─────────────────────────────────────────────────────────────────────────────

/// Deterministic LCG (SplitMix64).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) as u32
    }
}

/// Paul Kellet-style filtered white noise → pink noise (Voss-McCartney with
/// 16 octave filters), deterministic. Returns samples in [-1, 1].
fn pink_noise(frames: usize, seed: u64) -> Vec<f32> {
    let mut rng = Lcg(seed);
    const ROWS: usize = 16;
    let mut row = [0f32; ROWS];
    let mut running = 0f32;
    let mut out = Vec::with_capacity(frames);
    for i in 0..frames {
        // Only flip the row whose bit changed (Gray-code updates).
        let idx = (i as u32).trailing_zeros() as usize % ROWS;
        let prev = row[idx];
        let white = (rng.next() >> 8) as f32 / 8_388_608.0 - 1.0;
        row[idx] = white;
        running += white - prev;
        let mut sum = running;
        for r in row {
            sum += r;
        }
        // Normalize: pink output ≈ white / ROWS → scale up.
        out.push((sum / ROWS as f32 * 4.0).clamp(-1.0, 1.0));
    }
    out
}

#[test]
fn pink_noise_reference_has_approx_1f_spectrum() {
    let sr = 44_100.0f32;
    let n = 65_536usize;
    let signal = pink_noise(n, 0xC0FFEE);

    // Hann-windowed FFT power spectrum.
    let mut planner = realfft::RealFftPlanner::<f32>::new();
    let r2c = planner.plan_fft_forward(n);
    let mut input = signal.clone();
    for (i, s) in input.iter_mut().enumerate() {
        let w = 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos());
        *s *= w;
    }
    let mut spectrum = r2c.make_output_vec();
    r2c.process(&mut input, &mut spectrum).unwrap();

    // Log-log regression of power vs frequency over 100 Hz .. 10 kHz
    // (skip DC and the extreme high end). Expected slope ≈ -1 (1/f).
    let f0 = 100.0f32;
    let f1 = 10_000.0f32;
    let k0 = (f0 / sr * n as f32) as usize;
    let k1 = (f1 / sr * n as f32) as usize;
    let mut sum_x = 0f64;
    let mut sum_y = 0f64;
    let mut sum_xy = 0f64;
    let mut sum_xx = 0f64;
    let mut count = 0f64;
    for (k, &v) in spectrum.iter().enumerate().take(k1).skip(k0) {
        let power = v.re * v.re + v.im * v.im;
        if power <= 0.0 {
            continue;
        }
        let x = (k as f32 * sr / n as f32) as f64;
        let y = power as f64;
        sum_x += x.ln();
        sum_y += y.ln();
        sum_xy += x.ln() * y.ln();
        sum_xx += x.ln() * x.ln();
        count += 1.0;
    }
    assert!(count > 100.0);
    let slope = (count * sum_xy - sum_x * sum_y) / (count * sum_xx - sum_x * sum_x);
    assert!(
        (slope + 1.0).abs() < 0.15,
        "pink noise reference must be ~1/f (measured slope {slope:.3})"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Stepped amplitude
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn stepped_amplitude_passes_through_gain_stages_exactly() {
    let steps = [0.0f32, 0.25, 0.5, 0.75, 1.0, 0.5, 0.0, -0.25, -0.5, -1.0];
    let mut signal = Vec::new();
    for &s in &steps {
        for _ in 0..64 {
            signal.push(s);
        }
    }

    // Unity gain: bit-exact passthrough.
    let mut unity = GainProcessor::with_ramp(1.0, 0.0, 44_100.0);
    unity.snap();
    let l: Vec<f32> = signal
        .iter()
        .map(|&s| unity.process_stereo(s, 0.0).0)
        .collect();
    assert_eq!(l, signal, "unity gain must be bit-exact");

    // -6.0206 dB ≈ 0.5×: each step halves exactly (within f32 rounding).
    // The zero-duration ramp makes the 1.0 → 0.5 transition instant.
    let mut half = GainProcessor::with_ramp(1.0, 0.0, 44_100.0);
    half.snap();
    half.set_gain(0.5);
    let l: Vec<f32> = signal
        .iter()
        .map(|&s| half.process_stereo(s, 0.0).0)
        .collect();
    for (i, (&a, &b)) in l.iter().zip(signal.iter()).enumerate() {
        let expected = b * 0.5;
        assert!(
            (a - expected).abs() < 1e-6,
            "step {i}: {a} must equal {expected}"
        );
    }
}

#[test]
fn stepped_amplitude_through_pipeline_keeps_step_positions() {
    let mut pipeline = DspPipeline::from_config(&Default::default(), 44_100.0);
    pipeline.set_bit_perfect(true);
    // Impulse-like step at a known frame must not shift position.
    let mut signal = vec![0.0f32; 1024];
    signal[512] = 1.0;
    let mut out = signal.clone();
    pipeline.process_block(&mut out, &mut vec![0.0f32; 1024]);
    assert_eq!(
        out, signal,
        "bit-perfect pipeline must preserve sample positions"
    );
}
