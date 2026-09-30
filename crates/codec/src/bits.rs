/// MSB-first bit reader over a byte slice. Reads past the end yield zero bits,
/// so callers check [`BitReader::at_end`] when the distinction matters.
#[derive(Debug, Clone)]
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// True once every bit of the data has been consumed.
    pub(crate) fn at_end(&self) -> bool {
        self.pos >= self.data.len() * 8
    }

    /// Bits remaining before the end of the data.
    pub(crate) fn bits_left(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.pos)
    }

    /// Next `n` bits (1..=24) without consuming them, zero-padded past the end.
    pub(crate) fn peek(&self, n: u32) -> u32 {
        debug_assert!((1..=24).contains(&n));
        let byte = self.pos / 8;
        let shift = self.pos % 8;
        let mut word = 0u32;
        for i in 0..4 {
            word <<= 8;
            if let Some(&b) = self.data.get(byte + i) {
                word |= u32::from(b);
            }
        }
        (word << shift) >> (32 - n)
    }

    pub(crate) fn skip(&mut self, n: u32) {
        self.pos = self.pos.saturating_add(n as usize);
    }

    pub(crate) fn read(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.skip(n);
        v
    }

    /// Advances to the next byte boundary if not already on one.
    pub(crate) fn align_byte(&mut self) {
        self.pos = self.pos.div_ceil(8) * 8;
    }
}

/// MSB-first bit writer.
#[derive(Debug, Default, Clone)]
pub(crate) struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    nbits: u32,
}

impl BitWriter {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Bits written so far.
    #[cfg(test)]
    pub(crate) fn bit_len(&self) -> usize {
        self.out.len() * 8 + self.nbits as usize
    }

    /// Appends the low `n` bits (n <= 24) of `code`, most significant first.
    pub(crate) fn write(&mut self, code: u32, n: u32) {
        debug_assert!(n <= 24);
        self.acc = (self.acc << n) | (code & ((1u32 << n) - 1));
        self.nbits += n;
        while self.nbits >= 8 {
            self.nbits -= 8;
            self.out.push((self.acc >> self.nbits) as u8);
        }
        self.acc &= (1u32 << self.nbits) - 1;
    }

    /// Pads with zero bits to the next byte boundary.
    pub(crate) fn align(&mut self) {
        if self.nbits > 0 {
            self.write(0, 8 - self.nbits);
        }
    }

    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.align();
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peek_reads_across_byte_boundaries_and_zero_pads_past_end() {
        let mut r = BitReader::new(&[0b1010_0000, 0b1111_0000]);
        assert_eq!(r.peek(3), 0b101);
        r.skip(3);
        assert_eq!(r.read(9), 0b0_0000_1111);
        assert_eq!(r.bits_left(), 4);
        assert_eq!(r.peek(12), 0);
        r.skip(4);
        assert!(r.at_end());
    }

    #[test]
    fn writer_round_trips_through_reader() {
        let mut w = BitWriter::new();
        w.write(0b1, 1);
        w.write(0b0110, 4);
        w.write(0x5a5a, 16);
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read(1), 1);
        assert_eq!(r.read(4), 0b0110);
        assert_eq!(r.read(16), 0x5a5a);
        assert_eq!(bytes.len(), 3);
    }
}
