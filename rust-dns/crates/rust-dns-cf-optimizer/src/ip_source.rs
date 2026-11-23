// SPDX-License-Identifier: GPL-3.0-only
//! Candidate address sources: fixtures, user files, CIDR expansion, and the
//! pluggable Cloudflare range fetcher.

use std::collections::HashSet;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};

use crate::output::atomic_write;
use crate::probe::{DownloadUrl, ProbeSocketOptions, build_tls_client, https_get_body};
use crate::types::{Family, IpSourceKind, OptimizerConfig};

/// Fixed cache directory used when `--source cloudflare` is selected.
pub const DEFAULT_RANGE_CACHE_DIR: &str = "/var/rust-dns/cf-optimizer";
/// Range lists are refreshed at most once per day (design §3.1).
pub const RANGE_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

const RANGE_URL_V4: &str = "https://www.cloudflare.com/ips-v4";
const RANGE_URL_V6: &str = "https://www.cloudflare.com/ips-v6";

/// Bundled list used when neither the network nor the cache is available.
///
/// Sourced from Cloudflare's published ranges; it is intentionally conservative
/// because `max_candidates` bounds expansion anyway.
pub const BUNDLED_RANGES_V4: &[&str] = &[
    "173.245.48.0/20",
    "103.21.244.0/22",
    "103.22.200.0/22",
    "103.31.4.0/22",
    "141.101.64.0/18",
    "108.162.192.0/18",
    "190.93.240.0/20",
    "188.114.96.0/20",
    "197.234.240.0/22",
    "198.41.128.0/17",
    "162.158.0.0/15",
    "104.16.0.0/13",
    "104.24.0.0/14",
    "172.64.0.0/13",
    "131.0.72.0/22",
];

pub const BUNDLED_RANGES_V6: &[&str] = &[
    "2400:cb00::/32",
    "2606:4700::/32",
    "2803:f800::/32",
    "2405:b500::/32",
    "2405:8100::/32",
    "2a06:98c0::/29",
    "2c0f:f248::/32",
];

/// Fetches the official Cloudflare CIDR lists (`ips-v4`/`ips-v6`).
///
/// The trait boundary keeps tests off the network: inject a fake fetcher into
/// [`crate::pipeline::run_with_sources`].
pub trait RangeFetcher: Send + Sync {
    fn fetch<'a>(
        &'a self,
        family: Family,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>>> + Send + 'a>>;
}

/// The two HTTP GET URLs published by Cloudflare.
pub fn range_url(family: Family) -> &'static str {
    match family {
        Family::V4 => RANGE_URL_V4,
        Family::V6 => RANGE_URL_V6,
    }
}

/// Cache file for one family: `<dir>/ranges-v4.txt` or `<dir>/ranges-v6.txt`.
pub fn range_cache_path(cache_dir: &Path, family: Family) -> PathBuf {
    cache_dir.join(match family {
        Family::V4 => "ranges-v4.txt",
        Family::V6 => "ranges-v6.txt",
    })
}

/// Parse a Cloudflare range document into canonical `address/prefix` strings.
///
/// Blank lines and `#` comments are ignored; every entry must be a CIDR of the
/// requested family whose network address is public.
pub fn parse_range_list(text: &str, family: Family) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (line_no, raw_line) in text.lines().enumerate() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        for token in line
            .split([',', ' ', '\t'])
            .filter(|token| !token.is_empty())
        {
            let (addr, prefix) = parse_cidr(token).with_context(|| {
                format!("line {}: invalid Cloudflare CIDR {token:?}", line_no + 1)
            })?;
            if !family.matches(&addr) {
                bail!(
                    "line {}: {token:?} is not an IPv{} range",
                    line_no + 1,
                    family.as_u8()
                );
            }
            if !is_public_ip(&addr) {
                bail!(
                    "line {}: {token:?} is not a public Cloudflare range",
                    line_no + 1
                );
            }
            let canonical = format!("{addr}/{prefix}");
            if seen.insert(canonical.clone()) {
                out.push(canonical);
            }
        }
    }
    if out.is_empty() {
        bail!(
            "range document for IPv{} contained no entries",
            family.as_u8()
        );
    }
    Ok(out)
}

/// Bundled fallback ranges for `family`.
pub fn bundled_ranges(family: Family) -> Vec<String> {
    match family {
        Family::V4 => BUNDLED_RANGES_V4.iter().map(|s| s.to_string()).collect(),
        Family::V6 => BUNDLED_RANGES_V6.iter().map(|s| s.to_string()).collect(),
    }
}

