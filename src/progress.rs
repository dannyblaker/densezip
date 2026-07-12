//! Optional progress bar with ETA (`--progress`).
//!
//! Work is counted in weighted byte-units: each backend contributes its
//! input length scaled by a per-byte cost weight so that one unit is worth
//! roughly the same wall time everywhere. dzcm dominates real time (~0.2
//! MB/s), so it gets the reference weight and reports incrementally from
//! its bit loops; the fast backends report their whole input on completion.
//! Workers always tick the counters (nanosecond-cheap atomics); `start()`
//! only controls whether the renderer thread draws.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// dzcm compress or decompress, per byte (~0.2 MB/s).
pub const W_CM: u64 = 1024;
/// One LZMA parameter pass, per byte (three passes race per stream).
pub const W_LZMA: u64 = 128;
pub const W_BROTLI: u64 = 96;
pub const W_ZSTD: u64 = 48;
/// Decompression by any non-CM backend, per byte.
pub const W_FAST_D: u64 = 8;
/// L1 planning (scan + preflate/lepton + render-verify), per input byte.
pub const W_PLAN: u64 = 64;

static DONE: AtomicU64 = AtomicU64::new(0);
static TOTAL: AtomicU64 = AtomicU64::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static PHASE: Mutex<&'static str> = Mutex::new("");
static HANDLE: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

pub fn add(units: u64) {
    DONE.fetch_add(units, Relaxed);
}

pub fn add_total(units: u64) {
    TOTAL.fetch_add(units, Relaxed);
}

pub fn set_phase(phase: &'static str) {
    *PHASE.lock().unwrap() = phase;
}

/// Cost of racing all backends over one stream (compress + round-trip
/// verify), per the instrumentation in `backends`/`cm`.
pub fn race_units(len: u64, use_cm: bool) -> u64 {
    let per_byte =
        W_ZSTD + W_BROTLI + 3 * W_LZMA + 3 * W_FAST_D + if use_cm { 2 * W_CM } else { 0 };
    len * per_byte
}

/// Cost of decompressing one stream with a known backend.
pub fn decompress_units(is_cm: bool, raw_len: u64) -> u64 {
    raw_len * if is_cm { W_CM } else { W_FAST_D }
}

/// Print a line to stderr without garbling an active progress bar.
pub fn println_above(msg: &str) {
    if ACTIVE.load(Relaxed) {
        eprint!("\r{:<90}\r", "");
    }
    eprintln!("{msg}");
}

pub fn start() {
    if ACTIVE.swap(true, Relaxed) {
        return;
    }
    DONE.store(0, Relaxed);
    TOTAL.store(0, Relaxed);
    let handle = std::thread::spawn(|| {
        let t0 = Instant::now();
        // (elapsed seconds, done units) samples over the last ~10 s, so the
        // ETA tracks the current backend mix rather than the overall average.
        let mut window: VecDeque<(f64, u64)> = VecDeque::new();
        while ACTIVE.load(Relaxed) {
            let elapsed = t0.elapsed();
            let done = DONE.load(Relaxed);
            let total = TOTAL.load(Relaxed);
            window.push_back((elapsed.as_secs_f64(), done));
            while window.len() > 50 {
                window.pop_front();
            }
            let eta = eta(&window, done, total);
            let phase = *PHASE.lock().unwrap();
            eprint!("\r{:<90}", render(phase, done, total, elapsed, eta));
            let _ = std::io::stderr().flush();
            std::thread::sleep(Duration::from_millis(200));
        }
        eprint!("\r{:<90}\r", "");
        let _ = std::io::stderr().flush();
    });
    *HANDLE.lock().unwrap() = Some(handle);
}

/// Stop and erase the bar. Idempotent; safe to call from anywhere.
pub fn finish() {
    if !ACTIVE.swap(false, Relaxed) {
        return;
    }
    if let Some(h) = HANDLE.lock().unwrap().take() {
        let _ = h.join();
    }
}

fn eta(window: &VecDeque<(f64, u64)>, done: u64, total: u64) -> Option<Duration> {
    let &(t0, d0) = window.front()?;
    let &(t1, d1) = window.back()?;
    if total <= done || d1 <= d0 || t1 - t0 < 1.0 {
        return None;
    }
    let rate = (d1 - d0) as f64 / (t1 - t0);
    Some(Duration::from_secs_f64((total - done) as f64 / rate))
}

fn render(phase: &str, done: u64, total: u64, elapsed: Duration, eta: Option<Duration>) -> String {
    const WIDTH: usize = 28;
    let frac = if total > 0 {
        (done as f64 / total as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = (frac * WIDTH as f64) as usize;
    format!(
        "{:<12} [{}{}] {:>5.1}%  elapsed {}  ETA {}",
        phase,
        "#".repeat(filled),
        "-".repeat(WIDTH - filled),
        frac * 100.0,
        fmt_dur(elapsed),
        eta.map_or_else(|| "--:--".to_string(), fmt_dur),
    )
}

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_formatting() {
        assert_eq!(fmt_dur(Duration::from_secs(0)), "0:00");
        assert_eq!(fmt_dur(Duration::from_secs(75)), "1:15");
        assert_eq!(fmt_dur(Duration::from_secs(3723)), "1:02:03");
    }

    #[test]
    fn render_line() {
        let line = render("compressing", 50, 100, Duration::from_secs(60), None);
        assert!(line.contains("50.0%"));
        assert!(line.contains("elapsed 1:00"));
        assert!(line.contains("ETA --:--"));
        // zero total must not divide by zero or overfill the bar
        let line = render("planning", 5, 0, Duration::from_secs(1), None);
        assert!(line.contains("0.0%"));
    }

    #[test]
    fn eta_from_window() {
        let mut w = VecDeque::new();
        w.push_back((0.0, 0u64));
        w.push_back((2.0, 100u64)); // 50 units/s, 100 remaining -> 2 s
        assert_eq!(eta(&w, 100, 200), Some(Duration::from_secs(2)));
        assert_eq!(eta(&w, 200, 200), None); // done
        w.clear();
        w.push_back((0.0, 0u64));
        w.push_back((0.5, 10u64)); // window too short
        assert_eq!(eta(&w, 10, 200), None);
    }
}
