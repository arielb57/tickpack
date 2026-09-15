use tickpack::price::{
    self, detect_scale, encode_with, shortest_decimals, BlockKind, FallbackReason, Mode,
};
use tickpack::{gorilla, DecodeError};

fn bits_of(v: &[f64]) -> Vec<u64> {
    v.iter().map(|x| x.to_bits()).collect()
}

fn assert_roundtrip(values: &[f64], block: usize) -> Vec<price::BlockInfo> {
    let (enc, infos) = encode_with(values, block);
    let dec = price::decode(&enc).expect("decode");
    assert_eq!(bits_of(&dec), bits_of(values), "round-trip differs");
    let total: u64 = infos.iter().map(|b| b.bits).sum();
    let header = {
        let mut h = Vec::new();
        tickpack::bits::put_varint(&mut h, values.len() as u64);
        tickpack::bits::put_varint(&mut h, block as u64);
        h.len() as u64
    };
    assert_eq!(
        enc.len() as u64,
        header + total.div_ceil(8),
        "block bit accounting"
    );
    infos
}

fn grid(infos: &[price::BlockInfo], i: usize) -> (u32, u64, Mode) {
    match infos[i].kind {
        BlockKind::Grid { scale, tick, mode } => (scale, tick, mode),
        other => panic!("block {i} took {other:?}"),
    }
}

fn fallback(infos: &[price::BlockInfo], i: usize) -> FallbackReason {
    match infos[i].kind {
        BlockKind::Gorilla { reason } => reason,
        other => panic!("block {i} took {other:?}"),
    }
}

#[test]
fn single_value_block() {
    let infos = assert_roundtrip(&[101.3], 1024);
    assert_eq!(grid(&infos, 0).0, 1);
    assert_eq!(infos.len(), 1);

    let walk: Vec<f64> = (0..50).map(|i| (10_000 + i * 3) as f64 / 100.0).collect();
    let infos = assert_roundtrip(&walk, 1);
    assert_eq!(infos.len(), 50);
    assert!(infos.iter().all(|b| b.len == 1));
}

#[test]
fn empty_column() {
    let (enc, infos) = encode_with(&[], 1024);
    assert!(infos.is_empty());
    assert_eq!(price::decode(&enc).unwrap(), Vec::<f64>::new());
}

#[test]
fn all_equal_values_cost_only_the_header() {
    let v = vec![42.17; 1024];
    let infos = assert_roundtrip(&v, 1024);
    let (scale, tick, mode) = grid(&infos, 0);
    assert_eq!((scale, tick, mode), (2, 1, Mode::Packed));
    assert!(
        infos[0].bits < 64,
        "{} bits for a constant block",
        infos[0].bits
    );
}

#[test]
fn one_off_grid_value_forces_fallback_for_its_block_only() {
    let mut v: Vec<f64> = (0..3072)
        .map(|i| (10_130 + (i % 7) as i64) as f64 / 100.0)
        .collect();
    let clean = v[1500];
    v[1500] = 1013.0 * 0.1;
    assert_eq!(v[1500].to_bits(), 101.30000000000001f64.to_bits());
    assert_ne!(v[1500].to_bits(), clean.to_bits());
    let infos = assert_roundtrip(&v, 1024);
    assert_eq!(grid(&infos, 0).0, 2);
    assert_eq!(fallback(&infos, 1), FallbackReason::NoDecimalScale);
    assert_eq!(grid(&infos, 2).0, 2);
}

#[test]
fn precision_change_at_block_boundary() {
    let mut v: Vec<f64> = (0..1024)
        .map(|i| (25_000 + 5 * (i % 11)) as f64 / 100.0)
        .collect();
    v.extend((0..1024).map(|i| (250_000 + (i % 13)) as f64 / 1000.0));
    let infos = assert_roundtrip(&v, 1024);
    assert_eq!(grid(&infos, 0).0, 2);
    assert_eq!(grid(&infos, 0).1, 5);
    assert_eq!(grid(&infos, 1).0, 3);
    assert_eq!(grid(&infos, 1).1, 1);
}

#[test]
fn precision_change_inside_a_block_uses_the_finer_scale() {
    let mut v: Vec<f64> = (0..500).map(|i| (1_000 + (i % 3)) as f64 / 10.0).collect();
    v.extend((0..524).map(|i| (100_000 + 7 * (i % 3)) as f64 / 1000.0));
    let infos = assert_roundtrip(&v, 1024);
    assert_eq!(grid(&infos, 0).0, 3);
}

#[test]
fn scale_twelve_at_the_i64_edge() {
    // 9e6 at scale 12 is a mantissa of 9e18, just under i64::MAX (9.22e18).
    let fits = [1e-12, 2e-12, 5e-12, 9_000_000.0, 1e-12, 3e-12, 4e-12, 2e-12];
    assert_eq!(detect_scale(&fits), Some(12));
    let infos = assert_roundtrip(&fits, 1024);
    assert!(matches!(
        infos[0].kind,
        BlockKind::Grid { scale: 12, .. }
            | BlockKind::Gorilla {
                reason: FallbackReason::Smaller
            }
    ));
    assert_eq!(
        price::exact_mantissa(9_000_000.0, 12),
        Some(9_000_000_000_000_000_000)
    );

    // 9.3e6 would need a mantissa of 9.3e18, which does not fit.
    let overflows = [1e-12, 9_300_000.0];
    assert_eq!(price::exact_mantissa(9_300_000.0, 12), None);
    assert_eq!(detect_scale(&overflows), None);
    let infos = assert_roundtrip(&overflows, 1024);
    assert_eq!(fallback(&infos, 0), FallbackReason::NoDecimalScale);
}