/// Transport used by [`HttpsRangeFetcher`]; injectable for tests.
pub trait RangeTransport: Send + Sync {
    fn get<'a>(&'a self, url: &'a str)
    -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;
}

/// HTTPS transport for the Cloudflare range endpoints.
pub struct HttpsRangeTransport {
    timeout: Duration,
    tls: Arc<tokio_rustls::rustls::ClientConfig>,
}

impl HttpsRangeTransport {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            timeout,
            tls: build_tls_client()?,
        })
    }
}

impl RangeTransport for HttpsRangeTransport {
    fn get<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            let parsed = DownloadUrl::parse(url).with_context(|| format!("parse URL {url:?}"))?;
            if !parsed.is_tls() {
                bail!("Cloudflare range URLs must use HTTPS, got {url:?}");
            }
            let addrs: Vec<_> = tokio::net::lookup_host((parsed.host.as_str(), parsed.port))
                .await
                .with_context(|| format!("resolve {}", parsed.host))?
                .collect();
            if addrs.is_empty() {
                bail!("no address returned for {}", parsed.host);
            }
            let mut last_error = None;
            for addr in addrs {
                match https_get_body(
                    addr,
                    &parsed.host,
                    &parsed.host_header(),
                    &parsed.path,
                    &ProbeSocketOptions::default(),
                    self.timeout,
                    &self.tls,
                )
                .await
                {
                    Ok(body) => {
                        return String::from_utf8(body)
                            .context("Cloudflare range response is not UTF-8");
                    }
                    Err(err) => last_error = Some(err),
                }
            }
            Err(last_error.unwrap_or_else(|| anyhow!("no usable address for {}", parsed.host)))
        })
    }
}

/// Fetch Cloudflare ranges over HTTPS, caching them to a fixed path with a
/// 24-hour TTL. Order of preference: fresh cache, network, stale cache, then
/// the bundled list.
pub struct HttpsRangeFetcher<T = HttpsRangeTransport> {
    cache_dir: PathBuf,
    ttl: Duration,
    transport: T,
}

impl HttpsRangeFetcher<HttpsRangeTransport> {
    /// Production fetcher writing to [`DEFAULT_RANGE_CACHE_DIR`].
    pub fn new(cache_dir: impl Into<PathBuf>) -> Result<Self> {
        Ok(Self {
            cache_dir: cache_dir.into(),
            ttl: RANGE_CACHE_TTL,
            transport: HttpsRangeTransport::new(Duration::from_secs(10))?,
        })
    }
}

impl<T: RangeTransport> HttpsRangeFetcher<T> {
    /// Construct with an injected transport, TTL, and cache directory.
    pub fn with_transport(cache_dir: impl Into<PathBuf>, ttl: Duration, transport: T) -> Self {
        Self {
            cache_dir: cache_dir.into(),
            ttl,
            transport,
        }
    }

    fn read_cache(&self, family: Family) -> Option<(String, bool)> {
        let path = range_cache_path(&self.cache_dir, family);
        let text = std::fs::read_to_string(&path).ok()?;
        let fresh = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age <= self.ttl);
        Some((text, fresh))
    }

    async fn fetch_or_fallback(&self, family: Family) -> Result<Vec<String>> {
        if let Some((text, true)) = self.read_cache(family)
            && let Ok(parsed) = parse_range_list(&text, family)
        {
            return Ok(parsed);
        }

        match self.transport.get(range_url(family)).await {
            Ok(text) => match parse_range_list(&text, family) {
                Ok(parsed) => {
                    // Cache write failures must not discard a good fetch.
                    let _ =
                        atomic_write(&range_cache_path(&self.cache_dir, family), text.as_bytes());
                    Ok(parsed)
                }
                Err(_) => self.stale_or_bundled(family),
            },
            Err(_) => self.stale_or_bundled(family),
        }
    }

    fn stale_or_bundled(&self, family: Family) -> Result<Vec<String>> {
        if let Some((text, _)) = self.read_cache(family)
            && let Ok(parsed) = parse_range_list(&text, family)
        {
            return Ok(parsed);
        }
        Ok(bundled_ranges(family))
    }
}

impl<T: RangeTransport> RangeFetcher for HttpsRangeFetcher<T> {
    fn fetch<'a>(
        &'a self,
        family: Family,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>>> + Send + 'a>> {
        Box::pin(async move { self.fetch_or_fallback(family).await })
    }
}

