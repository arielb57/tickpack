use std::hint::black_box;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use tickpack::gen::{bench_scenarios, generate, Scenario};
use tickpack::price::{BlockKind, FallbackReason};
use tickpack::{gorilla, price, size, timestamp};

const USAGE: &str = "\
tickpack - lossless tick-data codec

USAGE:
    tickpack bench   [--n VALUES] [--block SIZE] [--repeat R]
    tickpack inspect <SCENARIO> [--n VALUES] [--block SIZE]
    tickpack help

bench    Encode generated price columns with tickpack and the in-repo Gorilla
         XOR baseline; print bits/value and encode/decode MB/s. Every run is
         checked for a bit-exact round-trip.
inspect  Show which path each block of one scenario took.

Defaults: --n 1000000 --block 1024 --repeat 5
Scenarios: equities-0.01 futures-0.25 fx-0.00001 precision-change
           adversarial-noise artefacts+nan-gaps";

struct Opts {
    n: usize,
    block: usize,
    repeat: usize,
    positional: Vec<String>,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        n: 1_000_000,
        block: price::DEFAULT_BLOCK_SIZE,
        repeat: 5,
        positional: Vec::new(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut num = |name: &str| -> Result<usize, String> {
            let v = it.next().ok_or(format!("{name} needs a value"))?;
            match v.parse::<usize>() {
                Ok(x) if x > 0 => Ok(x),
                _ => Err(format!("{name} must be a positive integer, got {v:?}")),
            }
        };
        match a.as_str() {
            "--n" => o.n = num("--n")?,
            "--block" => o.block = num("--block")?,
            "--repeat" => o.repeat = num("--repeat")?,
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            s => o.positional.push(s.to_string()),
        }
    }
    Ok(o)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let opts = match parse(&args[1..]) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match cmd.as_str() {
        "bench" => bench(&opts),
        "inspect" => inspect(&opts),
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown command {other:?}\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn best_of<T>(repeat: usize, mut f: impl FnMut() -> T) -> (T, Duration) {
    let mut best = Duration::MAX;
    let mut out = None;
    for _ in 0..repeat {
        let t = Instant::now();
        let r = black_box(f());
        best = best.min(t.elapsed());
        out = Some(r);
    }
    (out.expect("repeat is positive"), best)
}

fn mb_per_s(values: usize, d: Duration) -> f64 {
    (values * 8) as f64 / 1e6 / d.as_secs_f64().max(1e-9)
}

fn same_bits(a: &[f64], b: &[f64]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
}

fn bench(o: &Opts) -> Result<(), String> {
    println!(
        "prices: {} values per scenario, block size {}, best of {} runs",
        o.n, o.block, o.repeat
    );
    println!();
    println!("| scenario | seed | gorilla bits/value | tickpack bits/value | ratio | tickpack - gorilla, bits/block | grid blocks | gorilla enc MB/s | gorilla dec MB/s | tickpack enc MB/s | tickpack dec MB/s |");
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    let scenarios = bench_scenarios(o.n);
    for s in &scenarios {
        let data = generate(s);
        let p = &data.price;

        let (g_enc, g_enc_t) = best_of(o.repeat, || gorilla::encode_f64(p));
        let (g_dec, g_dec_t) = best_of(o.repeat, || gorilla::decode_f64(&g_enc));
        let ((t_enc, infos), t_enc_t) = best_of(o.repeat, || price::encode_with(p, o.block));
        let (t_dec, t_dec_t) = best_of(o.repeat, || price::decode(&t_enc));

        let g_dec = g_dec.map_err(|e| format!("{}: gorilla decode failed: {e}", s.name))?;
        let t_dec = t_dec.map_err(|e| format!("{}: tickpack decode failed: {e}", s.name))?;
        if !same_bits(p, &g_dec) || !same_bits(p, &t_dec) {
            return Err(format!("{}: round-trip is not bit-exact", s.name));
        }

        let g_bits = g_enc.len() as f64 * 8.0 / p.len() as f64;
        let t_bits = t_enc.len() as f64 * 8.0 / p.len() as f64;
        let grid = infos
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Grid { .. }))
            .count();
        println!(
            "| {} | {} | {:.2} | {:.2} | {:.2}x | {:+.2} | {}/{} | {:.0} | {:.0} | {:.0} | {:.0} |",
            s.name,
            s.seed,
            g_bits,
            t_bits,
            g_bits / t_bits,
            (t_enc.len() as f64 - g_enc.len() as f64) * 8.0 / infos.len() as f64,
            grid,
            infos.len(),
            mb_per_s(p.len(), g_enc_t),
            mb_per_s(p.len(), g_dec_t),
            mb_per_s(p.len(), t_enc_t),
            mb_per_s(p.len(), t_dec_t),
        );
    }

