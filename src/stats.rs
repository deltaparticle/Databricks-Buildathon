//! Phase timings on stderr (only with SS_LOG=1): wall time, and on Linux user/system
//! CPU time and minor page faults since the previous line.

use std::time::Instant;

#[derive(Clone, Copy, Default)]
struct Usage {
    user: f64,
    system: f64,
    faults: i64,
}

#[cfg(target_os = "linux")]
fn usage() -> Usage {
    let mut r = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage fills the struct; RUSAGE_SELF covers every thread.
    let r = unsafe {
        libc::getrusage(libc::RUSAGE_SELF, r.as_mut_ptr());
        r.assume_init()
    };
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 * 1e-6;
    Usage {
        user: secs(r.ru_utime),
        system: secs(r.ru_stime),
        faults: r.ru_minflt as i64,
    }
}

#[cfg(not(target_os = "linux"))]
fn usage() -> Usage {
    Usage::default()
}

/// True when SS_LOG=1: phase logs on stderr. Off by default (the scorer reads none).
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SS_LOG").as_deref() == Ok("1"))
}

pub struct Stats {
    clock: Instant,
    last_wall: f64,
    last: Usage,
}

impl Stats {
    pub fn new() -> Self {
        Self {
            clock: Instant::now(),
            last_wall: 0.0,
            last: usage(),
        }
    }

    pub fn log(&mut self, what: &str) {
        if !enabled() {
            return;
        }
        let wall = self.clock.elapsed().as_secs_f64();
        let now = usage();
        eprintln!(
            "[{wall:>7.3}s +{:>6.3}s user {:>6.3} sys {:>6.3} faults {:>8}] {what}",
            wall - self.last_wall,
            now.user - self.last.user,
            now.system - self.last.system,
            now.faults - self.last.faults
        );
        self.last_wall = wall;
        self.last = now;
    }
}
