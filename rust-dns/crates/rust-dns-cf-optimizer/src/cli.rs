// SPDX-License-Identifier: GPL-3.0-only
//! Command-line surface for `rust-dns cf-optimize`.

use std::net::IpAddr;
use std::path::PathBuf;

use clap::{Parser, ValueEnum};

use crate::types::{BypassMode, Family, IpSourceKind, OptimizerConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SourceArg {
    Cloudflare,
    File,
    Fixture,
}

impl From<SourceArg> for IpSourceKind {
    fn from(value: SourceArg) -> Self {
        match value {
            SourceArg::Cloudflare => IpSourceKind::Cloudflare,
            SourceArg::File => IpSourceKind::File,
            SourceArg::Fixture => IpSourceKind::Fixture,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BypassArg {
    None,
    Mark,
    Device,
}

impl From<BypassArg> for BypassMode {
    fn from(value: BypassArg) -> Self {
        match value {
            BypassArg::None => BypassMode::None,
            BypassArg::Mark => BypassMode::Mark,
            BypassArg::Device => BypassMode::Device,
        }
    }
}

fn parse_family(value: &str) -> Result<u8, String> {
    match value {
        "4" => Ok(4),
        "6" => Ok(6),
        other => Err(format!("family must be 4 or 6, got {other:?}")),
    }
}

/// Parse a mark as decimal or `0x`-prefixed hexadecimal.
fn parse_mark(value: &str) -> Result<u32, String> {
    let trimmed = value.trim();
    let parsed = if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16)
    } else {
        trimmed.parse::<u32>()
    };
    parsed.map_err(|_| format!("invalid mark {value:?}; use decimal or 0x-prefixed hex"))
}

/// Map a clap parse failure to the documented `cf-optimize` exit code.
///
/// `--help` and `--version` are not failures: they print to stdout and exit 0.
/// Every other parse error exits 1 so that it never collides with the
/// "cancelled" exit code (2) from design §2.1.
pub fn exit_code_for_parse_error(error: &clap::Error) -> i32 {
    if error.use_stderr() { 1 } else { 0 }
}

/// `rust-dns cf-optimize` arguments. The core binary should call this and then
/// [`crate::pipeline::run`].
#[derive(Debug, Clone, Parser)]
#[command(
    name = "rust-dns-cf-optimize",
    about = "Select fast Cloudflare IPs with TCP latency and HTTPS download probes",
    version
)]
pub struct Args {
    /// Candidate source: official Cloudflare ranges, a file, or the built-in fixture.
    #[arg(long, value_enum, default_value = "cloudflare")]
    pub source: SourceArg,

    /// File with IPs/CIDRs (required when --source file).
    #[arg(long)]
    pub ip_file: Option<PathBuf>,

    /// Address family (4 or 6).
    #[arg(long, default_value = "4", value_parser = parse_family)]
    pub family: u8,

    /// TCP port used by the latency probe.
    #[arg(long, default_value_t = 443)]
    pub port: u16,

    /// Maximum concurrent latency probes.
    #[arg(long, default_value_t = 50)]
    pub threads: usize,

    /// TCP handshake samples per address.
    #[arg(long, default_value_t = 4)]
    pub ping: u32,

    /// Number of latency winners that receive a download test.
    #[arg(long, default_value_t = 5)]
    pub download_count: usize,

    /// Download test budget in seconds.
    #[arg(long, default_value_t = 10)]
    pub download_time: u64,

    /// Download test byte budget.
    #[arg(long, default_value_t = 10_000_000)]
    pub download_bytes: u64,

    /// Maximum concurrent download probes.
    #[arg(long, default_value_t = 4)]
    pub download_concurrency: usize,

