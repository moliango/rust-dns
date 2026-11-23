// SPDX-License-Identifier: GPL-3.0-only
//! Non-blocking statistics collector: counters, QPS, history, top and logs.
//!
//! The DNS hot path calls [`StatsCollector::record`], which only bumps atomics,
//! stamps a timestamp and performs a bounded `try_send`. A single aggregator
//! task owns the ring buffer, QPS slots, history buckets and top maps behind a
//! short-lived mutex; HTTP handlers clone bounded snapshots.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::dump::{
    DUMP_MAGIC, DUMP_VERSION, DumpCounters, DumpData, DumpTop, LoadOutcome, corrupt_path,
    read_dump, write_dump,
};
use crate::time::{Clock, SystemClock, rfc3339_millis};
use crate::types::{
    HealthResponse, HistoryPoint, LogsResponse, QueryEvent, QueryRecord, StatsResponse,
    TopClientEntry, TopDomainEntry, TopResponse, normalize_domain, qtype_name, rcode_name,
    summarize_answers,
};

/// Fixed history ring capacity (7 days of hourly buckets).
pub const HISTORY_CAPACITY: usize = 168;
/// Default `points` for `/api/v1/history`.
pub const DEFAULT_HISTORY_POINTS: usize = 24;
/// Default `limit` for `/api/v1/top`.
pub const DEFAULT_TOP_LIMIT: usize = 10;
/// Default `limit` for `/api/v1/logs`.
pub const DEFAULT_LOGS_LIMIT: usize = 20;
/// Per-table hard cap for top aggregates.
pub const TOP_HARD_CAP: usize = 10_000;
/// Default collector configuration values (design §8).
pub const DEFAULT_CAPACITY: usize = 2000;

const TOP_PRUNE_THRESHOLD: usize = 8_000;
const TOP_PRUNE_INTERVAL_SECS: u64 = 60;

/// Collector configuration. Values are clamped by [`StatsConfig::sanitized`].
#[derive(Debug, Clone)]
pub struct StatsConfig {
    /// When false, no events are aggregated and all read endpoints are empty.
    pub enabled: bool,
    /// Ring-buffer capacity, clamped to 100..=100000.
    pub capacity: usize,
    /// Maximum accepted `points`, clamped to 1..=168.
    pub history_max: usize,
    /// Maximum accepted top `limit`, clamped to 1..=1000.
    pub top_max: usize,
    /// Maximum accepted logs `limit`, clamped to 1..=capacity.
    pub logs_max: usize,
    /// Bounded event channel capacity.
    pub channel_capacity: usize,
    /// QPS averaging window in seconds.
    pub qps_window_secs: u64,
    /// History bucket width in seconds.
    pub history_bucket_secs: u64,
    /// Periodic dump interval in seconds.
    pub dump_interval_secs: u64,
    /// Periodic dump path; `None` disables dumping.
    pub dump_path: Option<std::path::PathBuf>,
}

impl Default for StatsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            capacity: DEFAULT_CAPACITY,
            history_max: HISTORY_CAPACITY,
            top_max: 100,
            logs_max: 200,
            channel_capacity: 4096,
            qps_window_secs: 10,
            history_bucket_secs: 3600,
            dump_interval_secs: 600,
            dump_path: None,
        }
    }
}

impl StatsConfig {
    /// Clamp every field to the documented range.
    pub fn sanitized(mut self) -> Self {
        self.capacity = self.capacity.clamp(100, 100_000);
        self.history_max = self.history_max.clamp(1, HISTORY_CAPACITY);
        self.top_max = self.top_max.clamp(1, 1000);
        self.logs_max = self.logs_max.clamp(1, self.capacity);
        self.channel_capacity = self.channel_capacity.max(1);
        self.qps_window_secs = self.qps_window_secs.clamp(1, 60);
        self.history_bucket_secs = self.history_bucket_secs.max(1);
        self.dump_interval_secs = self.dump_interval_secs.clamp(60, 86_400);
        self
    }
}

/// Filter for `/api/v1/logs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFilter {
    /// Every retained record.
    #[default]
    All,
    /// Only records with `is_blocked = true`.
    Blocked,
    /// Only records with `is_cached = true`.
    Cached,
}

