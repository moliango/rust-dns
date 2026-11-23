// SPDX-License-Identifier: GPL-3.0-only
//! Clock abstraction and UTC timestamp formatting.
//!
//! The collector never calls `SystemTime` directly. Tests inject a
//! [`ManualClock`] so QPS, history buckets and log timestamps are deterministic.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Monotonic + wall-clock time source used by the collector.
pub trait Clock: Send + Sync {
    /// Current wall-clock time as Unix milliseconds.
    fn wall_millis(&self) -> i64;

    /// Seconds elapsed since some arbitrary monotonic origin.
    ///
    /// Only differences are meaningful; the value is used for QPS slots,
    /// history bucket rolling and uptime.
    fn monotonic_secs(&self) -> u64;
}

/// Production clock: `SystemTime` for wall clock, `Instant` for monotonic time.
#[derive(Debug)]
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    /// Create a clock whose monotonic origin is now.
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn wall_millis(&self) -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => duration.as_millis() as i64,
            Err(_) => 0,
        }
    }

    fn monotonic_secs(&self) -> u64 {
        self.start.elapsed().as_secs()
    }
}

/// Fully controlled clock for tests.
#[derive(Debug)]
pub struct ManualClock {
    wall_millis: AtomicI64,
    monotonic_secs: AtomicU64,
}

impl ManualClock {
    /// Create a clock frozen at `wall_secs` (Unix seconds) and `monotonic_secs`.
    pub fn new(wall_secs: i64, monotonic_secs: u64) -> Self {
        Self {
            wall_millis: AtomicI64::new(wall_secs.saturating_mul(1000)),
            monotonic_secs: AtomicU64::new(monotonic_secs),
        }
    }

    /// Advance both the wall and monotonic clocks by `secs` seconds.
    pub fn advance_secs(&self, secs: u64) {
        let millis = i64::try_from(secs).unwrap_or(i64::MAX).saturating_mul(1000);
        self.wall_millis.fetch_add(millis, Ordering::Relaxed);
        self.monotonic_secs.fetch_add(secs, Ordering::Relaxed);
    }

    /// Set the wall clock without touching the monotonic clock.
    pub fn set_wall_secs(&self, secs: i64) {
        self.wall_millis
            .store(secs.saturating_mul(1000), Ordering::Relaxed);
    }

    /// Set the monotonic clock without touching the wall clock.
    pub fn set_monotonic_secs(&self, secs: u64) {
        self.monotonic_secs.store(secs, Ordering::Relaxed);
    }
}

impl Clock for ManualClock {
    fn wall_millis(&self) -> i64 {
        self.wall_millis.load(Ordering::Relaxed)
    }

    fn monotonic_secs(&self) -> u64 {
        self.monotonic_secs.load(Ordering::Relaxed)
    }
}

/// Format Unix milliseconds as RFC 3339 UTC with millisecond precision.
///
/// Output always ends with `Z`, for example `2025-11-23T12:13:20.000Z`.
pub fn rfc3339_millis(wall_millis: i64) -> String {
    let secs = wall_millis.div_euclid(1000);
    let millis = wall_millis.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let second_of_day = secs.rem_euclid(86_400);

    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3600;
    let minute = (second_of_day % 3600) / 60;
    let second = second_of_day % 60;

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 -> (year, month, day).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_epoch() {
        assert_eq!(rfc3339_millis(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn formats_fixture_time() {
        assert_eq!(
            rfc3339_millis(1_763_900_000_123),
            "2025-11-23T12:13:20.123Z"
        );
    }

    #[test]
    fn formats_negative_time() {
        assert_eq!(rfc3339_millis(-1), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn manual_clock_advances() {
        let clock = ManualClock::new(100, 5);
        assert_eq!(clock.wall_millis(), 100_000);
        assert_eq!(clock.monotonic_secs(), 5);
        clock.advance_secs(3);
        assert_eq!(clock.wall_millis(), 103_000);
        assert_eq!(clock.monotonic_secs(), 8);
    }
}
