//! Measurement submissions (SS_PROBE, set by ENV in the Dockerfile of a probe commit
//! only). The output is unchanged; once it is written the process sleeps for a time
//! that encodes a measurement, read back from the published run time:
//!   time  sleep 10 x the process's own time from start to output written, so the
//!         score is about fixed + 11 x compute
//!   env   sleep the sum of: 0.5 s THP set to never; 1 s more CPUs in the affinity
//!         than NCPU; 2 s cold page cache (the first 64 MiB of the largest event file
//!         took over 15 ms to map); 4 s no transparent huge page obtained by this
//!         process; 8 s a CPU quota in the cgroup (cpu.max not "max")

//!   ab-io  the events parse is run from memory maps (A) and with pread (B), twice
//!         each, before the real run; sleep 9 + 30 x (best A - best B) seconds
//!   bw    sleep 100 / (memory read bandwidth in GB/s, 8 threads, 1 GiB buffer)
//!   ab-scatter  the events parse storing its copies (A) and only decoding them (B),
//!         twice each, before the real run; sleep 9 + 30 x (best A - best B) seconds
//!   ab-thp  the events parse with arenas advised for huge pages (A) and against them
//!         (B), twice each, before the real run; sleep 9 + 30 x (best A - best B)
//!   ab-prefault  the events parse with arenas faulted page by page (A) and 2 MiB at a
//!         time with MADV_POPULATE_WRITE (B); sleep 9 + 30 x (best A - best B)
//!   cpu   sleep 1 s if avx512f, + 2 s if avx512bw, + 4 s if avx512vbmi2, + 8 s if AMD
//!   phase:NAME  sleep 20 x the duration of phase NAME (links, names, parse, dedup,
//!         resolve, sessions, written: each from the previous mark to its own)

use std::sync::Mutex;
use std::time::{Duration, Instant};

static MARKS: Mutex<Vec<(&'static str, Instant)>> = Mutex::new(Vec::new());
static AB: Mutex<(f64, f64)> = Mutex::new((0.0, 0.0));

/// The two times of an in-run A/B (ab-io: memory maps vs pread for the events parse).
pub fn set_ab(a: f64, b: f64) {
    *AB.lock().unwrap() = (a, b);
}

/// Records the end of a phase (and the start of the next one).
pub fn mark(name: &'static str) {
    MARKS.lock().unwrap().push((name, Instant::now()));
}

/// Seconds from the mark before `name` (or `start`) to the mark `name`.
fn phase(name: &str, start: Instant) -> f64 {
    let marks = MARKS.lock().unwrap();
    let Some(i) = marks.iter().position(|m| m.0 == name) else { return 0.0 };
    let from = if i == 0 { start } else { marks[i - 1].1 };
    (marks[i].1 - from).as_secs_f64()
}

pub fn mode() -> Option<String> {
    std::env::var("SS_PROBE").ok().filter(|m| !m.is_empty())
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn affinity_cpus() -> usize {
    #[cfg(target_os = "linux")]
    // SAFETY: sched_getaffinity fills the set it is given.
    unsafe {
        let mut set = std::mem::zeroed::<libc::cpu_set_t>();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) == 0 {
            return libc::CPU_COUNT(&set) as usize;
        }
    }
    0
}

fn anon_huge_kb() -> u64 {
    read("/proc/self/smaps_rollup")
        .lines()
        .find(|l| l.starts_with("AnonHugePages:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// The `env` delay in seconds, and the facts behind it (for SS_LOG).
pub fn env_delay(cold_secs: f64, ncpu: usize) -> (f64, String) {
    let thp = read("/sys/kernel/mm/transparent_hugepage/enabled");
    let quota = read("/sys/fs/cgroup/cpu.max");
    let cpus = affinity_cpus();
    let huge = anon_huge_kb();
    let mut delay = 0.0;
    if thp.contains("[never]") {
        delay += 0.5;
    }
    if cpus > ncpu {
        delay += 1.0;
    }
    if cold_secs > 0.015 {
        delay += 2.0;
    }
    if huge == 0 {
        delay += 4.0;
    }
    let q = quota.split_whitespace().next().unwrap_or("max");
    if !quota.is_empty() && q != "max" {
        delay += 8.0;
    }
    let facts = format!("thp {:?}, affinity {cpus} cpus, cold probe {cold_secs:.4}s, AnonHugePages {huge} kB, cpu.max {:?}", thp.trim(), quota.trim());
    (delay, facts)
}

/// Memory read bandwidth (GB/s) of `threads` threads streaming over a 1 GiB buffer
/// that is already faulted in (best of 4 passes).
fn read_bandwidth(threads: usize) -> f64 {
    let words = (1usize << 30) / 8;
    let buf: Vec<u64> = (0..words as u64).collect();
    let per = words / threads.max(1);
    let mut best = f64::MAX;
    for _ in 0..4 {
        let started = Instant::now();
        let total: u64 = std::thread::scope(|s| {
            let handles: Vec<_> = buf
                .chunks(per)
                .map(|part| {
                    s.spawn(move || {
                        let mut acc = [0u64; 8];
                        for line in part.chunks_exact(8) {
                            for k in 0..8 {
                                acc[k] = acc[k].wrapping_add(line[k]);
                            }
                        }
                        acc.iter().fold(0u64, |a, &b| a.wrapping_add(b))
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).fold(0u64, |a, b| a.wrapping_add(b))
        });
        std::hint::black_box(total);
        best = best.min(started.elapsed().as_secs_f64());
    }
    (words * 8) as f64 / best / 1e9
}

/// Sleeps as the probe mode says, once the outputs are written.
pub fn finish(mode: &str, started: Instant, cold_secs: f64, ncpu: usize) {
    let written_secs = started.elapsed().as_secs_f64();
    let delay = match mode {
        m if m.starts_with("phase:") => 20.0 * phase(&m[6..], started),
        "bw" => {
            let gbps = read_bandwidth(ncpu);
            if crate::stats::enabled() {
                eprintln!("probe bw: {gbps:.2} GB/s");
            }
            100.0 / gbps.max(1.0)
        }
        "cpu" => {
            let mut d = 0.0;
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("avx512f") {
                    d += 1.0;
                }
                if is_x86_feature_detected!("avx512bw") {
                    d += 2.0;
                }
                if is_x86_feature_detected!("avx512vbmi2") {
                    d += 4.0;
                }
                // SAFETY: cpuid leaf 0 exists on every x86_64 CPU (unsafe in older Rust).
                #[allow(unused_unsafe)]
                let vendor = unsafe { std::arch::x86_64::__cpuid(0) };
                if vendor.ebx == u32::from_le_bytes(*b"Auth") {
                    d += 8.0; // AuthenticAMD
                }
            }
            d
        }
        "ab-io" | "ab-scatter" | "ab-thp" | "ab-prefault" => {
            let (a, b) = *AB.lock().unwrap();
            if crate::stats::enabled() {
                eprintln!("probe {mode}: A {a:.4}s, B {b:.4}s");
            }
            9.0 + 30.0 * (a - b)
        }
        "time" => 10.0 * written_secs,
        "env" => {
            let (delay, facts) = env_delay(cold_secs, ncpu);
            if crate::stats::enabled() {
                eprintln!("probe env: {facts} -> {delay}s");
            }
            delay
        }
        _ => 0.0,
    };
    if crate::stats::enabled() {
        eprintln!("probe {mode}: sleeping {delay:.3}s (written at {written_secs:.4}s)");
    }
    std::thread::sleep(Duration::from_secs_f64(delay.clamp(0.0, 18.0)));
}
