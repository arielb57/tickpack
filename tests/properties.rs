use proptest::prelude::*;
use proptest::test_runner::Config;
use tickpack::price::{self, detect_scale, encode_with, shortest_decimals, BlockKind};
use tickpack::{gorilla, size, timestamp};

fn config(cases: u32) -> Config {
    Config {
        cases,
        // A failing case must be reported within minutes, not shrink for hours.
        max_shrink_time: 60_000,
        max_shrink_iters: 2_000,
        ..Config::default()
    }
}

fn bits_of(v: &[f64]) -> Vec<u64> {
    v.iter().map(|x| x.to_bits()).collect()
}

fn awkward_f64() -> impl Strategy<Value = f64> {
    prop_oneof![
        // Raw bit patterns: NaN payloads, subnormals, infinities, anything.
        any::<u64>().prop_map(f64::from_bits),
        prop::sample::select(vec![
            f64::NAN,
            -f64::NAN,
            f64::from_bits(0x7ff8_0000_0000_0001),
            f64::from_bits(0xfff4_0000_dead_beef),
            -0.0,
            0.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MAX,
            f64::MIN,
            f64::MIN_POSITIVE,
            5e-324,
            101.30000000000001,
            9_223_372_036_854_775_808.0,
        ]),
        (-10_000_000i64..10_000_000, 0u32..=12).prop_map(|(m, k)| m as f64 / 10f64.powi(k as i32)),
        -1e9f64..1e9,
    ]
}

/// A column made of stretches that stay on one grid and stretches of awkward
/// values, so grid and Gorilla blocks interleave and share XOR state.
fn mixed_column() -> impl Strategy<Value = Vec<f64>> {
    let on_grid = (
        0u32..=6,
        1i64..500,
        -100_000i64..100_000,
        prop::collection::vec(-5i64..=5, 1..300),
    )
        .prop_map(|(k, tick, start, steps)| {
            let mut m = start * tick;
            steps
                .into_iter()
                .map(|s| {
                    m += s * tick;
                    m as f64 / 10f64.powi(k as i32)
                })
                .collect::<Vec<_>>()
        });
    let awkward = prop::collection::vec((awkward_f64(), 1usize..6), 1..60).prop_map(|v| {
        v.into_iter()
            .flat_map(|(x, r)| std::iter::repeat_n(x, r))
            .collect::<Vec<_>>()
    });
    prop::collection::vec(prop_oneof![on_grid, awkward], 0..8).prop_map(|parts| parts.concat())
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

proptest! {
    #![proptest_config(config(400))]

    #[test]
    fn arbitrary_f64_roundtrip_is_bit_exact(v in prop::collection::vec(awkward_f64(), 0..600), block in 1usize..80) {
        let (enc, _) = encode_with(&v, block);
        let dec = price::decode(&enc).unwrap();
        prop_assert_eq!(bits_of(&dec), bits_of(&v));
    }

    #[test]
    fn mixed_columns_roundtrip_and_stay_within_gorilla_bound(v in mixed_column(), block in prop::sample::select(vec![1usize, 7, 64, 256, 1024])) {
        let (enc, infos) = encode_with(&v, block);
        let dec = price::decode(&enc).unwrap();
        prop_assert_eq!(bits_of(&dec), bits_of(&v));

        let baseline_bits = gorilla::encode_f64(&v).len() as u64 * 8;
        // Header difference is the block-size varint (at most 2 bytes here),
        // plus byte padding; each block adds at most its 2-bit tag.
        prop_assert!(enc.len() as u64 * 8 <= baseline_bits + 2 * infos.len() as u64 + 16 + 8);
    }

    #[test]
    fn arbitrary_timestamps_roundtrip(ts in prop::collection::vec(any::<i64>(), 0..400)) {
        prop_assert_eq!(timestamp::decode(&timestamp::encode(&ts)).unwrap(), ts);
    }

    #[test]
    fn near_regular_timestamps_roundtrip(start in any::<i64>(), gaps in prop::collection::vec(-3000i64..1_000_000, 0..400)) {
        let mut t = start;
        let ts: Vec<i64> = gaps.iter().map(|g| { t = t.wrapping_add(*g); t }).collect();
        prop_assert_eq!(timestamp::decode(&timestamp::encode(&ts)).unwrap(), ts);
    }

    #[test]
    fn arbitrary_sizes_roundtrip(v in prop::collection::vec(any::<i64>(), 0..400), block in 1usize..100) {
        prop_assert_eq!(size::decode(&size::encode_with(&v, block)).unwrap(), v);
    }
}

proptest! {
    #![proptest_config(config(300))]

    /// On-grid series take the grid path; the detected scale matches Rust's
    /// shortest round-trip formatting; and the recovered tick, converted to
    /// the generating scale, is the generating tick times the GCD of the step
    /// counts. With a one-tick move present that GCD is 1, so the tick is
    /// recovered exactly.
    #[test]
    fn on_grid_series_recover_scale_and_tick(
        scale in 0u32..=8,
        tick in 1i64..1000,
        start in 1i64..1_000_000,
        mut steps in prop::collection::vec(-20i64..=20, 31..600),
        one_tick_at in any::<prop::sample::Index>(),
        force_one_tick in any::<bool>(),
    ) {
        if force_one_tick {
            let i = one_tick_at.index(steps.len());
            steps[i] = if steps[i] < 0 { -1 } else { 1 };
        }
        let mut m = start * tick;
        let mut mants = vec![m];
        for s in &steps {
            m += s * tick;
            mants.push(m);
        }
        let p = 10f64.powi(scale as i32);
        let values: Vec<f64> = mants.iter().map(|&m| m as f64 / p).collect();

        let (enc, infos) = encode_with(&values, values.len());
        prop_assert_eq!(bits_of(&price::decode(&enc).unwrap()), bits_of(&values));
        prop_assert_eq!(infos.len(), 1);
        let (k, recovered) = match infos[0].kind {
            BlockKind::Grid { scale, tick, .. } => (scale, tick),
            other => return Err(TestCaseError::fail(format!("fell back: {other:?}"))),
        };

        let oracle = values.iter().map(|&v| shortest_decimals(v).unwrap()).max().unwrap();
        prop_assert_eq!(Some(k), detect_scale(&values));
        prop_assert_eq!(k, oracle);
        prop_assert!(k <= scale);

        let step_gcd = steps.iter().fold(0u64, |g, s| gcd(g, s.unsigned_abs()));
        let at_generating_scale = recovered * 10u64.pow(scale - k);
        if step_gcd == 0 {
            prop_assert_eq!(recovered, 1);
        } else {
            prop_assert_eq!(at_generating_scale, tick as u64 * step_gcd);
            prop_assert_eq!(at_generating_scale % tick as u64, 0);
        }
        if force_one_tick {
            prop_assert_eq!(at_generating_scale, tick as u64);
        }
    }
}
