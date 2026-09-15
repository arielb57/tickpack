//! Deterministic synthetic tick generator.
//!
//! Prices follow a random walk on a decimal tick grid. The walk runs on an
//! integer mantissa at the finest scale used by any segment, so a precision
//! change keeps the price level and only changes the grid it snaps to. Clean
//! prices are produced as `mantissa as f64 / 10^scale`, which is the double a
//! feed handler gets from parsing the decimal string.

/// SplitMix64: small, fast, and good enough for synthetic data.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }

    /// 1, 2, 3, ... with P(k) = (1 - p) p^(k - 1).
    pub fn geometric(&mut self, p: f64) -> i64 {
        let mut k = 1;
        while k < 1000 && self.chance(p) {
            k += 1;
        }
        k
    }
}

/// A tick grid in effect from `start` onwards.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grid {
    pub start: usize,
    /// Decimal places of the quoted price.
    pub scale: u32,
    /// Tick size in units of 10^-scale (25 at scale 2 is a 0.25 tick).
    pub tick: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scenario {
    pub name: &'static str,
    pub seed: u64,
    pub n: usize,
    pub start_price: f64,
    /// Grids sorted by `start`; the first must start at 0.
    pub grids: Vec<Grid>,
    /// Probability that a tick moves the price.
    pub p_move: f64,
    /// Continuation probability of the geometric move size, in ticks.
    pub p_step: f64,
    /// Probability per tick that a halt (flat price, then a time gap) begins.
    pub p_halt: f64,
    pub halt_len: usize,
    /// Fraction of prices recomputed as `mantissa as f64 * 10^-scale`, which
    /// yields artefacts such as 101.30000000000001.
    pub artefact_rate: f64,
    /// Probability per tick that a gap of NaN prices begins.
    pub p_nan_gap: f64,
    pub nan_gap_len: usize,
    /// Uniform off-grid noise added to every price, as a fraction of a tick.
    pub noise: f64,
}

