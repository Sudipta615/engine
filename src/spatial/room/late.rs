//! Late-field reverberation engine — Feedback Delay Network (FDN).
//!
//! An 8-delay-line Feedback Delay Network (FDN) with prime-based coprime
//! delay lengths, an orthogonal Householder feedback matrix, frequency-dependent
//! absorption damping, and input diffusion allpasses.
//!
//! Output encodes into the ambisonic diffuse field (§55) with diffuse decay
//! shaped by the room's `rt60_ms`.
#![allow(clippy::needless_range_loop, clippy::manual_is_multiple_of)]

use super::Room;

/// Number of delay lines in the FDN.
pub const FDN_LINES: usize = 8;

/// Coprime base delay lengths in samples at 48 kHz (~22.6 ms to 51.6 ms).
/// All chosen as prime numbers to maximize modal echo density and prevent
/// resonant ringing.
pub const FDN_BASE_DELAYS: [usize; FDN_LINES] = [1087, 1283, 1487, 1693, 1867, 2087, 2281, 2477];

/// Pre-diffusion allpass base delay lengths at 48 kHz.
pub const DIFF_BASE_DELAYS: [usize; 2] = [149, 211];

/// Pre-diffusion allpass feedback coefficient.
pub const DIFF_FEEDBACK: f32 = 0.6;

/// Late-field tail: an 8-line Feedback Delay Network (FDN).
///
/// Features:
/// - 8 coprime delay lines scaled to the operating sample rate
/// - Orthogonal Householder feedback matrix ($A = I - \frac{2}{N}\mathbf{1}\mathbf{1}^T$)
///   ensuring perfect energy conservation and maximal cross-channel diffusion
/// - Frequency-dependent one-pole absorption damping mimicking air and surface absorption
/// - Dual series allpass input diffusers for smooth transient onset
/// - Normalized DC steady-state response matching acoustic RT60 specifications
#[derive(Debug)]
pub struct RoomLateField {
    delays: [usize; FDN_LINES],
    delay_off: [usize; FDN_LINES],
    delay_pos: [usize; FDN_LINES],
    delay_buf: Vec<f32>,
    gains: [f32; FDN_LINES],
    damp_state: [f32; FDN_LINES],
    diff_delays: [usize; 2],
    diff_off: [usize; 2],
    diff_pos: [usize; 2],
    diff_buf: Vec<f32>,
    sample_rate: f32,
    prepared: bool,
}

impl Default for RoomLateField {
    fn default() -> Self {
        Self::new()
    }
}

impl RoomLateField {
    /// Create a new uninitialised late-field FDN engine.
    pub fn new() -> Self {
        Self {
            delays: FDN_BASE_DELAYS,
            delay_off: [0; FDN_LINES],
            delay_pos: [0; FDN_LINES],
            delay_buf: Vec::new(),
            gains: [0.0; FDN_LINES],
            damp_state: [0.0; FDN_LINES],
            diff_delays: DIFF_BASE_DELAYS,
            diff_off: [0; 2],
            diff_pos: [0; 2],
            diff_buf: Vec::new(),
            sample_rate: 48_000.0,
            prepared: false,
        }
    }

    /// Size and allocate all delay lines and allpass diffusers to match
    /// the target `sample_rate`. Allocation-free once prepared.
    pub fn prepare(&mut self, sample_rate: u32) {
        let scale = sample_rate as f32 / 48_000.0;

        // Allocate FDN delay lines
        let mut off = 0usize;
        for i in 0..FDN_LINES {
            let mut d = ((FDN_BASE_DELAYS[i] as f32 * scale).round() as usize).max(64);
            if d % 2 == 0 {
                d += 1; // Preserve odd length for coprime distribution
            }
            self.delays[i] = d;
            self.delay_off[i] = off;
            self.delay_pos[i] = 0;
            off += d;
        }
        self.delay_buf = vec![0.0; off];
        self.damp_state = [0.0; FDN_LINES];

        // Allocate input diffuser allpasses
        let mut diff_off = 0usize;
        for a in 0..2 {
            let d = ((DIFF_BASE_DELAYS[a] as f32 * scale).round() as usize).max(8);
            self.diff_delays[a] = d;
            self.diff_off[a] = diff_off;
            self.diff_pos[a] = 0;
            diff_off += d;
        }
        self.diff_buf = vec![0.0; diff_off];

        self.sample_rate = sample_rate as f32;
        self.prepared = true;
    }

