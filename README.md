# tickpack

A lossless codec for tick-data columns. It finds the decimal price grid inside f64 values without being told the tick size, and falls back to Gorilla XOR, block by block, when there is no grid.

## The problem

Gorilla XOR compression (Pelkonen et al., VLDB 2015) works well on slowly drifting sensor readings. Exchange prices behave differently. They jump between multiples of a tick size, and neighbouring decimal prices such as 101.30 and 101.31 differ in about 40 bits of their binary mantissas, so almost every XOR is wide. Tick stores either pay that cost or make the user declare a scale and tick size for each instrument, and that declaration breaks when a feed changes precision partway through a session. tickpack finds the grid for each block on its own and still guarantees that every f64 decodes bit for bit, including NaN payloads and -0.0.

## How it works

The price column is cut into blocks of 1024 values. Each block goes through four steps:

1. **Decimal scale.** Find the smallest `k` in `0..=12` such that every value `v` in the block satisfies `(round(v·10^k) as i64) as f64 / 10^k` having exactly the same bits as `v`. The check uses the same arithmetic as the decoder, so a block that passes is guaranteed to reconstruct. Powers of ten up to 10^12 are exact doubles, and dividing by them is correctly rounded. The result is cross-checked in the tests against Rust's shortest round-trip formatting: `k` must equal the largest number of digits after the decimal point in `format!("{v}")`.
2. **Tick.** Map the values to i64 mantissas, take first differences, and take the GCD of their absolute values. That GCD is the tick size in mantissa units. Dividing by it turns the differences into small integers: tick deltas.
3. **Pack.** Store the scale, the base mantissa, the tick, and the tick deltas bit-packed with a frame of reference (`delta - min` in `width` bits). There are three payload layouts, and the encoder computes the exact bit cost of each and keeps the smallest:
   - *packed*: every delta in `width` bits;
   - *zero-flag*: 1 bit per value, plus `width` bits for each price that moved;
   - *zero-runs*: run length of unchanged prices, then the next move. This wins during halts.
4. **Fallback.** If any value fails step 1 (NaN, ±inf, -0.0, subnormals, off-grid values, mantissas outside i64), if a mantissa difference overflows i64, or if the Gorilla encoding of the block would be no larger, the block is written with Gorilla XOR instead. A 2-bit tag in front of every block records the choice, so the decoder never has to guess.

Worked example, one block of five prices:

```
values      101.30   101.31   101.31   101.29   101.30
scale       k=0 fails (101 != 101.3), k=1 fails (101.3 != 101.31), k=2 passes
mantissas   10130    10131    10131    10129    10130
differences          +1       0        -2       +1         gcd = 1 -> tick 1 (0.01)
offsets              3        2        0        3          min -2, width 2 bits

Gorilla XOR on the same values: meaningful windows of 40, 0, 41, 37 bits.
```

Two details keep the fallback honest:

- **Shared XOR state.** Gorilla blocks do not restart. They continue one XOR state across the whole column, and a grid block advances that state as if it had been XOR-encoded (the decoder replays it). On data with no grid at all, the output is therefore the baseline Gorilla stream plus 2 bits per block and a 2-byte header. A test checks this bound, and so does a property test on random mixed columns.
- **Exact cost comparison.** The encoder counts the Gorilla cost of every block with a bit counter that shares code with the writer, so the grid path is only taken when it is strictly smaller.

**Timestamps** use Gorilla's delta-of-delta buckets (`0`, `10`+7, `110`+9, `1110`+12 bits). The paper's final `1111`+32 bucket is split into `11110`+32 and `11111`+64 bits, so any i64 sequence round-trips. **Sizes** use a zigzag varint for each block's minimum, then an LEB128 varint of `size - min` for each value.

The baseline is not a number quoted from the paper. `src/gorilla.rs` is a full Gorilla XOR and delta-of-delta implementation, and it is checked bit for bit against the worked example in Figure 2 of the paper (values 12, 12, 24 encode as the raw 64 bits, then `0`, then `11 01011 000001 1`; timestamp deltas 62, 60, 60 give `10`:-2 then `0`).

## Install and usage

Requires a Rust toolchain (developed and linted with 1.94.1). There are no runtime dependencies. `proptest` is a dev-dependency.

```
git clone <this repository> tickpack && cd tickpack
cargo test
cargo run --release -- bench
```

`cargo test` runs 40 tests in under 10 seconds: unit tests, the price edge-case suite, proptest properties and a doctest.

