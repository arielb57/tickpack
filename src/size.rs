//! Size column codec: per-block frame of reference plus varints.
//!
//! Layout: LEB128 count, LEB128 block size, then per block the zigzag varint
//! of the block minimum followed by one LEB128 varint per value holding
//! `value - minimum` (always non-negative, so it needs no zigzag).

use crate::bits::{get_varint, put_varint, unzigzag, zigzag};
use crate::DecodeError;

pub const DEFAULT_BLOCK_SIZE: usize = 1024;

pub fn encode(sizes: &[i64]) -> Vec<u8> {
    encode_with(sizes, DEFAULT_BLOCK_SIZE)
}

/// # Panics
/// If `block_size` is 0.
pub fn encode_with(sizes: &[i64], block_size: usize) -> Vec<u8> {
    assert!(block_size > 0, "block size must be positive");
    let mut out = Vec::with_capacity(sizes.len() + 16);
    put_varint(&mut out, sizes.len() as u64);
    put_varint(&mut out, block_size as u64);
    for block in sizes.chunks(block_size) {
        let min = *block.iter().min().expect("chunks are never empty");
        put_varint(&mut out, zigzag(min));
        for &s in block {
            put_varint(&mut out, (s as i128 - min as i128) as u64);
        }
    }
    out
}

pub fn decode(data: &[u8]) -> Result<Vec<i64>, DecodeError> {
    let mut pos = 0;
    let n = get_varint(data, &mut pos)?;
    let block_size = get_varint(data, &mut pos)?;
    if block_size == 0 && n > 0 {
        return Err(DecodeError::Corrupt("zero block size"));
    }
    let mut out = Vec::new();
    let mut remaining = n;
    while remaining > 0 {
        let len = remaining.min(block_size);
        remaining -= len;
        let min = unzigzag(get_varint(data, &mut pos)?);
        for _ in 0..len {
            out.push(min.wrapping_add(get_varint(data, &mut pos)? as i64));
        }
    }
    if pos != data.len() {
        return Err(DecodeError::Corrupt("trailing bytes after size column"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extremes_roundtrip_across_blocks() {
        let v = vec![i64::MIN, i64::MAX, 0, -1, 100, 100, 200, i64::MAX, i64::MIN];
        for bs in 1..=10 {
            assert_eq!(decode(&encode_with(&v, bs)).unwrap(), v, "block size {bs}");
        }
    }

    #[test]
    fn frame_of_reference_shrinks_round_lots() {
        let v: Vec<i64> = (0..1000).map(|i| 1_000_000 + (i % 5) * 100).collect();
        let enc = encode(&v);
        // Offsets up to 400 take two bytes; without the reference each value
        // would take three.
        assert!(enc.len() < 2 * v.len() + 16, "{} bytes", enc.len());
        assert_eq!(decode(&enc).unwrap(), v);
    }

    #[test]
    fn empty_and_truncated() {
        assert_eq!(decode(&encode(&[])).unwrap(), Vec::<i64>::new());
        let enc = encode(&[5, 6, 7]);
        for cut in 0..enc.len() {
            assert!(decode(&enc[..cut]).is_err());
        }
        let mut extra = enc.clone();
        extra.push(0);
        assert!(decode(&extra).is_err());
    }
}
