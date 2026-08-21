//! Multiband crossover reconstruction fidelity tests.
//!
//! The compressor splits the signal into low / mid / high bands with
//! Linkwitz-Riley 4th-order crossovers and sums them back. A correct
//! crossover must be **reconstruction-perfect**: the summed response is
//! magnitude-flat across the entire spectrum (0 dB at every frequency) and
//! impulse energy is conserved (Parseval).
//!
//! The band compressors are set to their neutral defaults (ratio 1:1 →
//! transparent), so the measured output is exactly the crossover
//! reconstruction with no gain reduction involved.
//!
//! Run with: `cargo test --test multiband_reconstruction`

use engine::dsp::multiband_compressor::MultibandCompressor;

const SR: f32 = 48000.0;

fn transparent_compressor() -> MultibandCompressor {
    let mut mb = MultibandCompressor::new(SR);
    mb.set_enabled(true);
    mb
}

/// Steady-state RMS gain in dB of the reconstructed output for a sine at
/// `freq_hz`. Skips the first quarter of the signal to exclude the startup
/// transient of the crossover filters.
fn sine_reconstruction_gain_db(mb: &mut MultibandCompressor, freq_hz: f32) -> f32 {
    let n = (SR / freq_hz * 60.0) as usize; // 60 cycles
    let mut sum_in = 0.0f64;
    let mut sum_out = 0.0f64;
    for i in 0..n {
        let s = (2.0 * std::f32::consts::PI * freq_hz * i as f32 / SR).sin();
        let (ol, _or_) = mb.process(s, s);
        if i >= n / 4 {
            sum_in += (s as f64) * (s as f64);
            sum_out += (ol as f64) * (ol as f64);
        }
    }
    if sum_in > 1e-12 && sum_out > 1e-12 {
        10.0 * (sum_out / sum_in).log10() as f32
    } else {
        f32::NEG_INFINITY
    }
}

#[test]
fn crossover_reconstruction_sine_sweep_is_flat() {
    let mut mb = transparent_compressor();

    // Frequencies spanning both crossovers (250 Hz and 4 kHz): inside each
    // band, at the crossover skirts, and at the spectrum extremes.
    let freqs = [
        20.0f32, 100.0, 249.0, 251.0, 500.0, 1000.0, 3900.0, 4100.0, 8000.0, 10000.0, 18000.0,
        20000.0,
    ];

    let mut worst_db = 0.0f32;
    for &f in &freqs {
        let gain_db = sine_reconstruction_gain_db(&mut mb, f);
        worst_db = worst_db.max(gain_db.abs());
        assert!(
            gain_db.abs() < 0.05,
            "crossover reconstruction gain at {f} Hz: {gain_db:.4} dB (expected ~0 dB)"
        );
    }
    eprintln!("worst |gain error| across sweep: {worst_db:.4} dB");
}

#[test]
fn crossover_reconstruction_impulse_conserves_energy() {
    let mut mb = transparent_compressor();
    let impulse_index = 4096usize;
    let n = 8192;
    let window = 256usize;

    let mut peak = 0.0f64;
    let mut energy_total = 0.0f64;
    let mut energy_window = 0.0f64;
    for i in 0..n {
        let x = if i == impulse_index { 1.0 } else { 0.0 };
        let (ol, _or_) = mb.process(x, x);
        let e = (ol as f64) * (ol as f64);
        peak = peak.max(ol.abs() as f64);
        energy_total += e;
        if i.abs_diff(impulse_index) <= window {
            energy_window += e;
        }
    }

    // An LR4 crossover is magnitude-flat but not phase-linear, so the impulse
    // is deliberately smeared by the crossover's allpass-style phase twist
    // (this is what makes the summed magnitude flat). The reference-grade
    // properties to pin are therefore:
    //   1. Total energy is conserved (Parseval: |H| ≡ 1 ⇒ Σ h² = 1).
    //   2. The response is bounded and concentrated around the impulse
    //      (a defective crossover would scatter or lose energy).
    //   3. The peak is a healthy fraction of unity (not collapsed by
    //      cancellation between the bands).
    assert!(
        (energy_total - 1.0).abs() < 0.05,
        "crossover reconstruction must conserve impulse energy, got {energy_total:.4}"
    );
    assert!(
        energy_window / energy_total > 0.95,
        "impulse energy must concentrate within ±{window} samples, got {:.4}",
        energy_window / energy_total
    );
    assert!(
        peak > 0.5,
        "crossover impulse response peak too small, got {peak:.4}"
    );
}

/// Verify the reconstruction is flat at the exact crossover frequencies with
/// a tighter RMS-style bound than the sine-peak measurement.
#[test]
fn crossover_reconstruction_lr4_power_complementarity() {
    // LR4 (cascaded 2nd-order Butterworth) is power-complementary: at the
    // crossover, each pair is at -6 dB and the summed magnitude is exactly
    // 0 dB. Measure the reconstruction at both crossover points.
    let mut mb = transparent_compressor();
    for &f in &[250.0f32, 4000.0] {
        let gain_db = sine_reconstruction_gain_db(&mut mb, f);
        assert!(
            gain_db.abs() < 0.05,
            "reconstruction at LR4 crossover {f} Hz: {gain_db:.4} dB"
        );
    }
}
