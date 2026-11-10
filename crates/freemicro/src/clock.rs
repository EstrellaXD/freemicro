//! Time helpers. There is no `Clock` trait: components take `now: f64`
//! explicitly, so tests pass literal times. Only the run loop, the CLI and
//! the hook entry point read the real clock, through these two functions.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Wall-clock seconds since the Unix epoch. Use for anything persisted
/// (`updated_at` in state files, `started` in `pad.lock`), as Python's
/// `time.time()`.
pub fn wall() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Monotonic seconds since the first call in this process. Use for intervals,
/// deadlines and debouncing, as Python's `time.monotonic()`.
pub fn mono() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}