    /// URL used for the download test.
    #[arg(
        long,
        default_value = "https://speed.cloudflare.com/__down?bytes=10000000"
    )]
    pub url: String,

    /// Candidate cap after CIDR expansion.
    #[arg(long, default_value_t = 1000)]
    pub max_candidates: usize,

    /// Reject addresses whose average latency exceeds this (milliseconds).
    #[arg(long, default_value_t = 300.0)]
    pub max_latency: f64,

    /// Reject addresses whose loss exceeds this (percent).
    #[arg(long, default_value_t = 20.0)]
    pub max_loss: f64,

    /// Minimum download speed required to mark a result as passed.
    #[arg(long, default_value_t = 1.0)]
    pub min_speed: f64,

    /// Per TCP connect timeout (milliseconds).
    #[arg(long, default_value_t = 1000)]
    pub timeout_ms: u64,

    /// Firewall mark for bypass mode `mark` (decimal or 0x hex).
    #[arg(long, default_value = "0x400000", value_parser = parse_mark)]
    pub mark: u32,

    /// WAN interface for device binding / the temporary direct route.
    #[arg(long)]
    pub device: Option<String>,

    /// Optional source address bound on probe sockets.
    #[arg(long)]
    pub source_ip: Option<IpAddr>,

    /// Routing table for the temporary fwmark rule.
    #[arg(long, default_value_t = 1688)]
    pub table: u32,

    /// OpenClash bypass strategy.
    ///
    /// Safety deviation from design §4.1: the design names `mark` the default,
    /// but this implementation defaults to `none` so an unconfigured run never
    /// touches the host firewall or routing. `mark`/`device` must be requested
    /// explicitly; LuCI should keep sending the user's selection.
    #[arg(long, value_enum, default_value = "none")]
    pub bypass_mode: BypassArg,

    /// Explicitly stop OpenClash for the run and start it again afterwards.
    ///
    /// Only effective when requested; the service is never paused implicitly.
    #[arg(long)]
    pub pause_openclash: bool,

    /// Result JSON path.
    #[arg(long, default_value = "/var/rust-dns/cf-optimizer/last.json")]
    pub output: PathBuf,

    /// Progress JSON path.
    #[arg(long, default_value = "/var/rust-dns/cf-optimizer/progress.json")]
    pub progress: PathBuf,

    /// Cancellation flag path.
    #[arg(long, default_value = "/var/rust-dns/cf-optimizer/cancel")]
    pub cancel_file: PathBuf,

    /// Deterministic fixture run; no network, no firewall changes.
    #[arg(long)]
    pub dry_run: bool,
}

