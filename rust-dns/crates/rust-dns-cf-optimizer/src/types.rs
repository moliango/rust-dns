// SPDX-License-Identifier: GPL-3.0-only
//! Public data types shared by the CLI, the pipeline, and the LuCI consumer.
//!
//! The JSON field names follow `.trellis/tasks/11-23-cloudflare-ip-optimizer/design.md`
//! §2 and must not change without updating the LuCI/rpcd contract.

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Which address family the run targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    V4,
    V6,
}

impl Family {
    pub fn as_u8(self) -> u8 {
        match self {
            Family::V4 => 4,
            Family::V6 => 6,
        }
    }

    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            4 => Some(Family::V4),
            6 => Some(Family::V6),
            _ => None,
        }
    }

    /// Whether `ip` belongs to this family.
    pub fn matches(self, ip: &IpAddr) -> bool {
        matches!(
            (self, ip),
            (Family::V4, IpAddr::V4(_)) | (Family::V6, IpAddr::V6(_))
        )
    }
}

/// Where the candidate addresses come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpSourceKind {
    /// Official Cloudflare ranges (fetched through a [`crate::ip_source::RangeFetcher`]).
    Cloudflare,
    /// A user supplied file containing IPs and/or CIDRs.
    File,
    /// Deterministic built-in list used by `--dry-run`.
    Fixture,
}

/// How probe traffic is steered around OpenClash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BypassMode {
    /// No bypass; traffic follows the system routing table.
    None,
    /// Set `SO_MARK` and install a temporary fwmark routing rule.
    Mark,
    /// Bind probe sockets to a device (`SO_BINDTODEVICE`).
    Device,
}

impl BypassMode {
    pub fn as_str(self) -> &'static str {
        match self {
            BypassMode::None => "none",
            BypassMode::Mark => "mark",
            BypassMode::Device => "device",
        }
    }
}

/// Everything the pipeline needs to execute a run.
#[derive(Debug, Clone, PartialEq)]
pub struct OptimizerConfig {
    pub source: IpSourceKind,
    /// Required when `source == File`.
    pub ip_file: Option<PathBuf>,
    pub family: Family,
    /// TCP port used by the latency probe and as the default URL port.
    pub port: u16,
    /// Maximum number of in-flight latency probes.
    pub threads: usize,
    /// Number of TCP handshake samples per IP.
    pub ping: u32,
    /// Number of best-by-latency candidates that get a download test.
    pub download_count: usize,
    /// Per-download wall-clock budget, in seconds.
    pub download_time: u64,
    /// Per-download byte budget.
    pub download_bytes: u64,
    /// In-flight download probes.
    pub download_concurrency: usize,
    /// URL used for download tests.
    pub url: String,
    /// Candidate cap after CIDR expansion.
    pub max_candidates: usize,
    pub max_latency_ms: f64,
    pub max_loss_pct: f64,
    pub min_speed_mbps: f64,
    /// Per TCP connect timeout in milliseconds.
    pub timeout_ms: u64,
    /// Firewall mark (used when `bypass_mode == Mark`).
    pub mark: u32,
    /// Output interface, used for `SO_BINDTODEVICE` in device mode and as the
    /// device for the temporary direct route in mark mode.
    pub device: Option<String>,
    /// Optional source address bound on probe sockets.
    pub source_ip: Option<IpAddr>,
    /// Routing table used by the temporary fwmark rule.
    pub table: u32,
    pub bypass_mode: BypassMode,
    /// Explicitly stop OpenClash for the duration of the run (design §4.1
    /// `pause` fallback). Off by default: no service is touched otherwise.
    pub pause_openclash: bool,
    pub output: PathBuf,
    pub progress: PathBuf,
    pub cancel_file: PathBuf,
    pub dry_run: bool,
}

impl Default for OptimizerConfig {
    fn default() -> Self {
        Self {
            source: IpSourceKind::Fixture,
            ip_file: None,
            family: Family::V4,
            port: 443,
            threads: 50,
            ping: 4,
            download_count: 5,
            download_time: 10,
            download_bytes: 10_000_000,
            download_concurrency: 4,
            url: "https://speed.cloudflare.com/__down?bytes=10000000".to_string(),
            max_candidates: 1000,
            max_latency_ms: 300.0,
            max_loss_pct: 20.0,
            min_speed_mbps: 1.0,
            timeout_ms: 1000,
            mark: 0x40_0000,
            device: None,
            source_ip: None,
            table: 1688,
            bypass_mode: BypassMode::None,
            pause_openclash: false,
            output: PathBuf::from("/var/rust-dns/cf-optimizer/last.json"),
            progress: PathBuf::from("/var/rust-dns/cf-optimizer/progress.json"),
            cancel_file: PathBuf::from("/var/rust-dns/cf-optimizer/cancel"),
            dry_run: false,
        }
    }
}

/// Parameters echoed in the result document (`params` in design §2.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunParams {
    pub family: u8,
    pub port: u16,
    pub threads: usize,
    pub ping: u32,
    pub download_count: usize,
}

