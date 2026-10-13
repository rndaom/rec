//! RBSP bit reader (for parsing SPS) and emulation-prevention removal.

/// Removes emulation-prevention bytes: every `00 00 03` becomes `00 00`.
/// The input is NAL payload (after the NAL header) or a whole NAL; the header
/// bytes never contain `00 00`, so either works.
pub(crate) fn unescape_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0usize;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// MSB-first bit reader with Exp-Golomb support. Reads past the end return
/// `None` instead of panicking.
#[derive(Debug)]
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    pos: usize, // in bits
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub(crate) fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(u32::from(bit))
    }

    pub(crate) fn flag(&mut self) -> Option<bool> {
        self.bit().map(|b| b != 0)
    }

    /// Reads `n <= 32` bits.
    pub(crate) fn bits(&mut self, n: u32) -> Option<u32> {
        debug_assert!(n <= 32);
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | u64::from(self.bit()?);
        }
        Some(v as u32)
    }

    pub(crate) fn skip(&mut self, n: usize) -> Option<()> {
        let end = self.pos.checked_add(n)?;
        if end > self.data.len() * 8 {
            return None;
        }
        self.pos = end;
        Some(())
    }

    /// Unsigned Exp-Golomb, `ue(v)`.
    pub(crate) fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 32 {
                return None;
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        let rest = u64::from(self.bits(zeros.min(32))?);
        let value = (1u64 << zeros) - 1 + rest;
        u32::try_from(value).ok()
    }

    /// AV1 `uvlc()`: leading zeros, a one, then that many value bits.
    pub(crate) fn uvlc(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros >= 32 {
                return Some(u32::MAX);
            }
        }
        let value = u64::from(self.bits(zeros)?);
        u32::try_from(value + (1u64 << zeros) - 1).ok()
    }

    /// Signed Exp-Golomb, `se(v)`.
    #[cfg(test)]
    pub(crate) fn se(&mut self) -> Option<i32> {
        let k = i64::from(self.ue()?);
        let v = if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) };
        i32::try_from(v).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_removes_only_emulation_prevention() {
        assert_eq!(unescape_rbsp(&[0, 0, 3, 1]), [0, 0, 1]);
        assert_eq!(unescape_rbsp(&[0, 0, 3, 0, 0, 3, 2]), [0, 0, 0, 0, 2]);
        // A 03 not preceded by two zeros stays.
        assert_eq!(unescape_rbsp(&[0, 3, 0, 3]), [0, 3, 0, 3]);
        assert_eq!(unescape_rbsp(&[1, 0, 0, 3]), [1, 0, 0]);
        // After a removed 03 the zero count restarts: 00 00 03 00 03 keeps the second 03.
        assert_eq!(unescape_rbsp(&[0, 0, 3, 0, 3]), [0, 0, 0, 3]);
        assert_eq!(unescape_rbsp(&[]), Vec::<u8>::new());
    }

    #[test]
    fn reads_bits_msb_first() {
        let mut r = BitReader::new(&[0b1010_0000, 0xFF]);
        assert_eq!(r.bit(), Some(1));
        assert_eq!(r.bit(), Some(0));
        assert_eq!(r.bits(3), Some(0b100));
        assert_eq!(r.bits(11), Some(0x0FF));
        assert_eq!(r.bit(), None);
    }

    #[test]
    fn exp_golomb_matches_the_spec_table() {
        // ue(v): 1 -> 0, 010 -> 1, 011 -> 2, 00100 -> 3, 00101 -> 4, 0001000 -> 7
        // Bits: 1 010 011 00100 00101 0001000 -> packed below.
        // 1010 0110 0100 0010 1000 1000 (24 bits)
        let mut r = BitReader::new(&[0b1010_0110, 0b0100_0010, 0b1000_1000]);
        assert_eq!(r.ue(), Some(0));
        assert_eq!(r.ue(), Some(1));
        assert_eq!(r.ue(), Some(2));
        assert_eq!(r.ue(), Some(3));
        assert_eq!(r.ue(), Some(4));
        assert_eq!(r.ue(), Some(7));
        assert_eq!(r.ue(), None);
    }

    #[test]
    fn signed_exp_golomb() {
        // se: k=0 -> 0, 1 -> 1, 2 -> -1, 3 -> 2, 4 -> -2
        let mut r = BitReader::new(&[0b1010_0110, 0b0100_0010, 0b1000_0000]);
        assert_eq!(r.se(), Some(0));
        assert_eq!(r.se(), Some(1));
        assert_eq!(r.se(), Some(-1));
        assert_eq!(r.se(), Some(2));
        assert_eq!(r.se(), Some(-2));
    }

    #[test]
    fn uvlc_matches_exp_golomb_for_short_codes() {
        let mut r = BitReader::new(&[0b1010_0110, 0b0100_0000]);
        assert_eq!(r.uvlc(), Some(0));
        assert_eq!(r.uvlc(), Some(1));
        assert_eq!(r.uvlc(), Some(2));
        assert_eq!(r.uvlc(), Some(3));
        // 32 or more leading zeros saturate without reading further.
        assert_eq!(BitReader::new(&[0, 0, 0, 0, 0xFF]).uvlc(), Some(u32::MAX));
        assert_eq!(BitReader::new(&[0, 0]).uvlc(), None);
    }

    #[test]
    fn exp_golomb_rejects_truncated_and_overlong_codes() {
        assert_eq!(BitReader::new(&[0]).ue(), None);
        assert_eq!(BitReader::new(&[0, 0, 0, 0, 0, 0]).ue(), None);
        // Four zeros and a one, but only 3 of the 4 suffix bits are present.
        assert_eq!(BitReader::new(&[0b0000_1000]).ue(), None);
        let mut r = BitReader::new(&[0b0001_1100]);
        assert_eq!(r.ue(), Some(7 + 0b110));
        assert_eq!(BitReader::new(&[0b0001_1000]).skip(100), None);
    }

    #[test]
    fn max_ue_value_roundtrips() {
        // 32 zeros, 1, then 32 bits of suffix would be 2^33-2: out of u32 range.
        let mut bytes = vec![0, 0, 0, 0, 0x80];
        bytes.extend([0xFF; 4]);
        assert_eq!(BitReader::new(&bytes).ue(), None);
        // ue = 2^32-2 : 31 zeros, 1, 31-bit suffix all ones.
        let mut r = BitReader::new(&[0, 0, 0, 1, 0xFF, 0xFF, 0xFF, 0xFE]);
        assert_eq!(r.ue(), Some(u32::MAX - 1));
    }
}

