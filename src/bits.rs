//! MSB-first bit streams, LEB128 varints and zigzag mapping.

use crate::DecodeError;

/// Destination for bits. Implemented by [`BitWriter`] and [`BitCounter`] so the
/// same encoding routine can either emit bits or only measure their cost.
pub trait BitSink {
    /// Append the low `bits` bits of `value` (`bits` in 0..=64).
    fn put(&mut self, value: u64, bits: u32);
}

/// Counts bits without storing them.
#[derive(Debug, Default, Clone, Copy)]
pub struct BitCounter {
    pub bits: u64,
}

impl BitSink for BitCounter {
    #[inline]
    fn put(&mut self, _value: u64, bits: u32) {
        self.bits += bits as u64;
    }
}

#[derive(Debug, Default)]
pub struct BitWriter {
    buf: Vec<u8>,
    acc: u128,
    pending: u32,
    len: u64,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a bit stream after an existing byte prefix.
    pub fn with_prefix(prefix: Vec<u8>) -> Self {
        Self {
            buf: prefix,
            ..Self::default()
        }
    }

    /// Number of bits written through [`BitSink::put`] (the prefix is excluded).
    pub fn bit_len(&self) -> u64 {
        self.len
    }

    pub fn finish(mut self) -> Vec<u8> {
        if self.pending > 0 {
            let bytes = self.pending.div_ceil(8);
            let aligned = self.acc << (bytes * 8 - self.pending);
            for i in (0..bytes).rev() {
                self.buf.push((aligned >> (i * 8)) as u8);
            }
        }
        self.buf
    }
}

impl BitSink for BitWriter {
    #[inline]
    fn put(&mut self, value: u64, bits: u32) {
        if bits == 0 {
            return;
        }
        debug_assert!(bits <= 64);
        let masked = if bits == 64 {
            value
        } else {
            value & ((1u64 << bits) - 1)
        };
        self.acc = (self.acc << bits) | masked as u128;
        self.pending += bits;
        self.len += bits as u64;
        if self.pending >= 64 {
            self.pending -= 64;
            let word = (self.acc >> self.pending) as u64;
            self.buf.extend_from_slice(&word.to_be_bytes());
            self.acc &= (1u128 << self.pending) - 1;
        }
    }
}

pub struct BitReader<'a> {
    data: &'a [u8],
    pos: u64,
    limit: u64,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            limit: data.len() as u64 * 8,
        }
    }

    #[inline]
    pub fn read(&mut self, bits: u32) -> Result<u64, DecodeError> {
        if bits == 0 {
            return Ok(0);
        }
        if bits > 64 || self.pos + bits as u64 > self.limit {
            return Err(DecodeError::UnexpectedEof);
        }
        let byte = (self.pos / 8) as usize;
        let shift = (self.pos % 8) as u32;
        let word = if byte + 16 <= self.data.len() {
            let mut b = [0u8; 16];
            b.copy_from_slice(&self.data[byte..byte + 16]);
            u128::from_be_bytes(b)
        } else {
            let mut b = [0u8; 16];
            let tail = &self.data[byte..];
            b[..tail.len()].copy_from_slice(tail);
            u128::from_be_bytes(b)
        };
        self.pos += bits as u64;
        Ok(((word << shift) >> (128 - bits)) as u64)
    }

    #[inline]
    pub fn read_bit(&mut self) -> Result<bool, DecodeError> {
        Ok(self.read(1)? == 1)
    }
}

#[inline]
pub fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
pub fn unzigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

/// Bits needed to represent `v` (0 for 0).
#[inline]
pub fn width(v: u64) -> u32 {
    64 - v.leading_zeros()
}

pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Read a LEB128 varint from `data` at `*pos`, advancing `*pos`.
pub fn get_varint(data: &[u8], pos: &mut usize) -> Result<u64, DecodeError> {
    let mut v = 0u64;
    for i in 0..10 {
        let byte = *data.get(*pos).ok_or(DecodeError::UnexpectedEof)?;
        *pos += 1;
        let payload = (byte & 0x7f) as u64;
        if i == 9 && payload > 1 {
            return Err(DecodeError::Corrupt("varint overflows u64"));
        }
        v |= payload << (7 * i);
        if byte & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(DecodeError::Corrupt("varint longer than 10 bytes"))
}

/// A varint-prefixed field of known width: 7 bits of width, then the value.
pub fn put_sized<S: BitSink>(sink: &mut S, v: u64) {
    let w = width(v);
    sink.put(w as u64, 7);
    sink.put(v, w);
}

pub fn sized_cost(v: u64) -> u64 {
    7 + width(v) as u64
}

pub fn get_sized(r: &mut BitReader) -> Result<u64, DecodeError> {
    let w = r.read(7)? as u32;
    if w > 64 {
        return Err(DecodeError::Corrupt("field width above 64"));
    }
    r.read(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_reader_roundtrip_mixed_widths() {
        let fields: Vec<(u64, u32)> = (0..500u64)
            .map(|i| {
                let bits = (i % 65) as u32;
                let v = i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
                let v = if bits == 64 {
                    v
                } else {
                    v & ((1u64 << bits) - 1)
                };
                (v, bits)
            })
            .collect();
        let mut w = BitWriter::new();
        for &(v, b) in &fields {
            w.put(v, b);
        }
        let total: u64 = fields.iter().map(|f| f.1 as u64).sum();
        assert_eq!(w.bit_len(), total);
        let bytes = w.finish();
        assert_eq!(bytes.len() as u64, total.div_ceil(8));
        let mut r = BitReader::new(&bytes);
        for &(v, b) in &fields {
            assert_eq!(r.read(b).unwrap(), v, "width {b}");
        }
    }

    #[test]
    fn reader_reports_eof_instead_of_panicking() {
        let mut w = BitWriter::new();
        w.put(0b101, 3);
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read(3).unwrap(), 0b101);
        assert_eq!(r.read(5).unwrap(), 0);
        assert_eq!(r.read(1), Err(DecodeError::UnexpectedEof));
    }

    #[test]
    fn writer_masks_high_bits() {
        let mut w = BitWriter::new();
        w.put(u64::MAX, 3);
        w.put(0, 5);
        assert_eq!(w.finish(), vec![0b1110_0000]);
    }

    #[test]
    fn zigzag_extremes() {
        for v in [0, -1, 1, i64::MIN, i64::MAX, -12345, 987654321] {
            assert_eq!(unzigzag(zigzag(v)), v);
        }
        assert_eq!(zigzag(-1), 1);
        assert_eq!(zigzag(1), 2);
        assert_eq!(zigzag(i64::MIN), u64::MAX);
    }

    #[test]
    fn varint_roundtrip_and_rejects_truncation() {
        let mut out = Vec::new();
        let vals = [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX];
        for &v in &vals {
            put_varint(&mut out, v);
        }
        let mut pos = 0;
        for &v in &vals {
            assert_eq!(get_varint(&out, &mut pos).unwrap(), v);
        }
        assert_eq!(pos, out.len());
        let mut pos = 0;
        assert_eq!(
            get_varint(&[0x80, 0x80], &mut pos),
            Err(DecodeError::UnexpectedEof)
        );
        let mut pos = 0;
        assert!(get_varint(&[0xff; 11], &mut pos).is_err());
    }
}
