//! Reference Gorilla codecs (Pelkonen et al., "Gorilla: A Fast, Scalable,
//! In-Memory Time Series Database", VLDB 2015), used as the baseline.
//!
//! Values: XOR with the previous value; `0` for an identical value, `10` plus
//! the meaningful bits when they fit in the previous leading/trailing window,
//! `11` plus 5 bits of leading zeros, 6 bits of length (0 means 64) and the
//! meaningful bits otherwise.
//!
//! Timestamps: delta-of-delta with the paper's buckets `0`, `10`+7 bits,
//! `110`+9 bits, `1110`+12 bits. The paper's last bucket is `1111`+32 bits; here
//! it is split into `11110`+32 bits and `11111`+64 bits so arbitrary i64
//! timestamps (nanoseconds, out-of-order feeds) round-trip. The first
//! timestamp is stored raw in 64 bits and the first delta goes through the
//! same buckets (against a previous delta of 0) instead of the paper's fixed
//! 14-bit field, which assumed seconds inside a two-hour window.
//!
//! Stream layout for both: LEB128 value count, then the bit stream.

use crate::bits::{get_varint, put_varint, BitReader, BitSink, BitWriter};
use crate::DecodeError;

/// Running state of the XOR value encoder. The tickpack price codec shares
/// this state across its blocks so that its fallback path costs exactly what
/// the baseline would.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct XorState {
    started: bool,
    prev: u64,
    leading: u32,
    trailing: u32,
    has_window: bool,
}

impl XorState {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn encode<S: BitSink>(&mut self, value: f64, sink: &mut S) {
        let x = value.to_bits();
        if !self.started {
            sink.put(x, 64);
            self.prev = x;
            self.started = true;
            return;
        }
        let xor = x ^ self.prev;
        self.prev = x;
        if xor == 0 {
            sink.put(0, 1);
            return;
        }
        let lz = xor.leading_zeros().min(31);
        let tz = xor.trailing_zeros();
        if self.has_window && lz >= self.leading && tz >= self.trailing {
            let len = 64 - self.leading - self.trailing;
            sink.put(0b10, 2);
            sink.put(xor >> self.trailing, len);
        } else {
            let len = 64 - lz - tz;
            sink.put(0b11, 2);
            sink.put(lz as u64, 5);
            sink.put((len % 64) as u64, 6);
            sink.put(xor >> tz, len);
            self.leading = lz;
            self.trailing = tz;
            self.has_window = true;
        }
    }

    #[inline]
    pub fn decode(&mut self, r: &mut BitReader) -> Result<f64, DecodeError> {
        if !self.started {
            self.prev = r.read(64)?;
            self.started = true;
            return Ok(f64::from_bits(self.prev));
        }
        if !r.read_bit()? {
            return Ok(f64::from_bits(self.prev));
        }
        let xor = if !r.read_bit()? {
            if !self.has_window {
                return Err(DecodeError::Corrupt("XOR window reused before being set"));
            }
            let len = 64 - self.leading - self.trailing;
            r.read(len)? << self.trailing
        } else {
            let lz = r.read(5)? as u32;
            let len = match r.read(6)? as u32 {
                0 => 64,
                l => l,
            };
            if lz + len > 64 {
                return Err(DecodeError::Corrupt("XOR window wider than 64 bits"));
            }
            let tz = 64 - lz - len;
            self.leading = lz;
            self.trailing = tz;
            self.has_window = true;
            r.read(len)? << tz
        };
        self.prev ^= xor;
        Ok(f64::from_bits(self.prev))
    }
}

pub fn encode_f64(values: &[f64]) -> Vec<u8> {
    let mut prefix = Vec::with_capacity(values.len() * 2 + 10);
    put_varint(&mut prefix, values.len() as u64);
    let mut w = BitWriter::with_prefix(prefix);
    let mut st = XorState::new();
    for &v in values {
        st.encode(v, &mut w);
    }
    w.finish()
}