impl LogFilter {
    /// Parse the `filter` query value.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "all" => Some(Self::All),
            "blocked" => Some(Self::Blocked),
            "cached" => Some(Self::Cached),
            _ => None,
        }
    }

    fn matches(self, record: &QueryRecord) -> bool {
        match self {
            Self::All => true,
            Self::Blocked => record.is_blocked,
            Self::Cached => record.is_cached,
        }
    }
}

/// Validated `/api/v1/logs` query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogQuery {
    /// Page size.
    pub limit: usize,
    /// Records to skip, counted from the newest record.
    pub offset: usize,
    /// Case-insensitive domain substring.
    pub search: String,
    /// Blocked/cached filter.
    pub filter: LogFilter,
}

impl Default for LogQuery {
    fn default() -> Self {
        Self {
            limit: DEFAULT_LOGS_LIMIT,
            offset: 0,
            search: String::new(),
            filter: LogFilter::All,
        }
    }
}

#[derive(Default)]
struct Counters {
    total: AtomicU64,
    blocked: AtomicU64,
    cached: AtomicU64,
    latency_us: AtomicU64,
    dropped: AtomicU64,
}

#[derive(Clone, Copy)]
struct QpsSlot {
    sec: u64,
    count: u64,
}

struct HistoryBucket {
    mono_index: u64,
    time: i64,
    total: u64,
    blocked: u64,
    cached: u64,
}

#[derive(Default)]
struct TopMaps {
    domains: HashMap<String, u64>,
    blocked: HashMap<String, u64>,
    clients: HashMap<String, u64>,
}

impl TopMaps {
    fn bump(map: &mut HashMap<String, u64>, key: &str) {
        if let Some(count) = map.get_mut(key) {
            *count = count.saturating_add(1);
            return;
        }
        if map.len() >= TOP_HARD_CAP {
            // Bounded memory: once full, new keys with a count of one are
            // approximated away instead of growing the table.
            return;
        }
        map.insert(key.to_string(), 1);
    }

    fn prune(&mut self) {
        for map in [&mut self.domains, &mut self.blocked, &mut self.clients] {
            if map.len() > TOP_PRUNE_THRESHOLD {
                map.retain(|_, count| *count >= 2);
            }
        }
    }
}

struct Store {
    capacity: usize,
    ring: VecDeque<QueryRecord>,
    qps: Vec<QpsSlot>,
    window: u64,
    history: VecDeque<HistoryBucket>,
    history_bucket_secs: u64,
    top: TopMaps,
    last_prune_mono: u64,
}

impl Store {
    fn new(config: &StatsConfig) -> Self {
        Self {
            capacity: config.capacity,
            ring: VecDeque::with_capacity(config.capacity.min(4096)),
            qps: (0..config.qps_window_secs)
                .map(|_| QpsSlot { sec: 0, count: 0 })
                .collect(),
            window: config.qps_window_secs.max(1),
            history: VecDeque::new(),
            history_bucket_secs: config.history_bucket_secs.max(1),
            top: TopMaps::default(),
            last_prune_mono: 0,
        }
    }

    fn on_event(&mut self, wall_millis: i64, mono: u64, event: &QueryEvent) {
        self.bump_qps(mono);
        self.bump_history(wall_millis, mono, event);
        self.bump_top(event);

        if mono.saturating_sub(self.last_prune_mono) >= TOP_PRUNE_INTERVAL_SECS {
            self.last_prune_mono = mono;
            self.top.prune();
        }

        let domain = normalize_domain(&event.domain);
        let record = QueryRecord {
            timestamp: rfc3339_millis(wall_millis),
            client_ip: event.client_ip.trim().to_string(),
            domain,
            qtype: qtype_name(event.qtype),
            status: rcode_name(event.status),
            elapsed_ms: event.elapsed_ms,
            answers: summarize_answers(&event.answers),
            is_blocked: event.is_blocked,
            is_cached: event.is_cached,
            upstream: event.upstream.clone(),
            rule: event.rule.clone(),
        };

        self.ring.push_back(record);
        while self.ring.len() > self.capacity {
            self.ring.pop_front();
        }
    }