impl Args {
    pub fn into_config(self) -> OptimizerConfig {
        let family = Family::from_u8(self.family).expect("clap validated the family");
        OptimizerConfig {
            source: self.source.into(),
            ip_file: self.ip_file,
            family,
            port: self.port,
            threads: self.threads,
            ping: self.ping,
            download_count: self.download_count,
            download_time: self.download_time,
            download_bytes: self.download_bytes,
            download_concurrency: self.download_concurrency,
            url: self.url,
            max_candidates: self.max_candidates,
            max_latency_ms: self.max_latency,
            max_loss_pct: self.max_loss,
            min_speed_mbps: self.min_speed,
            timeout_ms: self.timeout_ms,
            mark: self.mark,
            device: self.device,
            source_ip: self.source_ip,
            table: self.table,
            bypass_mode: self.bypass_mode.into(),
            pause_openclash: self.pause_openclash,
            output: self.output,
            progress: self.progress,
            cancel_file: self.cancel_file,
            dry_run: self.dry_run,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_map_to_documented_contract() {
        let args = Args::try_parse_from(["cf-optimize"]).unwrap();
        let config = args.into_config();
        assert_eq!(config.source, IpSourceKind::Cloudflare);
        assert_eq!(config.family, Family::V4);
        assert_eq!(config.port, 443);
        assert_eq!(config.threads, 50);
        assert_eq!(config.ping, 4);
        assert_eq!(config.download_count, 5);
        assert_eq!(config.mark, 0x40_0000);
        assert_eq!(config.table, 1688);
        assert_eq!(config.bypass_mode, BypassMode::None);
        assert!(!config.dry_run);
    }

    #[test]
    fn parses_all_documented_flags() {
        let args = Args::try_parse_from([
            "cf-optimize",
            "--source",
            "file",
            "--ip-file",
            "/tmp/ips.txt",
            "--family",
            "6",
            "--port",
            "8443",
            "--threads",
            "8",
            "--ping",
            "3",
            "--download-count",
            "2",
            "--download-time",
            "5",
            "--download-bytes",
            "1000",
            "--download-concurrency",
            "1",
            "--url",
            "https://speed.cloudflare.com/__down?bytes=1000",
            "--max-latency",
            "120.5",
            "--max-loss",
            "5",
            "--min-speed",
            "2.5",
            "--mark",
            "0x123456",
            "--device",
            "wan",
            "--source-ip",
            "104.16.0.1",
            "--table",
            "2000",
            "--bypass-mode",
            "mark",
            "--pause-openclash",
            "--output",
            "/tmp/out.json",
            "--progress",
            "/tmp/progress.json",
            "--cancel-file",
            "/tmp/cancel",
            "--dry-run",
        ])
        .unwrap();
        let config = args.into_config();
        assert_eq!(config.source, IpSourceKind::File);
        assert_eq!(
            config.ip_file.as_deref(),
            Some(std::path::Path::new("/tmp/ips.txt"))
        );
        assert_eq!(config.family, Family::V6);
        assert_eq!(config.port, 8443);
        assert_eq!(config.threads, 8);
        assert_eq!(config.ping, 3);
        assert_eq!(config.download_count, 2);
        assert_eq!(config.download_time, 5);
        assert_eq!(config.download_bytes, 1000);
        assert_eq!(config.download_concurrency, 1);
        assert_eq!(config.max_latency_ms, 120.5);
        assert_eq!(config.max_loss_pct, 5.0);
        assert_eq!(config.min_speed_mbps, 2.5);
        assert_eq!(config.mark, 0x12_3456);
        assert_eq!(config.device.as_deref(), Some("wan"));
        assert_eq!(config.source_ip, Some("104.16.0.1".parse().unwrap()));
        assert_eq!(config.table, 2000);
        assert_eq!(config.bypass_mode, BypassMode::Mark);
        assert!(config.pause_openclash);
        assert!(config.dry_run);
    }

    #[test]
    fn pausing_openclash_is_opt_in() {
        let args = Args::try_parse_from(["cf-optimize"]).unwrap();
        assert!(!args.into_config().pause_openclash);
    }

    #[test]
    fn rejects_bad_family_and_mark() {
        assert!(Args::try_parse_from(["cf-optimize", "--family", "5"]).is_err());
        assert!(Args::try_parse_from(["cf-optimize", "--mark", "nope"]).is_err());
        assert!(Args::try_parse_from(["cf-optimize", "--source", "nope"]).is_err());
    }

    #[test]
    fn mark_accepts_decimal_and_hex() {
        assert_eq!(parse_mark("4194304").unwrap(), 0x40_0000);
        assert_eq!(parse_mark("0x400000").unwrap(), 0x40_0000);
        assert_eq!(parse_mark("0X10").unwrap(), 16);
    }

    #[test]
    fn parse_errors_exit_1_but_help_and_version_exit_0() {
        let error = Args::try_parse_from(["cf-optimize", "--not-a-flag"]).unwrap_err();
        assert_eq!(exit_code_for_parse_error(&error), 1);

        let help = Args::try_parse_from(["cf-optimize", "--help"]).unwrap_err();
        assert_eq!(exit_code_for_parse_error(&help), 0);

        let version = Args::try_parse_from(["cf-optimize", "--version"]).unwrap_err();
        assert_eq!(exit_code_for_parse_error(&version), 0);
    }
}