pub fn decode_f64(data: &[u8]) -> Result<Vec<f64>, DecodeError> {
    let mut pos = 0;
    let n = get_varint(data, &mut pos)?;
    let mut r = BitReader::new(&data[pos..]);
    let mut st = XorState::new();
    let mut out = Vec::new();
    for _ in 0..n {
        out.push(st.decode(&mut r)?);
    }
    Ok(out)
}

/// Delta-of-delta timestamp encoder state.
#[derive(Debug, Clone, Copy, Default)]
pub struct DodState {
    started: bool,
    prev: i64,
    prev_delta: i64,
}

impl DodState {
    #[inline]
    pub fn encode<S: BitSink>(&mut self, t: i64, sink: &mut S) {
        if !self.started {
            sink.put(t as u64, 64);
            self.prev = t;
            self.started = true;
            return;
        }
        let delta = t.wrapping_sub(self.prev);
        let dod = delta.wrapping_sub(self.prev_delta);
        self.prev = t;
        self.prev_delta = delta;
        match dod {
            0 => sink.put(0, 1),
            -63..=64 => {
                sink.put(0b10, 2);
                sink.put((dod + 63) as u64, 7);
            }
            -255..=256 => {
                sink.put(0b110, 3);
                sink.put((dod + 255) as u64, 9);
            }
            -2047..=2048 => {
                sink.put(0b1110, 4);
                sink.put((dod + 2047) as u64, 12);
            }
            _ if i32::try_from(dod).is_ok() => {
                sink.put(0b11110, 5);
                sink.put(dod as u64, 32);
            }
            _ => {
                sink.put(0b11111, 5);
                sink.put(dod as u64, 64);
            }
        }
    }

    #[inline]
    pub fn decode(&mut self, r: &mut BitReader) -> Result<i64, DecodeError> {
        if !self.started {
            self.prev = r.read(64)? as i64;
            self.started = true;
            return Ok(self.prev);
        }
        let dod = if !r.read_bit()? {
            0
        } else if !r.read_bit()? {
            r.read(7)? as i64 - 63
        } else if !r.read_bit()? {
            r.read(9)? as i64 - 255
        } else if !r.read_bit()? {
            r.read(12)? as i64 - 2047
        } else if !r.read_bit()? {
            r.read(32)? as u32 as i32 as i64
        } else {
            r.read(64)? as i64
        };
        self.prev_delta = self.prev_delta.wrapping_add(dod);
        self.prev = self.prev.wrapping_add(self.prev_delta);
        Ok(self.prev)
    }
}

pub fn encode_timestamps(ts: &[i64]) -> Vec<u8> {
    let mut prefix = Vec::with_capacity(ts.len() / 2 + 10);
    put_varint(&mut prefix, ts.len() as u64);
    let mut w = BitWriter::with_prefix(prefix);
    let mut st = DodState::default();
    for &t in ts {
        st.encode(t, &mut w);
    }
    w.finish()
}