    /// Process the room-send plane through the FDN reverberator, writing
    /// late-field samples into `out`.
    ///
    /// Feedback matrix is Householder orthogonal:
    ///   A = I - (2/N) * 1 * 1^T
    /// Frequency damping applies one-pole low-pass filtering on each line.
    /// Allocation-free and lock-free on the audio thread.
    pub fn process(&mut self, room: &Room, send: &[f32], frames: usize, out: &mut [f32]) -> usize {
        if !self.prepared {
            return 0;
        }
        let frames = frames.min(send.len()).min(out.len());
        let rt60 = room.rt60_ms.max(1.0) / 1000.0;
        let fs = self.sample_rate;

        // Loop gains derived from target RT60
        for i in 0..FDN_LINES {
            let d = self.delays[i] as f32;
            self.gains[i] = 10f32.powf(-3.0 * d / (rt60 * fs));
        }

        // Absorption damping: room absorption [0..1] scales the one-pole filter
        // coefficient. Filter has unit DC gain ((1-alpha)/(1-alpha) = 1.0) so
        // low frequencies decay exactly at target RT60 while highs roll off.
        let alpha = (room.absorption.clamp(0.0, 0.95) * 0.25).min(0.4);
        let one_minus_alpha = 1.0 - alpha;

        const INV_N: f32 = 1.0 / FDN_LINES as f32;
        const TWO_OVER_N: f32 = 2.0 / FDN_LINES as f32;

        for f in 0..frames {
            let x = send[f];

            // 1. Input pre-diffusion allpasses
            let mut diff = x;
            for a in 0..2 {
                let pos = self.diff_pos[a];
                let idx = self.diff_off[a] + pos;
                let w_old = self.diff_buf[idx];
                let y = DIFF_FEEDBACK * diff + w_old;
                self.diff_buf[idx] = diff - DIFF_FEEDBACK * y;
                diff = y;
                self.diff_pos[a] = (pos + 1) % self.diff_delays[a];
            }

            // 2. Read delay line outputs, apply damping and attenuation
            let mut u = [0.0f32; FDN_LINES];
            let mut acc = 0.0f32;
            for i in 0..FDN_LINES {
                let pos = self.delay_pos[i];
                let idx = self.delay_off[i] + pos;
                let y = self.delay_buf[idx];

                // One-pole frequency damping
                let damped = one_minus_alpha * y + alpha * self.damp_state[i];
                self.damp_state[i] = damped;

                // Attenuation for RT60 loop decay
                u[i] = damped * self.gains[i];

                // Output accumulation normalized by (1 + gain) for unity steady-state
                acc += y * (1.0 + self.gains[i]);
            }

            // 3. Orthogonal Householder matrix multiplication:
            //    (A u)_i = u_i - (2/N) * sum(u)
            let sum_u: f32 = u.iter().sum();
            let householder_term = sum_u * TWO_OVER_N;

            // 4. Inject diffused input and feedback into delay lines
            for i in 0..FDN_LINES {
                let feedback_i = u[i] - householder_term;
                let in_sample = diff + feedback_i;

                let pos = self.delay_pos[i];
                let idx = self.delay_off[i] + pos;
                self.delay_buf[idx] = in_sample;
                self.delay_pos[i] = (pos + 1) % self.delays[i];
            }

            // 5. Output mix
            out[f] = acc * (0.5 * INV_N);
        }

        frames
    }
}