Library use:

```rust
let prices = [101.30, 101.31, 101.31, 101.29, 101.30];
let bytes = tickpack::price::encode(&prices);
let back = tickpack::price::decode(&bytes).unwrap();
assert!(prices.iter().zip(&back).all(|(a, b)| a.to_bits() == b.to_bits()));

// Per-block decisions (grid scale and tick, or the fallback reason):
let (bytes, blocks) = tickpack::price::encode_with(&prices, 1024);

let ts = tickpack::timestamp::encode(&[1_700_000_000_000_000_000, 1_700_000_000_002_000_000]);
let sizes = tickpack::size::encode(&[100, 200, 100, 37]);
```

`tickpack inspect` shows what the encoder decided for each block. It prints one row each time the path, scale or tick changes:

```
$ cargo run --release -q -- inspect precision-change --n 20000
precision-change: seed 4, 20000 values, 20 blocks, 2.77 bits/value, round-trip bit-exact

   start    len  path     scale     tick mode      bits/value
       0   1024  grid         2        5 ZeroFlag        2.54
   10240   1024  grid         3        1 ZeroFlag        4.75

(one row per change of path, scale or tick; at most 40 rows)

$ cargo run --release -q -- inspect artefacts+nan-gaps --n 12000
artefacts+nan-gaps: seed 6, 12000 values, 12 blocks, 5.57 bits/value, round-trip bit-exact

   start    len  path     scale     tick mode      bits/value
       0   1024  grid         2        1 ZeroFlag        2.63
    2048   1024  gorilla      -        - no-scale       18.66
    3072   1024  grid         2        1 ZeroFlag        2.73
    6144   1024  gorilla      -        - no-scale       20.78
    7168   1024  grid         2        1 ZeroFlag        2.67

(one row per change of path, scale or tick; at most 40 rows)
```

In the first run, the feed switches from a 0.05 tick to a 0.001 tick at value 10377. tickpack moves to scale 3 in the block that contains the switch, and nobody had to declare the change.

## Results

Command: `cargo run --release -- bench` (1,000,000 prices per scenario, block size 1024, best of 5 runs). The bench checks that every decode is bit-exact before it prints anything. Measured on an Apple Silicon Mac (arm64, macOS 26), Rust 1.94.1, single thread. MB/s counts 8 bytes of input per value.

| scenario | seed | gorilla bits/value | tickpack bits/value | ratio | tickpack - gorilla, bits/block | grid blocks | gorilla enc MB/s | gorilla dec MB/s | tickpack enc MB/s | tickpack dec MB/s |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| equities-0.01 | 1 | 19.76 | 2.54 | 7.79x | -17629.18 | 977/977 | 1545 | 1483 | 654 | 889 |
| futures-0.25 | 2 | 4.40 | 2.15 | 2.05x | -2303.63 | 977/977 | 2308 | 1739 | 583 | 1025 |
| fx-0.00001 | 3 | 40.30 | 5.11 | 7.89x | -36017.17 | 977/977 | 1607 | 1548 | 619 | 1335 |
| precision-change | 4 | 22.03 | 2.66 | 8.28x | -19829.20 | 977/977 | 1667 | 1405 | 546 | 842 |
| adversarial-noise | 5 | 52.70 | 52.70 | 1.00x | +2.01 | 0/977 | 1809 | 1932 | 1305 | 1555 |
| artefacts+nan-gaps | 6 | 25.81 | 6.43 | 4.01x | -19834.01 | 817/977 | 1780 | 1466 | 609 | 912 |

Other columns, equities scenario: timestamps 36.98 bits/value, sizes 11.21 bits/value.

The scenarios are defined in `src/gen.rs` (`bench_scenarios`). All of them are random walks, with P(move) 0.4 and a geometric move size unless noted:

- **equities-0.01**: tick 0.01 from 101.30. Halts of 400 flat ticks start with probability 0.0002.
- **futures-0.25**: tick 0.25 from 4500.25, P(move) 0.3.
- **fx-0.00001**: tick 0.00001 from 1.08450, P(move) 0.8, wider moves.
- **precision-change**: tick 0.05 at scale 2, switching to tick 0.001 at scale 3 at value 500,377, which is not a block boundary.
- **adversarial-noise**: the equities walk plus uniform noise of ±0.5 tick, so no value lies on a grid.
- **artefacts+nan-gaps**: the equities walk, where 0.05% of prices are computed as `mantissa * 0.01` (giving values like 101.30000000000001), plus gaps of 20 NaNs.