    fn bump_qps(&mut self, mono: u64) {
        if self.window == 0 || self.qps.is_empty() {
            return;
        }
        let index = (mono % self.window) as usize;
        let slot = &mut self.qps[index];
        if slot.sec != mono {
            *slot = QpsSlot {
                sec: mono,
                count: 0,
            };
        }
        slot.count = slot.count.saturating_add(1);
    }

    fn bump_history(&mut self, wall_millis: i64, mono: u64, event: &QueryEvent) {
        let wall_secs = wall_millis.div_euclid(1000);
        let width = self.history_bucket_secs as i64;
        let time = wall_secs - wall_secs.rem_euclid(width);
        let index = mono / self.history_bucket_secs;
        // Both the monotonic index and the displayed wall-time bucket must
        // match; this keeps restored (or clock-skewed) buckets from absorbing
        // fresh events.
        let same_bucket = self
            .history
            .back()
            .is_some_and(|bucket| bucket.mono_index == index && bucket.time == time);
        if !same_bucket {
            self.history.push_back(HistoryBucket {
                mono_index: index,
                time,
                total: 0,
                blocked: 0,
                cached: 0,
            });
            while self.history.len() > HISTORY_CAPACITY {
                self.history.pop_front();
            }
        }

        if let Some(bucket) = self.history.back_mut() {
            bucket.total = bucket.total.saturating_add(1);
            if event.is_blocked {
                bucket.blocked = bucket.blocked.saturating_add(1);
            }
            if event.is_cached {
                bucket.cached = bucket.cached.saturating_add(1);
            }
        }
    }

    fn bump_top(&mut self, event: &QueryEvent) {
        let domain = normalize_domain(&event.domain);
        TopMaps::bump(&mut self.top.domains, &domain);
        if event.is_blocked {
            TopMaps::bump(&mut self.top.blocked, &domain);
        }
        let client = event.client_ip.trim();
        if !client.is_empty() {
            TopMaps::bump(&mut self.top.clients, client);
        }
    }

    fn qps(&self, now: u64) -> f64 {
        let sum: u64 = self
            .qps
            .iter()
            .filter(|slot| slot.sec <= now && now - slot.sec < self.window)
            .map(|slot| slot.count)
            .sum();
        sum as f64 / self.window as f64
    }

    fn history_points(&self, points: usize) -> Vec<HistoryPoint> {
        let take = points.min(self.history.len());
        self.history
            .iter()
            .skip(self.history.len() - take)
            .map(|bucket| HistoryPoint {
                time: bucket.time,
                total: bucket.total,
                blocked: bucket.blocked,
                cached: bucket.cached,
            })
            .collect()
    }

    fn top_domains(&self, limit: usize) -> Vec<TopDomainEntry> {
        let mut entries: Vec<TopDomainEntry> = self
            .top
            .domains
            .iter()
            .map(|(domain, count)| TopDomainEntry {
                domain: domain.clone(),
                count: *count,
            })
            .collect();
        sort_domain_entries(&mut entries);
        entries.truncate(limit);
        entries
    }

    fn top_blocked(&self, limit: usize) -> Vec<TopDomainEntry> {
        let mut entries: Vec<TopDomainEntry> = self
            .top
            .blocked
            .iter()
            .map(|(domain, count)| TopDomainEntry {
                domain: domain.clone(),
                count: *count,
            })
            .collect();
        sort_domain_entries(&mut entries);
        entries.truncate(limit);
        entries
    }

    fn top_clients(&self, limit: usize) -> Vec<TopClientEntry> {
        let mut entries: Vec<TopClientEntry> = self
            .top
            .clients
            .iter()
            .map(|(client_ip, count)| TopClientEntry {
                client_ip: client_ip.clone(),
                count: *count,
            })
            .collect();
        entries.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| a.client_ip.cmp(&b.client_ip))
        });
        entries.truncate(limit);
        entries
    }

    fn logs(&self, query: &LogQuery) -> LogsResponse {
        let needle = query.search.to_ascii_lowercase();
        let matches = |record: &QueryRecord| {
            query.filter.matches(record) && (needle.is_empty() || record.domain.contains(&needle))
        };

        let total = self.ring.iter().filter(|record| matches(record)).count();
        let items = self
            .ring
            .iter()
            .rev()
            .filter(|record| matches(record))
            .skip(query.offset)
            .take(query.limit)
            .cloned()
            .collect();

        LogsResponse { total, items }
    }
}

