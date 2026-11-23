// SPDX-License-Identifier: GPL-3.0-only
//! Serde types shared by the collector and the HTTP API.
//!
//! Field names and JSON shapes follow
//! `.trellis/tasks/11-23-luci-management-ui/design.md` §5 verbatim.

use serde::{Deserialize, Serialize};

/// Maximum number of answers kept per logged query.
pub const MAX_ANSWERS: usize = 8;
/// Maximum bytes of a single answer's textual data.
pub const MAX_ANSWER_BYTES: usize = 512;
/// Maximum bytes of a normalized domain name.
pub const MAX_DOMAIN_BYTES: usize = 253;
/// Maximum bytes of the `search` parameter.
pub const MAX_SEARCH_CHARS: usize = 128;

/// One answer summary attached to a logged query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    /// Record type mnemonic such as `A` or `AAAA`.
    #[serde(rename = "type")]
    pub record_type: String,
    /// Normalized textual representation of the record data.
    pub data: String,
    /// TTL in seconds.
    pub ttl: u32,
}

/// Raw answer as supplied by the query pipeline before normalization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawAnswer {
    /// Numeric DNS record type.
    #[serde(rename = "type")]
    pub record_type: u16,
    /// Textual record data.
    pub data: String,
    /// TTL in seconds.
    pub ttl: u32,
}

/// A completed DNS query handed to [`crate::StatsCollector::record`].
///
/// This is the capture struct produced by the DNS hot path. It stays free of
/// DNS core types so this crate does not depend on `crates/rust-dns`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryEvent {
    /// Client IP address (port is ignored by callers).
    pub client_ip: String,
    /// Query name; normalization happens in the collector.
    pub domain: String,
    /// Numeric DNS query type (`1` = A).
    pub qtype: u16,
    /// Numeric DNS response code (`0` = NOERROR).
    pub status: u16,
    /// Query duration in whole milliseconds.
    pub elapsed_ms: u64,
    /// Response answers, truncated to [`MAX_ANSWERS`] by the collector.
    #[serde(default)]
    pub answers: Vec<RawAnswer>,
    /// The query was terminated by a rule or blocker.
    #[serde(default)]
    pub is_blocked: bool,
    /// The query was served from cache.
    #[serde(default)]
    pub is_cached: bool,
    /// Upstream tag, or an empty string.
    #[serde(default)]
    pub upstream: String,
    /// Matched rule tag, or an empty string.
    #[serde(default)]
    pub rule: String,
}

/// A stored query record as returned by `/api/v1/logs` and the dump file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryRecord {
    /// RFC 3339 UTC timestamp with millisecond precision, `Z` suffixed.
    pub timestamp: String,
    /// Client IP address text.
    pub client_ip: String,
    /// Normalized (lowercase, trailing dot removed) domain.
    pub domain: String,
    /// Uppercase record type mnemonic, or a decimal string when unknown.
    pub qtype: String,
    /// DNS rcode mnemonic, or a decimal string when unknown.
    pub status: String,
    /// Query duration in milliseconds.
    pub elapsed_ms: u64,
    /// Answer summaries, at most [`MAX_ANSWERS`].
    pub answers: Vec<Answer>,
    /// The query was terminated by a rule or blocker.
    pub is_blocked: bool,
    /// The query was served from cache.
    pub is_cached: bool,
    /// Upstream tag; `""` when absent.
    pub upstream: String,
    /// Matched rule tag; `""` when absent.
    pub rule: String,
}

/// `/api/v1/health` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    /// Always `"ok"`.
    pub status: String,
    /// Crate version.
    pub version: String,
    /// Seconds since the collector started.
    pub uptime_s: u64,
}

/// `/api/v1/version` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionResponse {
    /// Crate version.
    pub version: String,
}