/// Legacy placeholder retained for API compatibility; it always errors.
pub struct UnavailableRangeFetcher;

impl RangeFetcher for UnavailableRangeFetcher {
    fn fetch<'a>(
        &'a self,
        _family: Family,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>>> + Send + 'a>> {
        Box::pin(async {
            bail!("Cloudflare range fetching is unavailable; use --source file --ip-file <path>")
        })
    }
}

/// Resolve the configured source into a deduplicated, capped candidate list.
pub async fn load_candidates<R: RangeFetcher + ?Sized>(
    config: &OptimizerConfig,
    ranges: &R,
) -> Result<Vec<IpAddr>> {
    let ips = match config.source {
        IpSourceKind::Fixture => fixture_ips(config.family),
        IpSourceKind::File => {
            let path = config
                .ip_file
                .as_deref()
                .context("--source file requires --ip-file")?;
            load_ip_file(path, config.family, config.max_candidates)?
        }
        IpSourceKind::Cloudflare => {
            let lines = ranges.fetch(config.family).await?;
            parse_ip_entries(&lines.join("\n"), config.family, config.max_candidates)
                .context("parse Cloudflare ranges")?
        }
    };
    let mut seen = HashSet::new();
    let mut capped = Vec::new();
    for ip in ips {
        if !config.family.matches(&ip) {
            continue;
        }
        if seen.insert(ip) {
            capped.push(ip);
            if capped.len() >= config.max_candidates {
                break;
            }
        }
    }
    if capped.is_empty() {
        bail!("IP source produced no usable {:?} addresses", config.family);
    }
    Ok(capped)
}

/// Read IP/CIDR entries from a file. `#` starts a comment; entries may be
/// separated by whitespace or commas.
pub fn load_ip_file(path: &Path, family: Family, max_candidates: usize) -> Result<Vec<IpAddr>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read IP file {}", path.display()))?;
    parse_ip_entries(&text, family, max_candidates)
        .with_context(|| format!("parse IP file {}", path.display()))
}