impl RunParams {
    pub fn from_config(config: &OptimizerConfig) -> Self {
        Self {
            family: config.family.as_u8(),
            port: config.port,
            threads: config.threads,
            ping: config.ping,
            download_count: config.download_count,
        }
    }
}

/// One tested address (`results[]` in design §2.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultRow {
    pub ip: String,
    pub family: u8,
    pub latency_min_ms: f64,
    pub latency_avg_ms: f64,
    pub loss_pct: f64,
    pub speed_mbps: f64,
    pub passed: bool,
}

impl ResultRow {
    pub fn new(ip: IpAddr, family: Family, stats: LatencyStats) -> Self {
        Self {
            ip: ip.to_string(),
            family: family.as_u8(),
            latency_min_ms: stats.min_ms.unwrap_or(0.0),
            latency_avg_ms: stats.avg_ms.unwrap_or(0.0),
            loss_pct: stats.loss_pct,
            speed_mbps: 0.0,
            passed: false,
        }
    }
}

/// Direct-connection summary (`bypass` in design §2.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BypassInfo {
    pub mode: String,
    pub mark: Option<String>,
    pub device: Option<String>,
    pub verified: bool,
    pub egress_ip: Option<String>,
    pub note: String,
}

impl BypassInfo {
    /// Bypass was explicitly disabled.
    pub fn disabled() -> Self {
        Self {
            mode: BypassMode::None.as_str().to_string(),
            mark: None,
            device: None,
            verified: false,
            egress_ip: None,
            note: "bypass disabled".to_string(),
        }
    }

    /// Deterministic value used by `--dry-run` (no network, no firewall).
    pub fn dry_run() -> Self {
        Self {
            mode: BypassMode::None.as_str().to_string(),
            mark: None,
            device: None,
            verified: true,
            egress_ip: Some("203.0.113.10".to_string()),
            note: "dry-run (simulated direct egress)".to_string(),
        }
    }
}

/// Complete result document (`last.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultReport {
    pub run_id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub params: RunParams,
    pub bypass: BypassInfo,
    pub results: Vec<ResultRow>,
}

/// Run state written to `progress.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProgressState {
    Running,
    Done,
    Cancelled,
    Error,
}

impl ProgressState {
    pub fn as_str(self) -> &'static str {
        match self {
            ProgressState::Running => "running",
            ProgressState::Done => "done",
            ProgressState::Cancelled => "cancelled",
            ProgressState::Error => "error",
        }
    }
}

/// Current pipeline phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProgressPhase {
    Latency,
    Download,
    Verify,
}

impl ProgressPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            ProgressPhase::Latency => "latency",
            ProgressPhase::Download => "download",
            ProgressPhase::Verify => "verify",
        }
    }
}

/// `progress.json` payload (design §2.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    pub state: ProgressState,
    pub phase: ProgressPhase,
    pub tested: u64,
    pub total: u64,
    pub message: String,
}

impl Progress {
    pub fn new(phase: ProgressPhase, total: u64, message: impl Into<String>) -> Self {
        Self {
            state: ProgressState::Running,
            phase,
            tested: 0,
            total,
            message: message.into(),
        }
    }
}

/// Aggregated latency samples for one address.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatencyStats {
    pub min_ms: Option<f64>,
    pub avg_ms: Option<f64>,
    pub loss_pct: f64,
}

impl LatencyStats {
    pub const ALL_LOST: Self = Self {
        min_ms: None,
        avg_ms: None,
        loss_pct: 100.0,
    };

    /// Whether this address clears the configured latency/loss thresholds.
    pub fn passes(&self, max_latency_ms: f64, max_loss_pct: f64) -> bool {
        self.avg_ms.is_some_and(|avg| avg <= max_latency_ms) && self.loss_pct <= max_loss_pct
    }
}

/// Compute min/average/loss from per-sample results.
///
/// Each `None` represents a timed-out or failed handshake.
pub fn summarize_latency(samples: &[Option<Duration>]) -> LatencyStats {
    if samples.is_empty() {
        return LatencyStats::ALL_LOST;
    }
    let mut min = f64::INFINITY;
    let mut sum = 0.0_f64;
    let mut ok = 0_usize;
    for sample in samples.iter().flatten() {
        let ms = duration_ms(*sample);
        min = min.min(ms);
        sum += ms;
        ok += 1;
    }
    if ok == 0 {
        return LatencyStats::ALL_LOST;
    }
    let avg = sum / ok as f64;
    let loss = (samples.len() - ok) as f64 / samples.len() as f64 * 100.0;
    LatencyStats {
        min_ms: Some(round2(min)),
        avg_ms: Some(round2(avg)),
        loss_pct: round2(loss),
    }
}

pub(crate) fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

pub(crate) fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Sort results in place: download speed descending, then lower average
/// latency, then the textual IP as a stable tie-breaker.
pub fn sort_results(results: &mut [ResultRow]) {
    results.sort_by(|a, b| {
        b.speed_mbps
            .partial_cmp(&a.speed_mbps)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                a.latency_avg_ms
                    .partial_cmp(&b.latency_avg_ms)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.ip.cmp(&b.ip))
    });
}

