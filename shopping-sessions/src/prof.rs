//! Optional sub-phase timers (SS_PROFILE=1): CPU-side wall time summed over threads.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

pub const NAMES: [&str; 6] = ["dedup insert", "dedup kept", "dedup route", "sessions rank", "sessions sort", "sessions person"];
pub const DEDUP_INSERT: usize = 0;
pub const DEDUP_KEPT: usize = 1;
pub const DEDUP_ROUTE: usize = 2;
pub const SESS_RANK: usize = 3;
pub const SESS_SORT: usize = 4;
pub const SESS_PERSON: usize = 5;

static TOTAL: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SS_PROFILE").as_deref() == Ok("1"))
}

/// A timestamp when profiling, None otherwise.
#[inline]
pub fn start() -> Option<Instant> {
    enabled().then(Instant::now)
}

/// Adds the time since `t` to `slot`; returns a new timestamp.
#[inline]
pub fn lap(slot: usize, t: Option<Instant>) -> Option<Instant> {
    let t = t?;
    let now = Instant::now();
    TOTAL[slot].fetch_add((now - t).as_nanos() as u64, Relaxed);
    Some(now)
}

pub fn report() {
    if enabled() {
        for (name, total) in NAMES.iter().zip(&TOTAL) {
            eprintln!("profile {name:16} {:8.3}s (summed over threads)", total.load(Relaxed) as f64 * 1e-9);
        }
    }
}
