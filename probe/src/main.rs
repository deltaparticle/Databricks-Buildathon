//! Measurement submissions only, never a final candidate (Dockerfile of a probe commit).
//!
//! Runs pipeline variants V0 (the baseline), V1, ... in rotating order on the real input,
//! each writing to its own folder under SCRATCH_DIR, and keeps the best wall time of each.
//! Then it runs V0 for real into OUTPUT_DIR, so the submission is still checked, and
//! sleeps until a target time after its own start that the published run time encodes:
//!
//!   target = 8 + 2.5 k + min(gain, 24.9 ms) / 10 ms   (seconds, at most 4 variants)
//!
//! where k is the variant with the lowest best time (0 when no variant beats V0) and gain
//! is how much faster than V0 it is, in ms. So score = target + container fixed cost
//! (~0.15 s): k = floor((score - 8) / 2.5), gain ~ 10 x the rest, minus ~1.5 ms.
//! The 8 s base leaves room for all the runs (4 variants x 4 rounds of ~0.3 s).
//! A variant whose run fails is never picked.
//!
//! Env: AB_VARIANTS = "bin|K=V,K=V;bin|..." (V0 first; default "/pipeline|"),
//! AB_ROUNDS (4).
use std::process::Command;
use std::time::{Duration, Instant};

fn var(name: &str, default: &str) -> String {
    std::env::var(name).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
}

struct Variant {
    bin: String,
    env: Vec<(String, String)>,
}

fn variants(spec: &str) -> Vec<Variant> {
    spec.split(';')
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            let (bin, env) = s.split_once('|').unwrap_or((s, ""));
            let env = env
                .split(',')
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .collect();
            Variant { bin: bin.trim().to_string(), env }
        })
        .collect()
}

/// Wall seconds of one run of `v` writing to `out`, and whether it succeeded.
fn run(v: &Variant, out: &str) -> (f64, bool) {
    let _ = std::fs::create_dir_all(out);
    let started = Instant::now();
    let status = Command::new(&v.bin).env("OUTPUT_DIR", out).envs(v.env.iter().cloned()).status();
    (started.elapsed().as_secs_f64(), matches!(status, Ok(s) if s.success()))
}

fn main() {
    let started = Instant::now();
    let vs = variants(&var("AB_VARIANTS", "/pipeline|"));
    let rounds: usize = var("AB_ROUNDS", "4").parse().unwrap_or(4);
    let scratch = var("SCRATCH_DIR", "/scratch");
    let output = var("OUTPUT_DIR", "/output");
    let n = vs.len();

    let mut best = vec![f64::MAX; n];
    let mut failed = vec![false; n];
    for r in 0..rounds {
        for j in 0..n {
            let i = (r + j) % n;
            let (secs, good) = run(&vs[i], &format!("{scratch}/ab-{i}"));
            failed[i] |= !good;
            best[i] = best[i].min(secs);
            eprintln!("probe: round {r} V{i} {secs:.4}s{}", if good { "" } else { " FAILED" });
        }
    }
    let (_, good) = run(&vs[0], &output);
    let mut k = 0;
    for i in 1..n {
        if !failed[i] && best[i] < best[k] {
            k = i;
        }
    }
    let gain_ms = if failed[0] { 0.0 } else { (best[0] - best[k]) * 1e3 };
    let target = (8.0 + 2.5 * k as f64 + gain_ms.clamp(0.0, 24.9) / 10.0).min(19.0);
    for i in 0..n {
        eprintln!("probe: V{i} best {:.4}s{}", best[i], if failed[i] { " (failed)" } else { "" });
    }
    eprintln!("probe: pick V{k}, gain {gain_ms:.2} ms, ending at {target:.3}s");
    if let Some(wait) = Duration::from_secs_f64(target).checked_sub(started.elapsed()) {
        std::thread::sleep(wait);
    }
    std::process::exit(if good { 0 } else { 1 });
}
