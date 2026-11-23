// SPDX-License-Identifier: GPL-3.0-only
//! Result/progress serialization, atomic file replacement, and cancellation.
//!
//! Writes are deliberately small and synchronous: a run emits at most one JSON
//! document, one CSV document, and one progress update per second, so blocking
//! the orchestration task briefly is cheaper than adding an async writer layer.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::types::{Progress, ProgressState, ResultReport, ResultRow};

/// CSV header required by design §2.2.
pub const CSV_HEADER: [&str; 7] = [
    "IP",
    "family",
    "latency_min_ms",
    "latency_avg_ms",
    "loss_pct",
    "speed_mbps",
    "passed",
];

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Replace `path` with `bytes` using a sibling temporary file and `rename(2)`.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("create output directory {}", parent.display()))?;

    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    let tmp = parent.join(format!(".{file_name}.tmp.{}.{seq}", std::process::id()));

    {
        let mut file = fs::File::create(&tmp)
            .with_context(|| format!("create temporary file {}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("write temporary file {}", tmp.display()))?;
        file.flush()?;
        // Best effort: the data must be durable before rename on power loss.
        let _ = file.sync_all();
    }

    fs::rename(&tmp, path).with_context(|| {
        let _ = fs::remove_file(&tmp);
        format!("rename {} to {}", tmp.display(), path.display())
    })?;
    Ok(())
}

/// Write the result JSON document atomically.
pub fn write_report(path: &Path, report: &ResultReport) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(report).context("serialize result report")?;
    bytes.push(b'\n');
    atomic_write(path, &bytes)
}

/// Serialize rows to the documented CSV schema.
pub fn results_to_csv(rows: &[ResultRow]) -> Result<Vec<u8>> {
    let mut writer = csv::WriterBuilder::new()
        .has_headers(false)
        .from_writer(Vec::new());
    writer.write_record(CSV_HEADER)?;
    for row in rows {
        writer.write_record([
            row.ip.as_str(),
            &row.family.to_string(),
            &format!("{:.2}", row.latency_min_ms),
            &format!("{:.2}", row.latency_avg_ms),
            &format!("{:.2}", row.loss_pct),
            &format!("{:.2}", row.speed_mbps),
            if row.passed { "true" } else { "false" },
        ])?;
    }
    Ok(writer.into_inner()?)
}

/// Result CSV path convention: `result.csv` next to the JSON document.
pub fn csv_path_for(output_json: &Path) -> PathBuf {
    output_json
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("result.csv")
}

/// Write the CSV next to `output_json` atomically.
pub fn write_results_csv(output_json: &Path, rows: &[ResultRow]) -> Result<PathBuf> {
    let path = csv_path_for(output_json);
    let bytes = results_to_csv(rows)?;
    atomic_write(&path, &bytes)?;
    Ok(path)
}

/// Throttled atomic writer for `progress.json`.
pub struct ProgressWriter {
    path: PathBuf,
    last_write: Option<Instant>,
    interval: std::time::Duration,
}

impl ProgressWriter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            last_write: None,
            interval: std::time::Duration::from_secs(1),
        }
    }

    /// Write unless throttled. `force` bypasses the one-second limit and is
    /// used for phase/state transitions.
    pub fn update(&mut self, progress: &Progress, force: bool) -> Result<()> {
        let now = Instant::now();
        let throttled = self
            .last_write
            .is_some_and(|last| now.duration_since(last) < self.interval);
        if throttled && !force {
            return Ok(());
        }
        let mut bytes = serde_json::to_vec_pretty(progress).context("serialize progress")?;
        bytes.push(b'\n');
        atomic_write(&self.path, &bytes)?;
        self.last_write = Some(now);
        Ok(())
    }

    /// Record a terminal state (done/cancelled/error).
    pub fn finish(&mut self, mut progress: Progress, state: ProgressState) -> Result<()> {
        progress.state = state;
        self.update(&progress, true)
    }
}

/// Filesystem-based cancellation flag (design §2.3).
#[derive(Debug, Clone)]
pub struct Cancellation {
    path: PathBuf,
}

impl Cancellation {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_cancelled(&self) -> bool {
        self.path.exists()
    }

    /// Remove a stale flag. The rpcd/CLI entry point normally does this before
    /// starting a run; the library deliberately does not, so callers can
    /// pre-create the file to abort deterministically in tests.
    pub fn clear(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("remove {}", self.path.display())),
        }
    }
}

/// Current wall clock.
pub fn now_utc() -> SystemTime {
    SystemTime::now()
}

/// RFC3339 UTC timestamp with second precision (`2026-11-23T12:00:00Z`).
pub fn format_rfc3339(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, month, day, hour, minute, second) = civil_from_unix(seconds);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Compact identifier (`20261123-120000-1a2b3c4d`) for one run.
pub fn new_run_id() -> String {
    let now = now_utc();
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let nanos = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let (year, month, day, hour, minute, second) = civil_from_unix(seconds);
    format!(
        "{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}-{:08x}{:04x}",
        std::process::id(),
        (nanos >> 16) & 0xffff
    )
}

/// Convert seconds since the Unix epoch to civil UTC fields.
fn civil_from_unix(seconds: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (seconds / 86_400) as i64;
    let secs_of_day = (seconds % 86_400) as u32;
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    (year, month, day, hour, minute, second)
}

/// Howard Hinnant's `civil_from_days` (days relative to 1970-01-01).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ResultRow, summarize_latency};
    use std::net::IpAddr;
    use std::time::Duration;

    #[test]
    fn csv_matches_documented_header_and_order() {
        let mut row = ResultRow::new(
            "104.16.0.1".parse::<IpAddr>().unwrap(),
            crate::types::Family::V4,
            summarize_latency(&[Some(Duration::from_millis(10))]),
        );
        row.speed_mbps = 12.5;
        row.passed = true;
        let text = String::from_utf8(results_to_csv(&[row]).unwrap()).unwrap();
        let mut lines = text.lines();
        assert_eq!(
            lines.next().unwrap(),
            "IP,family,latency_min_ms,latency_avg_ms,loss_pct,speed_mbps,passed"
        );
        assert_eq!(
            lines.next().unwrap(),
            "104.16.0.1,4,10.00,10.00,0.00,12.50,true"
        );
    }

    #[test]
    fn atomic_write_replaces_existing_file() {
        let dir = tempdir();
        let path = dir.join("last.json");
        fs::write(&path, b"old").unwrap();
        atomic_write(&path, b"new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn timestamp_format_is_stable() {
        assert_eq!(format_rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            format_rfc3339(UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)),
            "2023-11-14T22:13:20Z"
        );
    }

    #[test]
    fn cancellation_is_file_presence() {
        let dir = tempdir();
        let path = dir.join("cancel");
        let cancel = Cancellation::new(&path);
        assert!(!cancel.is_cancelled());
        fs::write(&path, b"").unwrap();
        assert!(cancel.is_cancelled());
        cancel.clear().unwrap();
        assert!(!cancel.is_cancelled());
        let _ = fs::remove_dir_all(dir);
    }

    pub(crate) fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rust-dns-cf-test-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
