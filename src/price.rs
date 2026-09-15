//! Price column codec: decimal-grid detection with a Gorilla XOR fallback.
//!
//! Stream layout: LEB128 value count, LEB128 block size, then one bit stream
//! holding every block. Each block starts with a 2-bit tag:
//!
//! * `00` grid block: 4 bits scale, sized zigzag base mantissa, sized tick,
//!   sized zigzag frame-of-reference minimum, 7 bits delta width, 2 bits
//!   payload mode, then the tick deltas.
//! * `01` Gorilla block: the values XOR-encoded, continuing the XOR state of
//!   the whole column.
//! * `10`, `11` are reserved and rejected by the decoder.
//!
//! A "sized" field is 7 bits of width followed by that many bits.

use crate::bits::{
    get_sized, get_varint, put_sized, put_varint, sized_cost, unzigzag, width, zigzag, BitCounter,
    BitReader, BitSink, BitWriter,
};
use crate::gorilla::XorState;
use crate::DecodeError;

pub const DEFAULT_BLOCK_SIZE: usize = 1024;
pub const MAX_SCALE: u32 = 12;

const TAG_GRID: u64 = 0b00;
const TAG_GORILLA: u64 = 0b01;

const POW10: [f64; 13] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12,
];

/// How the tick deltas of a grid block are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Every delta as `q - min` in `width` bits.
    Packed = 0,
    /// One flag bit per delta; non-zero deltas follow the flag in `width` bits.
    ZeroFlag = 1,
    /// Zero runs: run length in `run_width` bits, then the non-zero delta.
    Runs = 2,
}

