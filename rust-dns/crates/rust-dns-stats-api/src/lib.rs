// SPDX-License-Identifier: GPL-3.0-only
//! `/api/v1/*` stats and admin API for rust-dns.
//!
//! The authoritative JSON contract lives in
//! `.trellis/tasks/11-23-luci-management-ui/design.md` §5 and
//! `.trellis/tasks/11-23-rust-dns-stats-api/design.md` §2.
//!
//! The crate is independent of the DNS core: the query pipeline feeds
//! [`types::QueryEvent`] values into [`StatsCollector::record`], which is
//! lock-free and non-blocking on the hot path.

pub mod collector;
pub mod dump;
pub mod http;
pub mod time;
pub mod types;

pub use collector::{
    DEFAULT_HISTORY_POINTS, DEFAULT_LOGS_LIMIT, DEFAULT_TOP_LIMIT, HISTORY_CAPACITY, LogFilter,
    LogQuery, StatsCollector, StatsConfig,
};
pub use dump::{DUMP_MAGIC, DUMP_VERSION, DumpData, LoadOutcome};
pub use http::{ApiError, CacheFlush, router, serve, serve_with_cache, serve_with_listener};
pub use time::{Clock, ManualClock, SystemClock, rfc3339_millis};
pub use types::{
    Answer, ErrorResponse, HealthResponse, HistoryPoint, HistoryResponse, LogsResponse, QueryEvent,
    QueryRecord, RawAnswer, StatsResponse, SuccessResponse, TopClientEntry, TopDomainEntry,
    TopResponse, VersionResponse,
};

/// The crate version reported by `/api/v1/health` and `/api/v1/version`.
pub const fn crate_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Marker retained for compatibility with the task scaffolding.
pub const IMPLEMENTED: bool = true;
