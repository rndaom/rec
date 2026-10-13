//! Tiny helpers for serialising ISO BMFF boxes into a `Vec<u8>`.

/// Big-endian primitives on `Vec<u8>`.
pub(crate) trait Put {
    fn put_u8(&mut self, v: u8);
    fn put_u16(&mut self, v: u16);
    fn put_u32(&mut self, v: u32);
    fn put_u64(&mut self, v: u64);
    fn put_zeros(&mut self, n: usize);
    fn put_fourcc(&mut self, cc: &[u8; 4]);
}

impl Put for Vec<u8> {
    fn put_u8(&mut self, v: u8) {
        self.push(v);
    }
    fn put_u16(&mut self, v: u16) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn put_u32(&mut self, v: u32) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn put_u64(&mut self, v: u64) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn put_zeros(&mut self, n: usize) {
        self.resize(self.len() + n, 0);
    }
    fn put_fourcc(&mut self, cc: &[u8; 4]) {
        self.extend_from_slice(cc);
    }
}

/// Writes a plain box: 32-bit size, fourcc, then whatever `body` appends.
/// The caller guarantees the box is under 4 GiB (every box we write is small
/// except `mdat`, which is handled separately).
pub(crate) fn boxed(buf: &mut Vec<u8>, fourcc: &[u8; 4], body: impl FnOnce(&mut Vec<u8>)) {
    let start = buf.len();
    buf.put_u32(0);
    buf.put_fourcc(fourcc);
    body(buf);
    let size = (buf.len() - start) as u32;
    buf[start..start + 4].copy_from_slice(&size.to_be_bytes());
}

/// Writes a full box (version byte + 24-bit flags before the body).
pub(crate) fn full_box(
    buf: &mut Vec<u8>,
    fourcc: &[u8; 4],
    version: u8,
    flags: u32,
    body: impl FnOnce(&mut Vec<u8>),
) {
    boxed(buf, fourcc, |b| {
        b.put_u8(version);
        b.extend_from_slice(&flags.to_be_bytes()[1..]);
        body(b);
    });
}

/// The identity transformation matrix used by `mvhd` and `tkhd`.
pub(crate) const UNITY_MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_boxes_get_correct_sizes() {
        let mut b = Vec::new();
        boxed(&mut b, b"moov", |b| {
            full_box(b, b"mfhd", 0, 0x0000_0102, |b| b.put_u32(7));
            boxed(b, b"free", |_| {});
        });
        assert_eq!(&b[0..8], [0, 0, 0, 32, b'm', b'o', b'o', b'v']);
        assert_eq!(
            &b[8..24],
            [0, 0, 0, 16, b'm', b'f', b'h', b'd', 0, 0, 1, 2, 0, 0, 0, 7]
        );
        assert_eq!(&b[24..32], [0, 0, 0, 8, b'f', b'r', b'e', b'e']);
        assert_eq!(b.len(), 32);
    }
}