    println!();
    println!("other columns (tickpack codecs, scenario equities-0.01):");
    let data = generate(&scenarios[0]);
    let ts = timestamp::encode(&data.ts);
    let sz = size::encode_with(&data.size, o.block);
    if timestamp::decode(&ts).map_err(|e| e.to_string())? != data.ts
        || size::decode(&sz).map_err(|e| e.to_string())? != data.size
    {
        return Err("timestamp or size round-trip failed".into());
    }
    println!(
        "  timestamps (delta-of-delta):     {:.2} bits/value",
        ts.len() as f64 * 8.0 / data.ts.len() as f64
    );
    println!(
        "  sizes (frame of reference+varint): {:.2} bits/value",
        sz.len() as f64 * 8.0 / data.size.len() as f64
    );
    Ok(())
}

fn find_scenario(name: &str, n: usize) -> Result<Scenario, String> {
    bench_scenarios(n)
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| format!("unknown scenario {name:?}"))
}

fn inspect(o: &Opts) -> Result<(), String> {
    let name = o
        .positional
        .first()
        .ok_or("inspect needs a scenario name")?;
    let s = find_scenario(name, o.n)?;
    let data = generate(&s);
    let (enc, infos) = price::encode_with(&data.price, o.block);
    let dec = price::decode(&enc).map_err(|e| e.to_string())?;
    if !same_bits(&data.price, &dec) {
        return Err("round-trip is not bit-exact".into());
    }
    println!(
        "{}: seed {}, {} values, {} blocks, {:.2} bits/value, round-trip bit-exact",
        s.name,
        s.seed,
        data.price.len(),
        infos.len(),
        enc.len() as f64 * 8.0 / data.price.len() as f64
    );
    println!();
    println!(
        "{:>8} {:>6}  {:<8} {:>5} {:>8} {:<9} {:>10}",
        "start", "len", "path", "scale", "tick", "mode", "bits/value"
    );
    let show = |b: &price::BlockInfo| {
        let per = b.bits as f64 / b.len as f64;
        match b.kind {
            BlockKind::Grid { scale, tick, mode } => println!(
                "{:>8} {:>6}  {:<8} {:>5} {:>8} {:<9} {:>10.2}",
                b.start,
                b.len,
                "grid",
                scale,
                tick,
                format!("{mode:?}"),
                per
            ),
            BlockKind::Gorilla { reason } => {
                let why = match reason {
                    FallbackReason::NoDecimalScale => "no-scale",
                    FallbackReason::DeltaOverflow => "overflow",
                    FallbackReason::Smaller => "smaller",
                };
                println!(
                    "{:>8} {:>6}  {:<8} {:>5} {:>8} {:<9} {:>10.2}",
                    b.start, b.len, "gorilla", "-", "-", why, per
                )
            }
        }
    };
    const LIMIT: usize = 40;
    let mut last_kind = None;
    let mut shown = 0;
    for b in &infos {
        let key = match b.kind {
            BlockKind::Grid { scale, tick, .. } => (0, scale, tick),
            BlockKind::Gorilla { .. } => (1, 0, 0),
        };
        if last_kind != Some(key) && shown < LIMIT {
            show(b);
            shown += 1;
        }
        last_kind = Some(key);
    }
    println!();
    println!("(one row per change of path, scale or tick; at most {LIMIT} rows)");
    Ok(())
}