impl Scenario {
    /// A plain random walk on one grid with no impairments.
    pub fn walk(
        name: &'static str,
        seed: u64,
        n: usize,
        start_price: f64,
        scale: u32,
        tick: i64,
    ) -> Self {
        Scenario {
            name,
            seed,
            n,
            start_price,
            grids: vec![Grid {
                start: 0,
                scale,
                tick,
            }],
            p_move: 0.4,
            p_step: 0.3,
            p_halt: 0.0,
            halt_len: 0,
            artefact_rate: 0.0,
            p_nan_gap: 0.0,
            nan_gap_len: 0,
            noise: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ticks {
    /// Nanoseconds since the Unix epoch.
    pub ts: Vec<i64>,
    pub price: Vec<f64>,
    pub size: Vec<i64>,
}

/// The six scenarios used by `tickpack bench`, with their seeds.
pub fn bench_scenarios(n: usize) -> Vec<Scenario> {
    let equities = Scenario {
        p_halt: 0.0002,
        halt_len: 400,
        ..Scenario::walk("equities-0.01", 1, n, 101.30, 2, 1)
    };
    let futures = Scenario {
        p_move: 0.3,
        p_step: 0.2,
        ..Scenario::walk("futures-0.25", 2, n, 4500.25, 2, 25)
    };
    let fx = Scenario {
        p_move: 0.8,
        p_step: 0.6,
        ..Scenario::walk("fx-0.00001", 3, n, 1.08450, 5, 1)
    };
    let mut precision = Scenario::walk("precision-change", 4, n, 250.00, 2, 5);
    precision.grids.push(Grid {
        start: n / 2 + 377,
        scale: 3,
        tick: 1,
    });
    let adversarial = Scenario {
        noise: 0.5,
        ..Scenario::walk("adversarial-noise", 5, n, 101.30, 2, 1)
    };
    let dirty = Scenario {
        artefact_rate: 0.0005,
        p_nan_gap: 0.0001,
        nan_gap_len: 20,
        ..Scenario::walk("artefacts+nan-gaps", 6, n, 101.30, 2, 1)
    };
    vec![equities, futures, fx, precision, adversarial, dirty]
}

/// Generate the scenario. Identical scenarios give identical output.
///
/// # Panics
/// If `grids` is empty, does not start at 0, or uses a scale above 12.
pub fn generate(s: &Scenario) -> Ticks {
    assert!(
        !s.grids.is_empty() && s.grids[0].start == 0,
        "first grid must start at 0"
    );
    let fine = s.grids.iter().map(|g| g.scale).max().expect("non-empty");
    assert!(fine <= 12, "scale above 12");
    let unit = |g: &Grid| g.tick * 10i64.pow(fine - g.scale);

    let mut rng = Rng::new(s.seed);
    let mut ts = Vec::with_capacity(s.n);
    let mut price = Vec::with_capacity(s.n);
    let mut size = Vec::with_capacity(s.n);

    let mut t: i64 = 1_767_000_000_000_000_000;
    let mut grid_idx = 0;
    let mut u = unit(&s.grids[0]);
    let mut level = ((s.start_price * 10f64.powi(fine as i32)).round() as i64) / u * u;
    let mut halt_left = 0usize;
    let mut nan_left = 0usize;

    for i in 0..s.n {
        while grid_idx + 1 < s.grids.len() && s.grids[grid_idx + 1].start <= i {
            grid_idx += 1;
            u = unit(&s.grids[grid_idx]);
            level = level / u * u;
        }
        let g = s.grids[grid_idx];

        if halt_left == 0 && s.p_halt > 0.0 && rng.chance(s.p_halt) {
            halt_left = s.halt_len;
            t += 60_000_000_000 * (1 + (rng.next_u64() % 30) as i64);
        }
        if halt_left > 0 {
            halt_left -= 1;
        } else if rng.chance(s.p_move) {
            let step = rng.geometric(s.p_step) * u;
            level = if rng.chance(0.5) || level - step <= u {
                level + step
            } else {
                level - step
            };
        }

        // Exponential inter-arrival, mean 2 ms, microsecond exchange clock.
        let gap_us = (-(1.0 - rng.unit()).ln() * 2000.0) as i64;
        t += gap_us * 1000;
        ts.push(t);

        let lots = if rng.chance(0.8) {
            100 * rng.geometric(0.5)
        } else {
            1 + (rng.next_u64() % 99) as i64
        };
        size.push(lots);

        if nan_left == 0 && s.p_nan_gap > 0.0 && rng.chance(s.p_nan_gap) {
            nan_left = s.nan_gap_len;
        }
        if nan_left > 0 {
            nan_left -= 1;
            price.push(f64::NAN);
            continue;
        }

        let mant = level / 10i64.pow(fine - g.scale);
        let mut p = if s.artefact_rate > 0.0 && rng.chance(s.artefact_rate) {
            mant as f64 * 10f64.powi(-(g.scale as i32))
        } else {
            mant as f64 / 10f64.powi(g.scale as i32)
        };
        if s.noise > 0.0 {
            let tick_value = g.tick as f64 / 10f64.powi(g.scale as i32);
            p += (rng.unit() - 0.5) * s.noise * 2.0 * tick_value;
        }
        price.push(p);
    }
    Ticks { ts, price, size }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_per_seed() {
        let s = Scenario::walk("x", 42, 5000, 10.0, 2, 1);
        assert_eq!(generate(&s), generate(&s));
        let other = Scenario {
            seed: 43,
            ..s.clone()
        };
        assert_ne!(generate(&s).price, generate(&other).price);
    }

    #[test]
    fn clean_walk_stays_on_grid_and_timestamps_increase() {
        let s = Scenario::walk("futures", 7, 20_000, 4500.25, 2, 25);
        let t = generate(&s);
        for &p in &t.price {
            let m = (p * 100.0).round() as i64;
            assert_eq!(m % 25, 0, "{p} off the 0.25 grid");
            assert_eq!((m as f64 / 100.0).to_bits(), p.to_bits());
            assert!(p > 0.0);
        }
        assert!(t.ts.windows(2).all(|w| w[1] >= w[0]));
        assert!(t.size.iter().all(|&s| s > 0));
    }

    #[test]
    fn precision_change_switches_grid_at_the_given_index() {
        let mut s = Scenario::walk("pc", 9, 4000, 50.0, 1, 1);
        s.p_move = 0.9;
        s.grids.push(Grid {
            start: 2000,
            scale: 3,
            tick: 1,
        });
        let t = generate(&s);
        let decimals = |p: f64| crate::price::shortest_decimals(p).unwrap();
        assert!(t.price[..2000].iter().all(|&p| decimals(p) <= 1));
        assert!(t.price[2000..].iter().any(|&p| decimals(p) == 3));
    }

    #[test]
    fn impairments_appear() {
        let s = Scenario {
            artefact_rate: 0.05,
            p_nan_gap: 0.001,
            nan_gap_len: 10,
            p_halt: 0.001,
            halt_len: 50,
            ..Scenario::walk("dirty", 3, 50_000, 101.30, 2, 1)
        };
        let t = generate(&s);
        assert!(t.price.iter().any(|p| p.is_nan()));
        let artefacts = t
            .price
            .iter()
            .filter(|p| !p.is_nan() && crate::price::shortest_decimals(**p).unwrap() > 2)
            .count();
        assert!(artefacts > 100, "only {artefacts} artefacts");
        let longest_flat = t
            .price
            .windows(2)
            .fold((0, 0), |(best, cur), w| {
                let c = if w[0].to_bits() == w[1].to_bits() {
                    cur + 1
                } else {
                    0
                };
                (best.max(c), c)
            })
            .0;
        assert!(longest_flat >= 49);
    }
}