/// `/api/v1/stats` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatsResponse {
    /// Queries recorded since startup or reset.
    pub total_queries: u64,
    /// Queries marked blocked.
    pub blocked_queries: u64,
    /// Queries marked cached.
    pub cached_queries: u64,
    /// `blocked / total * 100`, rounded to two decimals.
    pub blocked_percentage: f64,
    /// `cached / total * 100`, rounded to two decimals.
    pub cached_percentage: f64,
    /// Mean elapsed milliseconds, rounded to two decimals.
    pub avg_latency_ms: f64,
    /// Queries per second over the recent window, rounded to two decimals.
    pub qps: f64,
}

/// One history bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPoint {
    /// Bucket start time as Unix seconds (UTC).
    pub time: i64,
    /// Queries in the bucket.
    pub total: u64,
    /// Blocked queries in the bucket.
    pub blocked: u64,
    /// Cached queries in the bucket.
    pub cached: u64,
}

/// `/api/v1/history` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryResponse {
    /// Chronologically ascending buckets.
    pub points: Vec<HistoryPoint>,
}

/// One entry of `top_domains` or `top_blocked`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopDomainEntry {
    /// Normalized domain.
    pub domain: String,
    /// Occurrences.
    pub count: u64,
}

/// One entry of `top_clients`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopClientEntry {
    /// Client IP text.
    pub client_ip: String,
    /// Occurrences.
    pub count: u64,
}

/// `/api/v1/top` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopResponse {
    /// Most queried domains.
    pub top_domains: Vec<TopDomainEntry>,
    /// Most blocked domains.
    pub top_blocked: Vec<TopDomainEntry>,
    /// Most active clients.
    pub top_clients: Vec<TopClientEntry>,
}

/// `/api/v1/logs` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogsResponse {
    /// Matching retained records before pagination.
    pub total: usize,
    /// Requested page, newest first.
    pub items: Vec<QueryRecord>,
}

/// `{success:true}` response for write endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuccessResponse {
    /// Always `true`.
    pub success: bool,
}

impl SuccessResponse {
    /// Construct the canonical success body.
    pub fn ok() -> Self {
        Self { success: true }
    }
}

/// Unified error body: `{"error":"..."}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Human-readable error; never contains internal paths or stacks.
    pub error: String,
}

/// Lowercase a domain, remove trailing root dots and truncate to 253 bytes.
pub fn normalize_domain(domain: &str) -> String {
    let trimmed = domain.trim().trim_end_matches('.');
    let lowered = trimmed.to_ascii_lowercase();
    truncate_bytes(&lowered, MAX_DOMAIN_BYTES).to_string()
}

/// Map a numeric DNS query type to its uppercase mnemonic.
///
/// Unknown types fall back to the decimal representation.
pub fn qtype_name(qtype: u16) -> String {
    let name = match qtype {
        1 => "A",
        2 => "NS",
        3 => "MD",
        4 => "MF",
        5 => "CNAME",
        6 => "SOA",
        7 => "MB",
        8 => "MG",
        9 => "MR",
        10 => "NULL",
        11 => "WKS",
        12 => "PTR",
        13 => "HINFO",
        14 => "MINFO",
        15 => "MX",
        16 => "TXT",
        17 => "RP",
        18 => "AFSDB",
        19 => "X25",
        20 => "ISDN",
        21 => "RT",
        22 => "NSAP",
        23 => "NSAP-PTR",
        24 => "SIG",
        25 => "KEY",
        26 => "PX",
        27 => "GPOS",
        28 => "AAAA",
        29 => "LOC",
        30 => "NXT",
        31 => "EID",
        32 => "NIMLOC",
        33 => "SRV",
        34 => "ATMA",
        35 => "NAPTR",
        36 => "KX",
        37 => "CERT",
        38 => "A6",
        39 => "DNAME",
        40 => "SINK",
        41 => "OPT",
        42 => "APL",
        43 => "DS",
        44 => "SSHFP",
        45 => "IPSECKEY",
        46 => "RRSIG",
        47 => "NSEC",
        48 => "DNSKEY",
        49 => "DHCID",
        50 => "NSEC3",
        51 => "NSEC3PARAM",
        52 => "TLSA",
        53 => "SMIMEA",
        55 => "HIP",
        56 => "NINFO",
        57 => "RKEY",
        58 => "TALINK",
        59 => "CDS",
        60 => "CDNSKEY",
        61 => "OPENPGPKEY",
        62 => "CSYNC",
        63 => "ZONEMD",
        64 => "SVCB",
        65 => "HTTPS",
        99 => "SPF",
        100 => "UINFO",
        101 => "UID",
        102 => "GID",
        103 => "UNSPEC",
        104 => "NID",
        105 => "L32",
        106 => "L64",
        107 => "LP",
        108 => "EUI48",
        109 => "EUI64",
        249 => "TKEY",
        250 => "TSIG",
        251 => "IXFR",
        252 => "AXFR",
        253 => "MAILB",
        254 => "MAILA",
        255 => "ANY",
        256 => "URI",
        257 => "CAA",
        258 => "AVC",
        259 => "DOA",
        260 => "AMTRELAY",
        32768 => "TA",
        32769 => "DLV",
        _ => return qtype.to_string(),
    };
    name.to_string()
}