/// Parse text containing IPs/CIDRs into a bounded, family-filtered list.
pub fn parse_ip_entries(text: &str, family: Family, max_candidates: usize) -> Result<Vec<IpAddr>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (line_no, raw_line) in text.lines().enumerate() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        for token in line.split([',', ' ', '\t']).filter(|t| !t.is_empty()) {
            let expanded = expand_entry(token, max_candidates - out.len())
                .with_context(|| format!("line {}: invalid IP/CIDR {token:?}", line_no + 1))?;
            for ip in expanded {
                if !family.matches(&ip) {
                    continue;
                }
                if !is_public_ip(&ip) {
                    bail!("line {}: {ip} is not a public address", line_no + 1);
                }
                if seen.insert(ip) {
                    out.push(ip);
                    if out.len() >= max_candidates {
                        return Ok(out);
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Expand a single `IP` or `CIDR` token, taking at most `limit` addresses.
pub fn expand_entry(token: &str, limit: usize) -> Result<Vec<IpAddr>> {
    if token.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    if token.contains('/') {
        let (addr, prefix) = parse_cidr(token)?;
        expand_cidr(addr, prefix, limit)
    } else {
        let addr: IpAddr = token
            .parse()
            .with_context(|| format!("invalid IP address {token:?}"))?;
        Ok(vec![addr])
    }
}

/// Parse `address/prefix`.
pub fn parse_cidr(input: &str) -> Result<(IpAddr, u8)> {
    let (addr, prefix) = input
        .split_once('/')
        .with_context(|| format!("missing prefix length in {input:?}"))?;
    let addr: IpAddr = addr
        .parse()
        .with_context(|| format!("invalid network address in {input:?}"))?;
    let prefix: u8 = prefix
        .parse()
        .with_context(|| format!("invalid prefix length in {input:?}"))?;
    let max = match addr {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    if prefix > max {
        bail!("prefix length /{prefix} is invalid for {addr}");
    }
    Ok((addr, prefix))
}

/// Expand a network prefix in ascending order. The input host bits are masked.
pub fn expand_cidr(addr: IpAddr, prefix: u8, limit: usize) -> Result<Vec<IpAddr>> {
    let mut out = Vec::new();
    if limit == 0 {
        return Ok(out);
    }
    match addr {
        IpAddr::V4(v4) => {
            if prefix > 32 {
                bail!("IPv4 prefix /{prefix} is invalid");
            }
            let host_bits = 32 - u32::from(prefix);
            let base = u32::from(v4) & mask_v4(prefix);
            let count = 1_u64 << host_bits;
            let take = count.min(limit as u64);
            for offset in 0..take {
                let value = (u64::from(base) + offset) as u32;
                out.push(IpAddr::V4(Ipv4Addr::from(value)));
            }
        }
        IpAddr::V6(v6) => {
            if prefix > 128 {
                bail!("IPv6 prefix /{prefix} is invalid");
            }
            let host_bits = 128 - u32::from(prefix);
            let base = u128::from(v6) & mask_v6(prefix);
            let count = if host_bits >= 63 {
                u64::MAX
            } else {
                1_u64 << host_bits
            };
            let take = count.min(limit as u64);
            for offset in 0..take {
                out.push(IpAddr::V6(Ipv6Addr::from(base + u128::from(offset))));
            }
        }
    }
    Ok(out)
}

fn mask_v4(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    }
}

fn mask_v6(prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - u32::from(prefix))
    }
}

/// Reject addresses that must never be probed (private, loopback, test nets,
/// multicast, link-local, documentation ranges, ...).
pub fn is_public_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(*v4),
        IpAddr::V6(v6) => is_public_v6(*v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    if a == 0 || a == 10 || a == 127 || a >= 224 {
        return false;
    }
    if a == 100 && (64..=127).contains(&b) {
        return false; // 100.64.0.0/10 CGNAT
    }
    if a == 169 && b == 254 {
        return false; // link-local
    }
    if a == 172 && (16..=31).contains(&b) {
        return false;
    }
    if a == 192 && b == 168 {
        return false;
    }
    if a == 192 && b == 0 && (c == 0 || c == 2) {
        return false; // 192.0.0.0/24, TEST-NET-1
    }
    if a == 198 && (b == 18 || b == 19) {
        return false; // benchmarking
    }
    if a == 198 && b == 51 && c == 100 {
        return false; // TEST-NET-2
    }
    if a == 203 && b == 0 && c == 113 {
        return false; // TEST-NET-3
    }
    if a == 192 && b == 88 && c == 99 {
        return false; // 6to4 relay anycast
    }
    true
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_loopback() {
        return false;
    }
    let segments = ip.segments();
    if segments[0] & 0xfe00 == 0xfc00 {
        return false; // unique local fc00::/7
    }
    if segments[0] & 0xffc0 == 0xfe80 {
        return false; // link-local fe80::/10
    }
    if segments[0] & 0xff00 == 0xff00 {
        return false; // multicast
    }
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return false; // documentation
    }
    if segments[0] == 0
        && segments[1] == 0
        && segments[2] == 0
        && segments[3] == 0
        && segments[4] == 0
        && segments[5] == 0xffff
    {
        return false; // IPv4-mapped
    }
    true
}

/// Deterministic fixture list used by `--dry-run`.
pub fn fixture_ips(family: Family) -> Vec<IpAddr> {
    let strings: &[&str] = match family {
        Family::V4 => &[
            "104.16.0.1",
            "104.16.0.2",
            "104.16.0.3",
            "104.16.0.4",
            "104.16.0.8",
            "104.16.0.9",
            "104.17.0.1",
            "104.17.0.2",
            "104.18.0.1",
            "104.19.0.1",
        ],
        Family::V6 => &[
            "2606:4700:10::6810:1",
            "2606:4700:10::6810:2",
            "2606:4700:10::6810:3",
            "2606:4700:10::6810:4",
            "2606:4700:10::6810:8",
            "2606:4700:10::6810:9",
            "2606:4700:20::6810:1",
            "2606:4700:20::6810:2",
            "2606:4700:30::6810:1",
            "2606:4700:40::6810:1",
        ],
    };
    strings
        .iter()
        .map(|s| s.parse().expect("fixture addresses are valid"))
        .collect()
}

/// Deterministic simulated latency for dry-run mode.
pub fn simulated_latency_ms(ip: IpAddr) -> f64 {
    let seed = fixture_seed(ip);
    8.0 + f64::from(seed % 97) * 1.5
}

/// Deterministic simulated packet loss for dry-run mode.
pub fn simulated_loss_pct(ip: IpAddr) -> f64 {
    match fixture_seed(ip) % 16 {
        9 => 100.0,
        8 => 25.0,
        _ => 0.0,
    }
}

/// Deterministic simulated download speed for dry-run mode.
pub fn simulated_speed_mbps(ip: IpAddr) -> f64 {
    if simulated_loss_pct(ip) >= 100.0 {
        return 0.0;
    }
    let seed = fixture_seed(ip);
    (140.0 - f64::from(seed % 97) * 7.0).max(1.25)
}

fn fixture_seed(ip: IpAddr) -> u16 {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            u16::from(o[2]) * 256 + u16::from(o[3])
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[7]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_parsing_and_expansion_v4() {
        let (addr, prefix) = parse_cidr("104.16.0.1/30").unwrap();
        assert_eq!(prefix, 30);
        let ips = expand_cidr(addr, prefix, 10).unwrap();
        assert_eq!(ips.len(), 4);
        assert_eq!(ips[0].to_string(), "104.16.0.0");
        assert_eq!(ips[3].to_string(), "104.16.0.3");
    }

    #[test]
    fn expansion_respects_limit_and_prefix_31() {
        let (addr, prefix) = parse_cidr("104.16.0.4/31").unwrap();
        let ips = expand_cidr(addr, prefix, 100).unwrap();
        assert_eq!(ips.len(), 2);
        let (addr, prefix) = parse_cidr("104.16.0.0/8").unwrap();
        assert_eq!(expand_cidr(addr, prefix, 3).unwrap().len(), 3);
    }

    #[test]
    fn cidr_expansion_v6() {
        let (addr, prefix) = parse_cidr("2606:4700:10::/126").unwrap();
        let ips = expand_cidr(addr, prefix, 8).unwrap();
        assert_eq!(ips.len(), 4);
        assert_eq!(ips[0].to_string(), "2606:4700:10::");
    }

    #[test]
    fn prefix_out_of_range_is_rejected() {
        assert!(parse_cidr("104.16.0.1/33").is_err());
        assert!(parse_cidr("2606:4700::1/129").is_err());
        assert!(parse_cidr("not-an-ip/24").is_err());
    }

    #[test]
    fn public_address_rules() {
        for public in ["104.16.0.1", "1.1.1.1", "2606:4700::1111"] {
            assert!(
                is_public_ip(&public.parse().unwrap()),
                "{public} should be public"
            );
        }
        for private in [
            "10.0.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "192.168.1.1",
            "172.16.0.1",
            "100.64.0.1",
            "203.0.113.10",
            "198.51.100.1",
            "224.0.0.1",
            "::1",
            "fe80::1",
            "fc00::1",
            "ff02::1",
            "2001:db8::1",
        ] {
            assert!(
                !is_public_ip(&private.parse().unwrap()),
                "{private} should not be public"
            );
        }
    }

    #[test]
    fn entries_are_deduplicated_and_filtered_by_family() {
        let text = "# comment\n104.16.0.1 104.16.0.1\n2606:4700::1\n";
        let v4 = parse_ip_entries(text, Family::V4, 100).unwrap();
        assert_eq!(v4.len(), 1);
        let v6 = parse_ip_entries(text, Family::V6, 100).unwrap();
        assert_eq!(v6.len(), 1);
    }

    #[test]
    fn private_file_entry_is_rejected() {
        let err = parse_ip_entries("192.168.1.1\n", Family::V4, 100).unwrap_err();
        assert!(err.to_string().contains("not a public address"));
    }

    #[test]
    fn file_loading_reads_comments_and_caps() {
        let dir = std::env::temp_dir().join(format!("cf-ipfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ips.txt");
        std::fs::write(&path, "104.16.0.0/30 # block\n104.17.0.1\n").unwrap();
        let ips = load_ip_file(&path, Family::V4, 3).unwrap();
        assert_eq!(ips.len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fixtures_are_deterministic_per_family() {
        let v4 = fixture_ips(Family::V4);
        assert_eq!(v4.len(), 10);
        assert!(v4.iter().all(|ip| ip.is_ipv4()));
        let v6 = fixture_ips(Family::V6);
        assert_eq!(v6.len(), 10);
        assert!(v6.iter().all(|ip| ip.is_ipv6()));
        assert_eq!(simulated_latency_ms(v4[0]), simulated_latency_ms(v4[0]));
    }

    #[test]
    fn range_documents_parse_comments_and_dedupe() {
        let text = "# Cloudflare\n104.16.0.0/13\n\n104.16.0.0/13, 172.64.0.0/13 # dup\n";
        let ranges = parse_range_list(text, Family::V4).unwrap();
        assert_eq!(
            ranges,
            vec!["104.16.0.0/13".to_string(), "172.64.0.0/13".to_string()]
        );

        let v6 = parse_range_list("2606:4700::/32\n", Family::V6).unwrap();
        assert_eq!(v6, vec!["2606:4700::/32".to_string()]);
    }

    #[test]
    fn range_documents_reject_wrong_family_and_private_ranges() {
        assert!(parse_range_list("2606:4700::/32\n", Family::V4).is_err());
        assert!(parse_range_list("192.168.0.0/16\n", Family::V4).is_err());
        assert!(parse_range_list("104.16.0.1\n", Family::V4).is_err());
        assert!(parse_range_list("# only comments\n", Family::V4).is_err());
    }

    #[test]
    fn bundled_ranges_parse_for_both_families() {
        for family in [Family::V4, Family::V6] {
            let ranges = bundled_ranges(family);
            let joined = ranges.join("\n");
            let parsed = parse_range_list(&joined, family).unwrap();
            assert_eq!(parsed.len(), ranges.len());
        }
    }

    #[test]
    fn cache_paths_are_fixed_per_family() {
        let dir = Path::new(DEFAULT_RANGE_CACHE_DIR);
        assert_eq!(range_cache_path(dir, Family::V4), dir.join("ranges-v4.txt"));
        assert_eq!(range_cache_path(dir, Family::V6), dir.join("ranges-v6.txt"));
        assert!(range_url(Family::V4).ends_with("/ips-v4"));
        assert!(range_url(Family::V6).ends_with("/ips-v6"));
    }

    struct FakeTransport {
        response: Result<String, String>,
        calls: std::sync::Mutex<usize>,
    }

    impl RangeTransport for FakeTransport {
        fn get<'a>(
            &'a self,
            _url: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
            *self.calls.lock().unwrap() += 1;
            let response = self.response.clone().map_err(anyhow::Error::msg);
            Box::pin(async move { response })
        }
    }

    fn cache_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cf-ranges-{}-{name}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn network_fetch_writes_the_cache() {
        let dir = cache_dir("fetch");
        let transport = FakeTransport {
            response: Ok("104.16.0.0/13\n".to_string()),
            calls: std::sync::Mutex::new(0),
        };
        let fetcher = HttpsRangeFetcher::with_transport(&dir, RANGE_CACHE_TTL, transport);
        let ranges = fetcher.fetch(Family::V4).await.unwrap();
        assert_eq!(ranges, vec!["104.16.0.0/13".to_string()]);
        let cached = std::fs::read_to_string(range_cache_path(&dir, Family::V4)).unwrap();
        assert_eq!(cached, "104.16.0.0/13\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn fresh_cache_avoids_the_network() {
        let dir = cache_dir("fresh");
        std::fs::write(range_cache_path(&dir, Family::V4), "104.16.0.0/13\n").unwrap();
        let transport = FakeTransport {
            response: Err("network must not be used".to_string()),
            calls: std::sync::Mutex::new(0),
        };
        let fetcher = HttpsRangeFetcher::with_transport(&dir, RANGE_CACHE_TTL, transport);
        let ranges = fetcher.fetch(Family::V4).await.unwrap();
        assert_eq!(ranges, vec!["104.16.0.0/13".to_string()]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn stale_cache_is_returned_when_the_network_fails() {
        let dir = cache_dir("stale");
        std::fs::write(range_cache_path(&dir, Family::V4), "104.17.0.0/14\n").unwrap();
        let transport = FakeTransport {
            response: Err("offline".to_string()),
            calls: std::sync::Mutex::new(0),
        };
        // A zero TTL forces the cache to be considered stale.
        let fetcher = HttpsRangeFetcher::with_transport(&dir, Duration::ZERO, transport);
        let ranges = fetcher.fetch(Family::V4).await.unwrap();
        assert_eq!(ranges, vec!["104.17.0.0/14".to_string()]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn bundled_ranges_are_the_last_resort() {
        let dir = cache_dir("bundled");
        let transport = FakeTransport {
            response: Err("offline".to_string()),
            calls: std::sync::Mutex::new(0),
        };
        let fetcher = HttpsRangeFetcher::with_transport(&dir, RANGE_CACHE_TTL, transport);
        let ranges = fetcher.fetch(Family::V6).await.unwrap();
        assert_eq!(ranges, bundled_ranges(Family::V6));
        assert!(!range_cache_path(&dir, Family::V6).exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