fn sort_domain_entries(entries: &mut [TopDomainEntry]) {
    entries.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.domain.cmp(&b.domain)));
}

fn lock_store(store: &Mutex<Store>) -> MutexGuard<'_, Store> {
    store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

enum Message {
    Event { wall_millis: i64, event: QueryEvent },
    Flush(oneshot::Sender<()>),
}

struct Inner {
    tx: mpsc::Sender<Message>,
    counters: Arc<Counters>,
    store: Arc<Mutex<Store>>,
}

/// Thread-safe statistics collector.
pub struct StatsCollector {
    config: StatsConfig,
    clock: Arc<dyn Clock>,
    started_mono: u64,
    inner: Option<Inner>,
}

impl StatsCollector {
    /// Start a collector with the production clock.
    ///
    /// Must be called from within a Tokio runtime; the aggregator runs as a
    /// background task.
    pub fn start(config: StatsConfig) -> Arc<Self> {
        Self::start_with_clock(config, Arc::new(SystemClock::new()))
    }

    /// Start a collector with an injected clock (used by tests).
    ///
    /// Must be called from within a Tokio runtime.
    pub fn start_with_clock(config: StatsConfig, clock: Arc<dyn Clock>) -> Arc<Self> {
        let config = config.sanitized();
        let started_mono = clock.monotonic_secs();

        if !config.enabled {
            return Arc::new(Self {
                config,
                clock,
                started_mono,
                inner: None,
            });
        }

        let (tx, rx) = mpsc::channel(config.channel_capacity);
        let counters = Arc::new(Counters::default());
        let store = Arc::new(Mutex::new(Store::new(&config)));

        tokio::spawn(run_aggregator(rx, Arc::clone(&store), Arc::clone(&clock)));

        let collector = Arc::new(Self {
            config: config.clone(),
            clock,
            started_mono,
            inner: Some(Inner {
                tx,
                counters,
                store,
            }),
        });

        if let Some(path) = config.dump_path.clone() {
            let weak = Arc::downgrade(&collector);
            let interval = Duration::from_secs(config.dump_interval_secs.max(1));
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.tick().await; // consume the immediate first tick
                loop {
                    ticker.tick().await;
                    let Some(collector) = weak.upgrade() else {
                        break;
                    };
                    let _ = collector.dump(&path).await;
                }
            });
        }

        collector
    }

    /// Collector configuration after clamping.
    pub fn config(&self) -> &StatsConfig {
        &self.config
    }

    /// Whether event aggregation is enabled.
    pub fn enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Seconds since the collector was started.
    pub fn uptime_secs(&self) -> u64 {
        self.clock
            .monotonic_secs()
            .saturating_sub(self.started_mono)
    }

    /// Number of events dropped because the bounded channel was full.
    pub fn dropped_events(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |inner| inner.counters.dropped.load(Ordering::Relaxed))
    }

    /// Hot-path entry point. Never blocks, locks or awaits.
    pub fn record(&self, event: QueryEvent) {
        let Some(inner) = &self.inner else {
            return;
        };

        inner.counters.total.fetch_add(1, Ordering::Relaxed);
        if event.is_blocked {
            inner.counters.blocked.fetch_add(1, Ordering::Relaxed);
        }
        if event.is_cached {
            inner.counters.cached.fetch_add(1, Ordering::Relaxed);
        }
        inner
            .counters
            .latency_us
            .fetch_add(event.elapsed_ms.saturating_mul(1000), Ordering::Relaxed);

        let message = Message::Event {
            wall_millis: self.clock.wall_millis(),
            event,
        };
        if inner.tx.try_send(message).is_err() {
            inner.counters.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Wait until every event sent before this call has been aggregated.
    pub async fn flush(&self) {
        let Some(inner) = &self.inner else {
            return;
        };
        let (ack_tx, ack_rx) = oneshot::channel();
        if inner.tx.send(Message::Flush(ack_tx)).await.is_ok() {
            let _ = ack_rx.await;
        }
    }

    /// `/api/v1/health` payload.
    pub fn health(&self) -> HealthResponse {
        HealthResponse {
            status: "ok".to_string(),
            version: crate::crate_version().to_string(),
            uptime_s: self.uptime_secs(),
        }
    }

    /// `/api/v1/stats` payload.
    pub fn stats(&self) -> StatsResponse {
        let (total, blocked, cached, latency_us) = match &self.inner {
            Some(inner) => (
                inner.counters.total.load(Ordering::Relaxed),
                inner.counters.blocked.load(Ordering::Relaxed),
                inner.counters.cached.load(Ordering::Relaxed),
                inner.counters.latency_us.load(Ordering::Relaxed),
            ),
            None => (0, 0, 0, 0),
        };

        let qps = match &self.inner {
            Some(inner) => {
                let store = lock_store(&inner.store);
                store.qps(self.clock.monotonic_secs())
            }
            None => 0.0,
        };

        StatsResponse {
            total_queries: total,
            blocked_queries: blocked,
            cached_queries: cached,
            blocked_percentage: percentage(blocked, total),
            cached_percentage: percentage(cached, total),
            avg_latency_ms: average_latency_ms(latency_us, total),
            qps: round2(qps),
        }
    }

    /// `/api/v1/history` payload with at most `points` buckets.
    pub fn history(&self, points: usize) -> Vec<HistoryPoint> {
        let Some(inner) = &self.inner else {
            return Vec::new();
        };
        let points = points.min(self.config.history_max);
        let store = lock_store(&inner.store);
        store.history_points(points)
    }

    /// `/api/v1/top` payload, sorted by `count desc, name asc`.
    pub fn top(&self, limit: usize) -> TopResponse {
        let Some(inner) = &self.inner else {
            return TopResponse {
                top_domains: Vec::new(),
                top_blocked: Vec::new(),
                top_clients: Vec::new(),
            };
        };
        let limit = limit.min(self.config.top_max);
        let store = lock_store(&inner.store);
        TopResponse {
            top_domains: store.top_domains(limit),
            top_blocked: store.top_blocked(limit),
            top_clients: store.top_clients(limit),
        }
    }

    /// `/api/v1/logs` payload.
    pub fn logs(&self, query: &LogQuery) -> LogsResponse {
        let Some(inner) = &self.inner else {
            return LogsResponse {
                total: 0,
                items: Vec::new(),
            };
        };
        let mut query = query.clone();
        query.limit = query.limit.min(self.config.logs_max);
        let store = lock_store(&inner.store);
        store.logs(&query)
    }

    /// Clear the query-log ring without touching stats, history or top.
    pub fn clear_logs(&self) {
        if let Some(inner) = &self.inner {
            lock_store(&inner.store).ring.clear();
        }
    }

    /// Serialize the current state into a dump envelope.
    pub fn dump_data(&self) -> DumpData {
        let saved_at = self.clock.wall_millis().div_euclid(1000);
        let Some(inner) = &self.inner else {
            return DumpData::empty(saved_at, self.config.capacity);
        };

        let counters = DumpCounters {
            total_queries: inner.counters.total.load(Ordering::Relaxed),
            blocked_queries: inner.counters.blocked.load(Ordering::Relaxed),
            cached_queries: inner.counters.cached.load(Ordering::Relaxed),
            latency_us_sum: inner.counters.latency_us.load(Ordering::Relaxed),
        };

        let store = lock_store(&inner.store);
        DumpData {
            magic: DUMP_MAGIC.to_string(),
            version: DUMP_VERSION,
            saved_at,
            capacity: store.capacity,
            counters,
            records: store.ring.iter().cloned().collect(),
            history: store
                .history
                .iter()
                .map(|bucket| HistoryPoint {
                    time: bucket.time,
                    total: bucket.total,
                    blocked: bucket.blocked,
                    cached: bucket.cached,
                })
                .collect(),
            top: DumpTop {
                domains: store
                    .top
                    .domains
                    .iter()
                    .map(|(k, v)| (k.clone(), *v))
                    .collect(),
                blocked: store
                    .top
                    .blocked
                    .iter()
                    .map(|(k, v)| (k.clone(), *v))
                    .collect(),
                clients: store
                    .top
                    .clients
                    .iter()
                    .map(|(k, v)| (k.clone(), *v))
                    .collect(),
            },
        }
    }

    /// Write a dump atomically through `spawn_blocking`.
    pub async fn dump(&self, path: &Path) -> std::io::Result<()> {
        let data = self.dump_data();
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || write_dump(&path, &data))
            .await
            .map_err(std::io::Error::other)?
    }

    /// Flush pending events and write a final dump when configured.
    ///
    /// Call this from the service's SIGTERM/SIGINT handler before shutdown.
    pub async fn shutdown(&self) {
        self.flush().await;
        if let Some(path) = &self.config.dump_path {
            let _ = self.dump(path).await;
        }
    }

    /// Load a dump, quarantining a corrupt file.
    ///
    /// QPS is intentionally not restored; history/top/records are truncated to
    /// the current capacity.
    pub fn load(&self, path: &Path) -> std::io::Result<LoadOutcome> {
        let outcome = read_dump(path);
        match outcome {
            LoadOutcome::Missing => Ok(LoadOutcome::Missing),
            LoadOutcome::Corrupt => {
                let stamp = self.clock.wall_millis().div_euclid(1000);
                let _ = std::fs::rename(path, corrupt_path(path, stamp));
                Ok(LoadOutcome::Corrupt)
            }
            LoadOutcome::Loaded(data) => {
                self.apply_dump(&data);
                Ok(LoadOutcome::Loaded(data))
            }
        }
    }

    fn apply_dump(&self, data: &DumpData) {
        let Some(inner) = &self.inner else {
            return;
        };

        inner
            .counters
            .total
            .store(data.counters.total_queries, Ordering::Relaxed);
        inner
            .counters
            .blocked
            .store(data.counters.blocked_queries, Ordering::Relaxed);
        inner
            .counters
            .cached
            .store(data.counters.cached_queries, Ordering::Relaxed);
        inner
            .counters
            .latency_us
            .store(data.counters.latency_us_sum, Ordering::Relaxed);

        let mono = self.clock.monotonic_secs();
        let mut store = lock_store(&inner.store);

        store.ring.clear();
        let skip = data.records.len().saturating_sub(store.capacity);
        store.ring.extend(data.records.iter().skip(skip).cloned());

        store.history.clear();
        let len = data.history.len().min(HISTORY_CAPACITY);
        let start = data.history.len() - len;
        let current_index = mono / store.history_bucket_secs;
        for (offset, point) in data.history[start..].iter().enumerate() {
            // Loaded buckets get monotonic indexes before the current bucket.
            // Fresh events additionally require a matching wall bucket start,
            // so restored data never absorbs new counts.
            let backlog = (len - offset) as u64;
            store.history.push_back(HistoryBucket {
                mono_index: current_index.saturating_sub(backlog),
                time: point.time,
                total: point.total,
                blocked: point.blocked,
                cached: point.cached,
            });
        }

        store.top = TopMaps {
            domains: load_top(&data.top.domains),
            blocked: load_top(&data.top.blocked),
            clients: load_top(&data.top.clients),
        };
        store.qps = (0..store.window)
            .map(|_| QpsSlot {
                sec: mono,
                count: 0,
            })
            .collect();
        store.last_prune_mono = mono;
    }
}

async fn run_aggregator(
    mut rx: mpsc::Receiver<Message>,
    store: Arc<Mutex<Store>>,
    clock: Arc<dyn Clock>,
) {
    while let Some(message) = rx.recv().await {
        match message {
            Message::Event { wall_millis, event } => {
                let mono = clock.monotonic_secs();
                lock_store(&store).on_event(wall_millis, mono, &event);
            }
            Message::Flush(ack) => {
                let _ = ack.send(());
            }
        }
    }
}

fn load_top(source: &BTreeMap<String, u64>) -> HashMap<String, u64> {
    let mut map: HashMap<String, u64> = source
        .iter()
        .map(|(key, count)| (key.clone(), *count))
        .collect();
    if map.len() > TOP_HARD_CAP {
        let mut entries: Vec<(String, u64)> = map.into_iter().collect();
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        entries.truncate(TOP_HARD_CAP);
        map = entries.into_iter().collect();
    }
    map
}

fn percentage(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        round2((part as f64 / total as f64) * 100.0)
    }
}

fn average_latency_ms(latency_us: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        round2(latency_us as f64 / total as f64 / 1000.0)
    }
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}