/// Map a numeric DNS response code to its mnemonic.
///
/// Unknown rcodes fall back to the decimal representation.
pub fn rcode_name(rcode: u16) -> String {
    let name = match rcode {
        0 => "NOERROR",
        1 => "FORMERR",
        2 => "SERVFAIL",
        3 => "NXDOMAIN",
        4 => "NOTIMP",
        5 => "REFUSED",
        6 => "YXDOMAIN",
        7 => "YXRRSET",
        8 => "NXRRSET",
        9 => "NOTAUTH",
        10 => "NOTZONE",
        11 => "DSOTYPENI",
        16 => "BADSIG",
        17 => "BADKEY",
        18 => "BADTIME",
        19 => "BADMODE",
        20 => "BADNAME",
        21 => "BADALG",
        22 => "BADTRUNC",
        23 => "BADCOOKIE",
        _ => return rcode.to_string(),
    };
    name.to_string()
}

/// Truncate a string to at most `max` bytes without splitting a UTF-8 char.
pub fn truncate_bytes(value: &str, max: usize) -> &str {
    if value.len() <= max {
        return value;
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Normalize raw answers, keeping at most [`MAX_ANSWERS`] entries.
pub(crate) fn summarize_answers(answers: &[RawAnswer]) -> Vec<Answer> {
    answers
        .iter()
        .take(MAX_ANSWERS)
        .map(|answer| Answer {
            record_type: qtype_name(answer.record_type),
            data: truncate_bytes(&answer.data, MAX_ANSWER_BYTES).to_string(),
            ttl: answer.ttl,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_domains() {
        assert_eq!(normalize_domain("Example.COM."), "example.com");
        assert_eq!(normalize_domain("  localhost.  "), "localhost");
        assert_eq!(normalize_domain(".."), "");
    }

    #[test]
    fn truncates_domains_to_253_bytes() {
        let long = "a".repeat(300);
        assert_eq!(normalize_domain(&long).len(), MAX_DOMAIN_BYTES);
    }

    #[test]
    fn maps_qtypes_and_rcodes() {
        assert_eq!(qtype_name(1), "A");
        assert_eq!(qtype_name(28), "AAAA");
        assert_eq!(qtype_name(65), "HTTPS");
        assert_eq!(qtype_name(999), "999");
        assert_eq!(rcode_name(0), "NOERROR");
        assert_eq!(rcode_name(3), "NXDOMAIN");
        assert_eq!(rcode_name(42), "42");
    }

    #[test]
    fn answer_type_field_uses_json_name() {
        let answer = Answer {
            record_type: "A".to_string(),
            data: "93.184.216.34".to_string(),
            ttl: 300,
        };
        let json = serde_json::to_value(&answer).unwrap();
        assert_eq!(json["type"], "A");
        assert!(json.get("record_type").is_none());
    }
}
