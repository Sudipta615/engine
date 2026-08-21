//! LSB-first bit reader for the TTA bitstream.

pub(crate) struct BitReaderLsb<'a> {
    pub(crate) data: &'a [u8],
    /// Next bit position (bit 0 = LSB of byte 0).
    pub(crate) pos: usize,
}

impl<'a> BitReaderLsb<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub(crate) fn bits_left(&self) -> usize {
        self.data.len() * 8 - self.pos.min(self.data.len() * 8)
    }

    pub(crate) fn read_bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (self.pos % 8)) & 1;
        self.pos += 1;
        Some(bit as u32)
    }

    /// Read `n` bits, first-read bit becoming the LSB (LE bit order).
    /// Returns `None` when fewer than `n` bits remain.
    pub(crate) fn read_bits(&mut self, n: u32) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        if self.bits_left() < n as usize {
            return None;
        }
        let mut value = 0u32;
        for i in 0..n {
            value |= self.read_bit()? << i;
        }
        Some(value)
    }

    /// Count consecutive one-bits until a zero-bit (consuming it), capped at
    /// `max`. Mirrors the reference `get_unary(gb, 0, len)`.
    pub(crate) fn read_unary(&mut self, max: usize) -> u32 {
        let mut count = 0u32;
        while count < max as u32 {
            match self.read_bit() {
                Some(0) => break,
                Some(_) => count += 1,
                None => break,
            }
        }
        count
    }
}