/// Outcome of a finished (or cancelled/failed-verification) run.
#[derive(Debug, Clone, PartialEq)]
pub struct RunOutcome {
    pub report: ResultReport,
    pub csv_path: PathBuf,
    pub exit_code: i32,
}

/// Exit code used when the bypass could not be verified (design §2.1).
pub const EXIT_BYPASS_UNVERIFIED: i32 = 3;
/// Exit code used when the run was cancelled.
pub const EXIT_CANCELLED: i32 = 2;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::time::Duration;

    fn ms(value: u64) -> Option<Duration> {
        Some(Duration::from_millis(value))
    }

    #[test]
    fn latency_min_average_and_loss_math() {
        let stats = summarize_latency(&[ms(10), ms(30), None, ms(50)]);
        assert_eq!(stats.min_ms, Some(10.0));
        assert_eq!(stats.avg_ms, Some(30.0));
        assert_eq!(stats.loss_pct, 25.0);
        assert!(stats.passes(30.0, 25.0));
        assert!(!stats.passes(29.9, 100.0));
        assert!(!stats.passes(100.0, 24.9));
    }

    #[test]
    fn all_failed_or_empty_samples_are_full_loss() {
        for stats in [summarize_latency(&[None, None]), summarize_latency(&[])] {
            assert_eq!(stats, LatencyStats::ALL_LOST);
            assert_eq!(stats.loss_pct, 100.0);
            assert!(!stats.passes(10_000.0, 100.0));
        }
    }

    #[test]
    fn latency_is_rounded_to_two_decimals() {
        let stats = summarize_latency(&[Some(Duration::from_micros(12_345))]);
        assert_eq!(stats.min_ms, Some(12.35));
        assert_eq!(stats.avg_ms, Some(12.35));
    }

    #[test]
    fn sorting_uses_speed_then_latency_then_ip() {
        let stats = summarize_latency(&[ms(10)]);
        let mut rows = vec![
            ResultRow {
                ip: "b".into(),
                speed_mbps: 5.0,
                latency_avg_ms: 10.0,
                ..row(stats)
            },
            ResultRow {
                ip: "a".into(),
                speed_mbps: 5.0,
                latency_avg_ms: 10.0,
                ..row(stats)
            },
            ResultRow {
                ip: "c".into(),
                speed_mbps: 5.0,
                latency_avg_ms: 1.0,
                ..row(stats)
            },
            ResultRow {
                ip: "d".into(),
                speed_mbps: 9.0,
                latency_avg_ms: 99.0,
                ..row(stats)
            },
        ];
        sort_results(&mut rows);
        let order: Vec<&str> = rows.iter().map(|r| r.ip.as_str()).collect();
        assert_eq!(order, vec!["d", "c", "a", "b"]);
    }

    fn row(stats: LatencyStats) -> ResultRow {
        ResultRow::new("104.16.0.1".parse().unwrap(), Family::V4, stats)
    }

    #[test]
    fn json_field_names_match_the_design_contract() {
        let report = ResultReport {
            run_id: "run".into(),
            started_at: "2026-11-23T12:00:00Z".into(),
            finished_at: Some("2026-11-23T12:03:10Z".into()),
            params: RunParams {
                family: 4,
                port: 443,
                threads: 50,
                ping: 4,
                download_count: 5,
            },
            bypass: BypassInfo {
                mode: "mark".into(),
                mark: Some("0x400000".into()),
                device: Some("wan".into()),
                verified: true,
                egress_ip: Some("203.0.113.10".into()),
                note: "direct".into(),
            },
            results: vec![row(summarize_latency(&[ms(10)]))],
        };
        let value = serde_json::to_value(&report).unwrap();
        let top: BTreeSet<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            top,
            BTreeSet::from([
                "run_id",
                "started_at",
                "finished_at",
                "params",
                "bypass",
                "results"
            ])
        );
        let params: BTreeSet<&str> = value["params"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            params,
            BTreeSet::from(["family", "port", "threads", "ping", "download_count"])
        );
        let row_keys: BTreeSet<&str> = value["results"][0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            row_keys,
            BTreeSet::from([
                "ip",
                "family",
                "latency_min_ms",
                "latency_avg_ms",
                "loss_pct",
                "speed_mbps",
                "passed"
            ])
        );
        let bypass_keys: BTreeSet<&str> = value["bypass"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            bypass_keys,
            BTreeSet::from(["mode", "mark", "device", "verified", "egress_ip", "note"])
        );

        let progress = Progress::new(ProgressPhase::Latency, 10, "x");
        let value = serde_json::to_value(progress).unwrap();
        let keys: BTreeSet<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            BTreeSet::from(["state", "phase", "tested", "total", "message"])
        );
        assert_eq!(value["state"], "running");
        assert_eq!(value["phase"], "latency");
    }
}
