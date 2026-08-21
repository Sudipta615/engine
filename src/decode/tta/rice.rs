//! Adaptive Rice entropy coding and parameter adaptation for TTA.

use super::bitstream::BitReaderLsb;
use super::filter::{shift_1, shift_16};
use crate::decode::DecodeError;

/// The reference decoder refuses Rice parameters above this before shifting
/// (`k > MIN_CACHE_BITS` guard); larger `k` cannot be represented safely.
pub(crate) const MAX_RICE_K: u32 = 25;

/// Adaptive Rice coder state.
#[derive(Clone, Copy)]
pub(crate) struct TtaRice {
    pub(crate) k0: u32,
    pub(crate) k1: u32,
    pub(crate) sum0: u32,
    pub(crate) sum1: u32,
}

impl TtaRice {
    /// New-frame reset: both parameters start at 10 with pre-loaded sums
    /// (`shift_16[10] = 1 << 14`), matching the reference initialisation.
    pub(crate) fn init(&mut self) {
        self.k0 = 10;
        self.k1 = 10;
        self.sum0 = shift_16(10);
        self.sum1 = shift_16(10);
    }

    /// Reference parameter adaptation. `depth` selects the escape path; the
    /// depth-1 path adds `shift_1[k0]` to the value *between* the two sum
    /// updates, exactly as the reference does. Returns the post-adaptation
    /// value (still zigzag-coded).
    #[inline]
    pub(crate) fn adapt(&mut self, mut value: i32, depth: u32) -> i32 {
        if depth == 1 {
            self.sum1 = self
                .sum1
                .wrapping_add((value as u32).wrapping_sub(self.sum1 >> 4));
            if self.k1 > 0 && self.sum1 < shift_16(self.k1) {
                self.k1 -= 1;
            } else if self.sum1 > shift_16(self.k1 + 1) {
                self.k1 += 1;
            }
            value = value.wrapping_add(shift_1(self.k0) as i32);
        }
        self.sum0 = self
            .sum0
            .wrapping_add((value as u32).wrapping_sub(self.sum0 >> 4));
        if self.k0 > 0 && self.sum0 < shift_16(self.k0) {
            self.k0 -= 1;
        } else if self.sum0 > shift_16(self.k0 + 1) {
            self.k0 += 1;
        }
        value
    }

    /// Read one symbol (unary prefix + `k` value bits) and run adaptation.
    /// Returns the post-adaptation value (pre-zigzag).
    pub(crate) fn decode(&mut self, br: &mut BitReaderLsb) -> Result<i32, DecodeError> {
        let unary = br.read_unary(br.bits_left());
        let (depth, k) = if unary == 0 {
            (0u32, self.k0)
        } else {
            (1u32, self.k1)
        };
        let unary = if depth == 1 { unary - 1 } else { unary };

        // Reference guard: refuse parameters that cannot be represented
        // before shifting (`k > MIN_CACHE_BITS || unary > INT32_MAX >> k`).
        if k > MAX_RICE_K || unary > (i32::MAX as u32) >> k || br.bits_left() < k as usize {
            return Err(DecodeError::Decode(
                "corrupt TTA frame (Rice parameter out of range)".into(),
            ));
        }

        let raw: i32 = if k > 0 {
            let low = br
                .read_bits(k)
                .ok_or_else(|| DecodeError::Decode("corrupt TTA frame (truncated)".into()))?;
            ((unary << k).wrapping_add(low)) as i32
        } else {
            unary as i32
        };
        Ok(self.adapt(raw, depth))
    }
}
