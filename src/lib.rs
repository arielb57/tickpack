//! tickpack: a lossless codec for tick data columns.
//!
//! * [`price`]: finds the decimal scale and tick size of each block of f64
//!   prices and bit-packs tick deltas, falling back to Gorilla XOR per block.
//! * [`timestamp`]: Gorilla delta-of-delta.
//! * [`size`]: frame-of-reference varints.
//! * [`gorilla`]: the reference Gorilla implementation used as the baseline.
//! * [`gen`]: deterministic synthetic tick generator for tests and benchmarks.
//!
//! ```
//! let prices = [101.30, 101.31, 101.31, 101.29, 101.30];
//! let bytes = tickpack::price::encode(&prices);
//! let back = tickpack::price::decode(&bytes).unwrap();
//! assert!(prices.iter().zip(&back).all(|(a, b)| a.to_bits() == b.to_bits()));
//! ```

pub mod bits;
pub mod gen;
pub mod gorilla;
pub mod price;
pub mod size;

/// Timestamp column codec. Tick timestamps are what Gorilla's delta-of-delta
/// scheme was designed for, so tickpack uses it unchanged.
pub mod timestamp {
    pub use crate::gorilla::{decode_timestamps as decode, encode_timestamps as encode};
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    UnexpectedEof,
    Corrupt(&'static str),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::UnexpectedEof => write!(f, "unexpected end of input"),
            DecodeError::Corrupt(why) => write!(f, "corrupt input: {why}"),
        }
    }
}

impl std::error::Error for DecodeError {}