/// Bit writer used by tests to build synthetic parameter sets.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct BitWriter {
    bytes: Vec<u8>,
    nbits: usize,
}

#[cfg(test)]
impl BitWriter {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn bit(&mut self, b: u32) -> &mut Self {
        if self.nbits % 8 == 0 {
            self.bytes.push(0);
        }
        if b != 0 {
            *self.bytes.last_mut().unwrap() |= 0x80 >> (self.nbits % 8);
        }
        self.nbits += 1;
        self
    }

    pub(crate) fn bits(&mut self, value: u64, n: u32) -> &mut Self {
        for i in (0..n).rev() {
            self.bit(((value >> i) & 1) as u32);
        }
        self
    }

    pub(crate) fn ue(&mut self, v: u32) -> &mut Self {
        let x = u64::from(v) + 1;
        let len = 64 - x.leading_zeros();
        self.bits(0, len - 1);
        self.bits(x, len)
    }

    /// Appends the RBSP trailing bits and returns the bytes.
    pub(crate) fn finish(&mut self) -> Vec<u8> {
        self.bit(1);
        while self.nbits % 8 != 0 {
            self.bit(0);
        }
        std::mem::take(&mut self.bytes)
    }
}

#[cfg(test)]
mod writer_tests {
    use super::*;

    #[test]
    fn bit_writer_roundtrips_through_reader() {
        let mut w = BitWriter::new();
        w.ue(0).ue(1).ue(2).ue(255).ue(70_000).bits(0b101, 3);
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.ue(), Some(0));
        assert_eq!(r.ue(), Some(1));
        assert_eq!(r.ue(), Some(2));
        assert_eq!(r.ue(), Some(255));
        assert_eq!(r.ue(), Some(70_000));
        assert_eq!(r.bits(3), Some(0b101));
    }
}