#[test]
fn mantissa_difference_overflow_falls_back() {
    let v = [-9e18, 9e18, -9e18];
    assert_eq!(detect_scale(&v), Some(0));
    let infos = assert_roundtrip(&v, 1024);
    assert_eq!(fallback(&infos, 0), FallbackReason::DeltaOverflow);
}

#[test]
fn largest_i64_representable_doubles() {
    let below = 9_223_372_036_854_774_784.0; // largest double under 2^63
    assert_eq!(
        price::exact_mantissa(below, 0),
        Some(9_223_372_036_854_774_784)
    );
    assert_eq!(price::exact_mantissa(9_223_372_036_854_775_808.0, 0), None);
    assert_eq!(
        price::exact_mantissa(-9_223_372_036_854_775_808.0, 0),
        Some(i64::MIN)
    );
    assert_roundtrip(&[below, -9_223_372_036_854_775_808.0, below], 2);
}

#[test]
fn special_values_never_take_the_grid_path() {
    let specials = [
        f64::NAN,
        -f64::NAN,
        f64::from_bits(0x7ff0_0000_0000_0001),
        -0.0,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::MAX,
        f64::MIN,
        f64::MIN_POSITIVE / 7.0,
        5e-324,
    ];
    for s in specials {
        assert_eq!(detect_scale(&[s]), None, "{s:?} ({:#x})", s.to_bits());
        let v = [1.5, 2.5, s, 3.5];
        let infos = assert_roundtrip(&v, 4);
        assert_eq!(fallback(&infos, 0), FallbackReason::NoDecimalScale);
    }
    assert_eq!(detect_scale(&[0.0]), Some(0));
    assert_eq!(detect_scale(&[f64::MIN_POSITIVE]), None);
}

#[test]
fn each_payload_mode_is_selected_and_roundtrips() {
    // Every price changes by a small amount: packed.
    let packed: Vec<f64> = (0..1024)
        .map(|i| (10_000 + [0, 3, 1, 2][i % 4]) as f64 / 100.0)
        .collect();
    // About half the prices are unchanged, moves are wide: zero-flag.
    let mut m = 10_000i64;
    let flagged: Vec<f64> = (0..1024u64)
        .map(|i| {
            let h = i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58;
            if h % 2 == 0 {
                m += (h as i64 % 13) - 6;
            }
            m as f64 / 100.0
        })
        .collect();
    // Long flat stretches: zero runs.
    let runs: Vec<f64> = (0..1024)
        .map(|i| (10_000 + (i / 200) * 3) as f64 / 100.0)
        .collect();
    for (v, want) in [
        (packed, Mode::Packed),
        (flagged, Mode::ZeroFlag),
        (runs, Mode::Runs),
    ] {
        let infos = assert_roundtrip(&v, 1024);
        assert_eq!(grid(&infos, 0).2, want);
    }
}

#[test]
fn tick_is_the_gcd_of_moves() {
    let v: Vec<f64> = [450_025i64, 450_050, 450_000, 450_100, 450_075]
        .iter()
        .map(|&m| m as f64 / 100.0)
        .collect();
    let infos = assert_roundtrip(&v, 1024);
    assert_eq!(grid(&infos, 0), (2, 25, Mode::Packed));
}

#[test]
fn shortest_formatting_agrees_with_detected_scale() {
    for (v, k) in [
        (101.3, 1),
        (101.29, 2),
        (1.08451, 5),
        (4500.0, 0),
        (1e-12, 12),
        (0.1 + 0.2, 17),
    ] {
        assert_eq!(shortest_decimals(v), Some(k), "{v}");
        let detected = detect_scale(&[v]);
        assert_eq!(detected, (k <= 12).then_some(k), "{v}");
    }
}

#[test]
fn never_worse_than_gorilla_plus_two_bits_per_block() {
    let noisy: Vec<f64> = (0..10_000u64)
        .map(|i| 100.0 + (i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 11) as f64 / (1u64 << 53) as f64)
        .collect();
    let (enc, infos) = encode_with(&noisy, 1024);
    let baseline = gorilla::encode_f64(&noisy);
    assert!(infos
        .iter()
        .all(|b| matches!(b.kind, BlockKind::Gorilla { .. })));
    let blocks = infos.len() as u64;
    // Extra bytes: the varint block size (2 bytes) plus 2 bits per block.
    assert!((enc.len() as u64) * 8 <= baseline.len() as u64 * 8 + 2 * blocks + 16);
}

#[test]
fn corrupt_streams_are_errors_not_panics() {
    let v: Vec<f64> = (0..3000)
        .map(|i| (10_000 + (i * 7919) % 97) as f64 / 100.0)
        .collect();
    let mut mixed = v.clone();
    mixed[2500] = f64::NAN;
    for data in [v, mixed] {
        let enc = price::encode(&data);
        for cut in 0..enc.len() {
            assert!(price::decode(&enc[..cut]).is_err(), "cut {cut}");
        }
        // Flipping bits must never panic; it may or may not be detected.
        for i in 0..enc.len().min(400) {
            let mut bad = enc.clone();
            bad[i] ^= 0x5a;
            let _ = price::decode(&bad);
        }
    }
    // Header says one value in blocks of 1024, then tag 11.
    assert_eq!(
        price::decode(&[1, 0x80, 0x08, 0b1100_0000]),
        Err(DecodeError::Corrupt("reserved block tag"))
    );
    assert_eq!(
        price::decode(&[1, 0]),
        Err(DecodeError::Corrupt("zero block size"))
    );
}