/// What the encoder decided for one block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Grid {
        scale: u32,
        /// Tick size in mantissa units at `scale`.
        tick: u64,
        mode: Mode,
    },
    Gorilla {
        /// Why the grid path was not used.
        reason: FallbackReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    /// Some value has no exact decimal representation at scale 0..=12
    /// (NaN, ±inf, -0.0, subnormal, off-grid, or a mantissa outside i64).
    NoDecimalScale,
    /// Consecutive mantissas differ by more than i64 can hold.
    DeltaOverflow,
    /// The grid encoding existed but Gorilla was not larger.
    Smaller,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockInfo {
    pub start: usize,
    pub len: usize,
    pub kind: BlockKind,
    /// Bits used by the block, tag included.
    pub bits: u64,
}

/// Mantissa of `v` at `scale` if it reconstructs bit-for-bit, using exactly
/// the arithmetic the decoder uses.
#[inline]
pub fn exact_mantissa(v: f64, scale: u32) -> Option<i64> {
    let p = POW10[scale as usize];
    let r = (v * p).round();
    // i64::MIN as f64 is exactly -2^63; NaN is contained in no range.
    if !(i64::MIN as f64..-(i64::MIN as f64)).contains(&r) {
        return None;
    }
    let m = r as i64;
    ((m as f64 / p).to_bits() == v.to_bits()).then_some(m)
}

/// The smallest scale in 0..=12 at which every value is exact.
pub fn detect_scale(values: &[f64]) -> Option<u32> {
    (0..=MAX_SCALE).find(|&k| values.iter().all(|&v| exact_mantissa(v, k).is_some()))
}

/// Number of digits after the decimal point in Rust's shortest round-trip
/// formatting of `v`. Used as an independent oracle for [`detect_scale`].
pub fn shortest_decimals(v: f64) -> Option<u32> {
    if !v.is_finite() {
        return None;
    }
    let s = format!("{v}");
    Some(s.split_once('.').map_or(0, |(_, frac)| frac.len() as u32))
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

struct GridPlan {
    scale: u32,
    base: i64,
    tick: u64,
    qmin: i64,
    width: u32,
    mode: Mode,
    run_width: u32,
    bits: u64,
}

/// Plans a grid block. `q` receives the tick deltas.
fn plan_grid(
    values: &[f64],
    q: &mut Vec<i64>,
    m: &mut Vec<i64>,
) -> Result<GridPlan, FallbackReason> {
    let scale = detect_scale(values).ok_or(FallbackReason::NoDecimalScale)?;
    m.clear();
    m.extend(
        values
            .iter()
            .map(|&v| exact_mantissa(v, scale).expect("detect_scale verified every value")),
    );
    let mut g = 0u64;
    q.clear();
    for w in m.windows(2) {
        let d = w[1]
            .checked_sub(w[0])
            .ok_or(FallbackReason::DeltaOverflow)?;
        q.push(d);
        g = gcd(g, d.unsigned_abs());
    }
    let tick = g.max(1);
    if tick > 1 {
        for d in q.iter_mut() {
            *d = (*d as i128 / tick as i128) as i64;
        }
    }
    let (mut qmin, mut qmax) = (0i64, 0i64);
    if let Some(&first) = q.first() {
        (qmin, qmax) = (first, first);
        for &d in q.iter() {
            qmin = qmin.min(d);
            qmax = qmax.max(d);
        }
    }
    let delta_width = width((qmax as i128 - qmin as i128) as u64);
    let n = q.len() as u64;

    let mut nonzero = 0u64;
    let mut run = 0u64;
    let mut max_run = 0u64;
    for &d in q.iter() {
        if d == 0 {
            run += 1;
        } else {
            nonzero += 1;
            max_run = max_run.max(run);
            run = 0;
        }
    }
    let trailing = run;
    max_run = max_run.max(trailing);
    let run_width = width(max_run);

    let packed = n * delta_width as u64;
    let flagged = n + nonzero * delta_width as u64;
    let runs = 6
        + nonzero * (run_width + delta_width) as u64
        + if trailing > 0 { run_width as u64 } else { 0 };
    let (mode, payload) = if packed <= flagged && packed <= runs {
        (Mode::Packed, packed)
    } else if flagged <= runs {
        (Mode::ZeroFlag, flagged)
    } else {
        (Mode::Runs, runs)
    };
    let header =
        2 + 4 + sized_cost(zigzag(m[0])) + sized_cost(tick) + sized_cost(zigzag(qmin)) + 7 + 2;
    Ok(GridPlan {
        scale,
        base: m[0],
        tick,
        qmin,
        width: delta_width,
        mode,
        run_width,
        bits: header + payload,
    })
}

fn write_grid(w: &mut BitWriter, plan: &GridPlan, q: &[i64]) {
    w.put(TAG_GRID, 2);
    w.put(plan.scale as u64, 4);
    put_sized(w, zigzag(plan.base));
    put_sized(w, plan.tick);
    put_sized(w, zigzag(plan.qmin));
    w.put(plan.width as u64, 7);
    w.put(plan.mode as u64, 2);
    let off = |d: i64| (d as i128 - plan.qmin as i128) as u64;
    match plan.mode {
        Mode::Packed => {
            for &d in q {
                w.put(off(d), plan.width);
            }
        }
        Mode::ZeroFlag => {
            for &d in q {
                if d == 0 {
                    w.put(0, 1);
                } else {
                    w.put(1, 1);
                    w.put(off(d), plan.width);
                }
            }
        }
        Mode::Runs => {
            w.put(plan.run_width as u64, 6);
            let mut run = 0u64;
            for &d in q {
                if d == 0 {
                    run += 1;
                } else {
                    w.put(run, plan.run_width);
                    w.put(off(d), plan.width);
                    run = 0;
                }
            }
            if run > 0 {
                w.put(run, plan.run_width);
            }
        }
    }
}

/// Encode with [`DEFAULT_BLOCK_SIZE`].
pub fn encode(values: &[f64]) -> Vec<u8> {
    encode_with(values, DEFAULT_BLOCK_SIZE).0
}

/// Encode with a chosen block size and report the decision for each block.
///
/// # Panics
/// If `block_size` is 0.
pub fn encode_with(values: &[f64], block_size: usize) -> (Vec<u8>, Vec<BlockInfo>) {
    assert!(block_size > 0, "block size must be positive");
    let mut prefix = Vec::with_capacity(values.len() / 2 + 16);
    put_varint(&mut prefix, values.len() as u64);
    put_varint(&mut prefix, block_size as u64);
    let mut w = BitWriter::with_prefix(prefix);
    let mut xor = XorState::new();
    let mut infos = Vec::with_capacity(values.len().div_ceil(block_size));
    let (mut q, mut m) = (
        Vec::with_capacity(block_size),
        Vec::with_capacity(block_size),
    );

    for (i, block) in values.chunks(block_size).enumerate() {
        let start_bits = w.bit_len();
        let mut after = xor;
        let mut counter = BitCounter::default();
        for &v in block {
            after.encode(v, &mut counter);
        }
        let gorilla_bits = 2 + counter.bits;
        let kind = match plan_grid(block, &mut q, &mut m) {
            Ok(plan) if plan.bits < gorilla_bits => {
                write_grid(&mut w, &plan, &q);
                BlockKind::Grid {
                    scale: plan.scale,
                    tick: plan.tick,
                    mode: plan.mode,
                }
            }
            other => {
                w.put(TAG_GORILLA, 2);
                let mut st = xor;
                for &v in block {
                    st.encode(v, &mut w);
                }
                debug_assert_eq!(st, after);
                BlockKind::Gorilla {
                    reason: other.err().unwrap_or(FallbackReason::Smaller),
                }
            }
        };
        // Grid blocks still advance the XOR state as if they had been
        // XOR-encoded, so a later fallback block costs what the baseline pays.
        xor = after;
        infos.push(BlockInfo {
            start: i * block_size,
            len: block.len(),
            kind,
            bits: w.bit_len() - start_bits,
        });
    }
    (w.finish(), infos)
}

pub fn decode(data: &[u8]) -> Result<Vec<f64>, DecodeError> {
    let mut pos = 0;
    let n = get_varint(data, &mut pos)?;
    let block_size = get_varint(data, &mut pos)?;
    if block_size == 0 && n > 0 {
        return Err(DecodeError::Corrupt("zero block size"));
    }
    let mut r = BitReader::new(&data[pos..]);
    let mut xor = XorState::new();
    let mut out: Vec<f64> = Vec::new();
    let mut counter = BitCounter::default();
    let mut remaining = n;
    while remaining > 0 {
        let len = remaining.min(block_size);
        remaining -= len;
        let start = out.len();
        match r.read(2)? {
            TAG_GRID => {
                decode_grid(&mut r, len, &mut out)?;
                for &v in &out[start..] {
                    xor.encode(v, &mut counter);
                }
            }
            TAG_GORILLA => {
                for _ in 0..len {
                    out.push(xor.decode(&mut r)?);
                }
            }
            _ => return Err(DecodeError::Corrupt("reserved block tag")),
        }
    }
    Ok(out)
}

fn decode_grid(r: &mut BitReader, len: u64, out: &mut Vec<f64>) -> Result<(), DecodeError> {
    let scale = r.read(4)? as u32;
    if scale > MAX_SCALE {
        return Err(DecodeError::Corrupt("scale above 12"));
    }
    let p = POW10[scale as usize];
    let mut m = unzigzag(get_sized(r)?);
    let tick = get_sized(r)? as i64;
    let qmin = unzigzag(get_sized(r)?);
    let w = r.read(7)? as u32;
    if w > 64 {
        return Err(DecodeError::Corrupt("delta width above 64"));
    }
    let mode = r.read(2)?;
    let run_width = if mode == Mode::Runs as u64 {
        r.read(6)? as u32
    } else {
        0
    };
    out.push(m as f64 / p);
    let deltas = len - 1;
    let step = |m: &mut i64, off: u64, out: &mut Vec<f64>| {
        let d = qmin.wrapping_add(off as i64);
        *m = m.wrapping_add(d.wrapping_mul(tick));
        out.push(*m as f64 / p);
    };
    match mode {
        0 => {
            for _ in 0..deltas {
                let off = r.read(w)?;
                step(&mut m, off, out);
            }
        }
        1 => {
            for _ in 0..deltas {
                if r.read_bit()? {
                    let off = r.read(w)?;
                    step(&mut m, off, out);
                } else {
                    out.push(m as f64 / p);
                }
            }
        }
        2 => {
            let mut done = 0u64;
            while done < deltas {
                let run = r.read(run_width)?;
                if run > deltas - done {
                    return Err(DecodeError::Corrupt("zero run past end of block"));
                }
                for _ in 0..run {
                    out.push(m as f64 / p);
                }
                done += run;
                if done == deltas {
                    break;
                }
                let off = r.read(w)?;
                step(&mut m, off, out);
                done += 1;
            }
        }
        _ => return Err(DecodeError::Corrupt("reserved payload mode")),
    }
    Ok(())
}