pub fn decode_timestamps(data: &[u8]) -> Result<Vec<i64>, DecodeError> {
    let mut pos = 0;
    let n = get_varint(data, &mut pos)?;
    let mut r = BitReader::new(&data[pos..]);
    let mut st = DodState::default();
    let mut out = Vec::new();
    for _ in 0..n {
        out.push(st.decode(&mut r)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bit_string(values: &[(u64, u32)]) -> String {
        values
            .iter()
            .map(|&(v, b)| format!("{:0width$b}", v, width = b as usize))
            .collect()
    }

    fn encode_bits_f64(values: &[f64]) -> String {
        let mut w = BitWriter::new();
        let mut st = XorState::new();
        for &v in values {
            st.encode(v, &mut w);
        }
        let n = w.bit_len() as usize;
        let bytes = w.finish();
        let s: String = bytes.iter().map(|b| format!("{b:08b}")).collect();
        s[..n].to_string()
    }

    /// Figure 2 of the Gorilla paper: values 12, 12, 24 encode as the raw 64
    /// bits of 12.0, then `0`, then `11` + 11 leading zeros (01011) + one
    /// meaningful bit (000001) + `1`.
    #[test]
    fn paper_worked_example_values() {
        let bits = encode_bits_f64(&[12.0, 12.0, 24.0]);
        let expected = bit_string(&[
            (12.0f64.to_bits(), 64),
            (0, 1),
            (0b11, 2),
            (0b01011, 5),
            (0b000001, 6),
            (1, 1),
        ]);
        assert_eq!(bits, expected);
        assert_eq!(bits.len(), 64 + 1 + 14);
    }

    /// Figure 2 timestamps: 02:01:02, 02:02:02, 02:03:02 after the 02:00:00
    /// header give deltas 62, 60, 60, so the paper's stream continues with
    /// `10`:-2 and then `0`.
    #[test]
    fn paper_worked_example_timestamps() {
        let header = 1_427_162_400i64; // 2015-03-24 02:00:00 UTC
        let ts = [header, header + 62, header + 122, header + 182];
        let mut w = BitWriter::new();
        let mut st = DodState::default();
        for &t in &ts {
            st.encode(t, &mut w);
        }
        let n = w.bit_len() as usize;
        let bytes = w.finish();
        let s: String = bytes.iter().map(|b| format!("{b:08b}")).collect();
        let expected = bit_string(&[
            (header as u64, 64),
            (0b10, 2),
            ((62 + 63) as u64, 7), // first delta 62 against a previous delta of 0
            (0b10, 2),
            ((-2i64 + 63) as u64, 7), // dod -2, as in the paper
            (0, 1),                   // dod 0
        ]);
        assert_eq!(&s[..n], expected);
        assert_eq!(decode_timestamps(&encode_timestamps(&ts)).unwrap(), ts);
    }

    #[test]
    fn dod_bucket_boundaries_roundtrip() {
        let mut ts = vec![0i64];
        let dods = [
            -63,
            64,
            -64,
            65,
            -255,
            256,
            -256,
            257,
            -2047,
            2048,
            -2048,
            2049,
            i64::MAX,
            i64::MIN,
            0,
        ];
        let mut delta = 0i64;
        for d in dods {
            delta = delta.wrapping_add(d);
            let last = *ts.last().unwrap();
            ts.push(last.wrapping_add(delta));
        }
        assert_eq!(decode_timestamps(&encode_timestamps(&ts)).unwrap(), ts);
    }

    #[test]
    fn bucket_costs_match_paper() {
        let cost = |dod: i64| {
            let mut c = crate::bits::BitCounter::default();
            let mut st = DodState::default();
            st.encode(0, &mut c);
            st.encode(0, &mut c);
            let before = c.bits;
            st.encode(dod, &mut c);
            c.bits - before
        };
        assert_eq!(cost(0), 1);
        assert_eq!(cost(-63), 9);
        assert_eq!(cost(64), 9);
        assert_eq!(cost(65), 12);
        assert_eq!(cost(256), 12);
        assert_eq!(cost(-2047), 16);
        assert_eq!(cost(2049), 37);
        assert_eq!(cost(i32::MIN as i64), 37);
        assert_eq!(cost(i32::MAX as i64 + 1), 69);
    }

    #[test]
    fn xor_roundtrip_special_values() {
        let v = [
            f64::NAN,
            -0.0,
            0.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MAX,
            f64::MIN_POSITIVE / 3.0,
            f64::from_bits(0x7ff0_0000_dead_beef),
            1.0,
            1.0,
            -1.0,
        ];
        let d = decode_f64(&encode_f64(&v)).unwrap();
        assert_eq!(
            d.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
            v.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn truncated_stream_is_an_error() {
        let enc = encode_f64(&[1.5, 2.25, 3.125, 100.0]);
        for cut in 1..enc.len() {
            assert!(decode_f64(&enc[..cut]).is_err(), "cut at {cut}");
        }
    }
}