Against the design targets set before the code was written:

- **At least 3x smaller than Gorilla on grid data.** Met on three of the four clean grid scenarios (7.8x, 7.9x, 8.3x). **Not met on futures-0.25 (2.05x).** A quarter tick is an exact binary fraction, so 4500.25 → 4500.50 is a narrow XOR and Gorilla already spends only 4.4 bits per value. tickpack still halves it, but grid detection has much less to win on dyadic ticks.
- **No worse than Gorilla plus 2 bits per block on adversarial data.** Met: +2.01 bits/block. That is the 2-bit tag, plus the 2-byte block-size header spread over 977 blocks. `tests/price.rs` and a property test assert the bound.
- **Throughput.** Encoding is about 2.5x slower than Gorilla on grid data (550–650 MB/s). The scale search tries up to 13 scales with a `round` per value, and the encoder also counts the Gorilla cost of every block. Decoding runs at 840–1340 MB/s, about 60–85% of Gorilla's speed.

## Design notes

**Choosing per block, at exact cost, with shared Gorilla state.** The simple design runs grid detection and uses the grid if detection succeeds. It has two weaknesses. First, it can lose to Gorilla on blocks where the grid exists but is expensive: huge ticks, scale-12 mantissas, or dyadic prices. Second, if fallback blocks restart the XOR stream, each one pays a fresh 64-bit first value, so the "Gorilla plus a tag" promise does not hold. tickpack instead measures both encodings of every block exactly and keeps the smaller one. Grid blocks also advance the XOR state as if they had been XOR-encoded. The worst case therefore has a hard bound instead of an expected one: never more than 2 bits per block over the baseline. The price is encode speed. The Gorilla counting pass costs roughly 20–30% of encode time, and on pure grid data that work is wasted. A faster encoder could skip the count when the grid cost is below a trivial bound such as 2 bits per value, but that would make the guarantee depend on getting the bound right.

**The check is the decoder, not a heuristic.** Scale detection does not parse strings or estimate digits. It runs exactly the arithmetic the decoder will run and compares `to_bits()`. That is why -0.0 (whose mantissa 0 decodes as +0.0), NaN payloads and subnormals need no special cases: they fail the check and fall back. The shortest-formatting digit count is used only as a test oracle. The tests confirm it agrees with the arithmetic check on every generated on-grid series.

**Tick recovery is a GCD, which recovers the grid the data actually uses.** The recovered tick is the declared tick multiplied by the GCD of the step counts. Converted to the generating scale, it equals the generating tick exactly whenever at least one move of a single tick happens in the block, and is a multiple of it otherwise. The property test checks both cases. The original design statement had the relationship the other way round ("an exact divisor"). A GCD of differences can only produce multiples of the true tick, never proper divisors.

## Limitations

- **Float artefacts force a fallback for the whole block.** One `101.30000000000001` makes the block Gorilla-encoded (see artefacts+nan-gaps: 0.05% artefacts and a few NaN gaps knocked out 16% of blocks and cut the ratio from 7.8x to 4.0x). An exception list, with a few raw values patched into a grid block, would fix this. It is not implemented, and the reserved block tags `10`/`11` exist to leave room for it.
- **Scales above 12 are not searched**, and a block whose mantissas or mantissa differences exceed i64 falls back. Prices quoted to more than 12 decimals always use Gorilla.
- **Tick deltas are fixed-width, not entropy-coded.** A walk with mostly ±1 moves and rare large jumps pays the width of the largest jump for every move in the block.
- **Single-threaded, whole-column API.** There is no streaming encoder, no random access inside a column, and no container format that ties the three columns together.
- **Decoding is safe but not hardened against hostile sizes.** Corrupt or truncated input returns an error and never panics (tested). However, a header can claim many values that cost almost no bits, for example constant grid blocks, so decoding untrusted input can allocate a lot of memory.
- **Timestamps and sizes are simple.** The timestamp codec is plain Gorilla delta-of-delta, which is poor for jittery nanosecond clocks (37 bits/value in the benchmark). The sizes codec is byte-aligned varints. Neither is compared against another baseline.
- **Synthetic data only.** The scenarios are generated random walks, not recorded market data. The results depend on the chosen move probabilities.

## License

MIT. See [LICENSE](LICENSE).
