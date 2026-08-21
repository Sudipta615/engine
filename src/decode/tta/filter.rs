//! Adaptive hybrid filter and prediction for TTA decoding.

/// `shift_1[i] = 1 << i` for i < 32, saturating at 0x80000000, then 0xFFFFFFFF.
/// Mirrors the reference table used by the Rice adaptation and filter round.
pub(crate) const SHIFT_1: [u32; 41] = [
    0x0000_0001,
    0x0000_0002,
    0x0000_0004,
    0x0000_0008,
    0x0000_0010,
    0x0000_0020,
    0x0000_0040,
    0x0000_0080,
    0x0000_0100,
    0x0000_0200,
    0x0000_0400,
    0x0000_0800,
    0x0000_1000,
    0x0000_2000,
    0x0000_4000,
    0x0000_8000,
    0x0001_0000,
    0x0002_0000,
    0x0004_0000,
    0x0008_0000,
    0x0010_0000,
    0x0020_0000,
    0x0040_0000,
    0x0080_0000,
    0x0100_0000,
    0x0200_0000,
    0x0400_0000,
    0x0800_0000,
    0x1000_0000,
    0x2000_0000,
    0x4000_0000,
    0x8000_0000,
    0x8000_0000,
    0x8000_0000,
    0x8000_0000,
    0x8000_0000,
    0x8000_0000,
    0x8000_0000,
    0x8000_0000,
    0x8000_0000,
    0xFFFF_FFFF,
];

/// `shift_16[k] = shift_1[k + 4] = 1 << (k + 4)` (within the linear range).
#[inline]
pub(crate) fn shift_16(k: u32) -> u32 {
    SHIFT_1[(k as usize + 4).min(SHIFT_1.len() - 1)]
}

#[inline]
pub(crate) fn shift_1(k: u32) -> u32 {
    SHIFT_1[(k as usize).min(SHIFT_1.len() - 1)]
}

/// Adaptive hybrid filter state (8 taps).
#[derive(Clone, Copy)]
pub(crate) struct TtaFilter {
    pub(crate) qm: [i32; 8],
    pub(crate) dx: [i32; 8],
    pub(crate) dl: [i32; 8],
    pub(crate) error: i32,
    pub(crate) shift: i32,
    pub(crate) round: i32,
}

impl TtaFilter {
    /// Reset for a new frame: zeroed history, shift from the per-bit-depth
    /// config table {8-bit: 10, 16-bit: 9, 24-bit: 10}, round = 1<<(shift-1).
    pub(crate) fn init(&mut self, shift: i32) {
        self.qm = [0; 8];
        self.dx = [0; 8];
        self.dl = [0; 8];
        self.error = 0;
        self.shift = shift;
        self.round = shift_1((shift - 1) as u32) as i32;
    }

    /// One filter step. All arithmetic mirrors the reference C exactly:
    /// wrapping 32-bit weight updates, unsigned product accumulation, and an
    /// arithmetic shift of the (possibly negative) rounded accumulator.
    #[inline]
    pub(crate) fn process(&mut self, input: i32) -> i32 {
        if self.error < 0 {
            for i in 0..8 {
                self.qm[i] = self.qm[i].wrapping_sub(self.dx[i]);
            }
        } else if self.error > 0 {
            for i in 0..8 {
                self.qm[i] = self.qm[i].wrapping_add(self.dx[i]);
            }
        }

        // round += Σ dl[i]·qm[i] — products accumulate modulo 2^32 exactly as
        // the reference's int32×uint32 promotion does.
        let mut sum = self.round as u32;
        for i in 0..8 {
            sum = sum.wrapping_add((self.dl[i] as u32).wrapping_mul(self.qm[i] as u32));
        }

        self.dx.copy_within(1..5, 0);
        self.dl.copy_within(1..5, 0);

        self.dx[4] = (self.dl[4] >> 30) | 1;
        self.dx[5] = ((self.dl[5] >> 30) | 2) & !1;
        self.dx[6] = ((self.dl[6] >> 30) | 2) & !1;
        self.dx[7] = ((self.dl[7] >> 30) | 4) & !3;

        self.error = input;
        let output = input.wrapping_add((sum as i32) >> self.shift);

        self.dl[4] = self.dl[5].wrapping_neg();
        self.dl[5] = self.dl[6].wrapping_neg();
        self.dl[6] = output.wrapping_sub(self.dl[7]);
        self.dl[7] = output;
        self.dl[5] = self.dl[5].wrapping_add(self.dl[6]);
        self.dl[4] = self.dl[4].wrapping_add(self.dl[5]);

        output
    }

    /// The additive contribution the next `process` call would apply
    /// (`(round >> shift)` from the current state, including the error-driven
    /// weight update). Test-encoder only: lets the encoder invert the filter
    /// without mutating it.
    #[cfg(test)]
    pub(crate) fn peek_contribution(&self) -> i32 {
        let mut qm = self.qm;
        if self.error < 0 {
            for i in 0..8 {
                qm[i] = qm[i].wrapping_sub(self.dx[i]);
            }
        } else if self.error > 0 {
            for i in 0..8 {
                qm[i] = qm[i].wrapping_add(self.dx[i]);
            }
        }
        let mut sum = self.round as u32;
        for i in 0..8 {
            sum = sum.wrapping_add((self.dl[i] as u32).wrapping_mul(qm[i] as u32));
        }
        (sum as i32) >> self.shift
    }
}

/// Fixed-order prediction, mirroring the reference macro exactly:
/// `PRED(x,k) = (int32_t)((((uint64_t)(x) << k) - x) >> k)` — the subtraction
/// and shift happen in unsigned 64-bit (logical shift), then truncate.
#[inline]
pub(crate) fn pred(x: i32, k: u32) -> i32 {
    ((((x as u64) << k).wrapping_sub(x as u64)) >> k) as i32
}

/// Zigzag decode used by the reference:
/// `value = 1 + ((value >> 1) ^ ((value & 1) - 1))` — maps
/// 0→0, 1→+1, 2→−1, 3→+2, 4→−2, …
#[inline]
pub(crate) fn zigzag_decode(value: i32) -> i32 {
    1i32.wrapping_add((value >> 1) ^ ((value & 1).wrapping_sub(1)))
}
