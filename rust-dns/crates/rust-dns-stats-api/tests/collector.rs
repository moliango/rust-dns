// SPDX-License-Identifier: GPL-3.0-only
//! Collector unit/behavior tests: stats, QPS, history, top, logs, dump/load.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rust_dns_stats_api::{
    LoadOutcome, LogFilter, LogQuery, ManualClock, QueryEvent, RawAnswer, StatsCollector,
    StatsConfig, TopResponse,
};

const WALL: i64 = 1_763_900_000;

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(WALL, 0))
}

fn start(config: StatsConfig, clock: &Arc<ManualClock>) -> Arc<StatsCollector> {
    StatsCollector::start_with_clock(config, clock.clone())
}

fn event(domain: &str) -> QueryEvent {
    QueryEvent {
        client_ip: "192.168.1.10".to_string(),
        domain: domain.to_string(),
        qtype: 1,
        status: 0,
        elapsed_ms: 1,
        answers: Vec::new(),
        is_blocked: false,
        is_cached: false,
        upstream: String::new(),
        rule: String::new(),
    }
}

fn fixture_events() -> Vec<QueryEvent> {
    [
        include_str!("fixtures/records/normal.json"),
        include_str!("fixtures/records/blocked.json"),
        include_str!("fixtures/records/cached.json"),
    ]
    .iter()
    .map(|json| serde_json::from_str(json).unwrap())
    .collect()
}

fn default_logs(collector: &StatsCollector) -> rust_dns_stats_api::LogsResponse {
    collector.logs(&LogQuery {
        limit: 20,
        offset: 0,
        search: String::new(),
        filter: LogFilter::All,
    })
}

fn unique_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("rust-dns-stats-api-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn empty_stats_are_zeroed() {
    let collector = start(StatsConfig::default(), &clock());

    let stats = collector.stats();
    assert_eq!(stats.total_queries, 0);
    assert_eq!(stats.blocked_queries, 0);
    assert_eq!(stats.cached_queries, 0);
    assert_eq!(stats.blocked_percentage, 0.0);
    assert_eq!(stats.cached_percentage, 0.0);
    assert_eq!(stats.avg_latency_ms, 0.0);
    assert_eq!(stats.qps, 0.0);

    assert!(collector.history(24).is_empty());
    assert_eq!(
        collector.top(10),
        TopResponse {
            top_domains: Vec::new(),
            top_blocked: Vec::new(),
            top_clients: Vec::new(),
        }
    );
    assert_eq!(default_logs(&collector).total, 0);
}

#[tokio::test]
async fn fixture_response_shapes() {
    let collector = start(StatsConfig::default(), &clock());
    for event in fixture_events() {
        collector.record(event);
    }
    collector.flush().await;

    let stats: rust_dns_stats_api::StatsResponse =
        serde_json::from_str(include_str!("fixtures/api/stats.json")).unwrap();
    assert_eq!(collector.stats(), stats);

    let history: rust_dns_stats_api::HistoryResponse =
        serde_json::from_str(include_str!("fixtures/api/history.json")).unwrap();
    assert_eq!(collector.history(24), history.points);

    let top: rust_dns_stats_api::TopResponse =
        serde_json::from_str(include_str!("fixtures/api/top.json")).unwrap();
    assert_eq!(collector.top(10), top);

    let logs: rust_dns_stats_api::LogsResponse =
        serde_json::from_str(include_str!("fixtures/api/logs.json")).unwrap();
    assert_eq!(default_logs(&collector), logs);
}

#[tokio::test]
async fn record_fixtures_normalize_fields() {
    let collector = start(StatsConfig::default(), &clock());
    for json in [
        include_str!("fixtures/records/normal.json"),
        include_str!("fixtures/records/cached.json"),
        include_str!("fixtures/records/blocked.json"),
        include_str!("fixtures/records/nxdomain.json"),
        include_str!("fixtures/records/no_answers.json"),
        include_str!("fixtures/records/long_domain.json"),
    ] {
        let event: QueryEvent = serde_json::from_str(json).unwrap();
        collector.record(event);
    }
    collector.flush().await;

    let logs = default_logs(&collector);
    assert_eq!(logs.total, 6);
    let by_domain = |domain: &str| {
        logs.items
            .iter()
            .find(|record| record.domain == domain)
            .unwrap()
    };

    let normal = by_domain("example.com");
    assert_eq!(normal.qtype, "A");
    assert_eq!(normal.status, "NOERROR");
    assert_eq!(normal.answers.len(), 1);
    assert_eq!(normal.answers[0].record_type, "A");
    assert_eq!(normal.upstream, "forward_remote");
    assert_eq!(normal.rule, "");

    let nxdomain = by_domain("missing.example");
    assert_eq!(nxdomain.qtype, "HTTPS");
    assert_eq!(nxdomain.status, "NXDOMAIN");
    assert!(nxdomain.answers.is_empty());

    let no_answers = by_domain("empty.example");
    assert!(no_answers.answers.is_empty());
    assert_eq!(no_answers.upstream, "");
    assert_eq!(no_answers.rule, "");

    let long = logs
        .items
        .iter()
        .find(|record| record.domain.starts_with("aaa"))
        .unwrap();
    assert_eq!(long.domain.len(), 253);
    assert_eq!(long.answers[0].data.len(), 512);
}

#[tokio::test]
async fn ring_buffer_is_fifo() {
    let config = StatsConfig {
        capacity: 100,
        ..StatsConfig::default()
    };
    let collector = start(config, &clock());
    for index in 0..150 {
        collector.record(event(&format!("d{index:03}")));
    }
    collector.flush().await;

    let logs = collector.logs(&LogQuery {
        limit: 200,
        offset: 0,
        search: String::new(),
        filter: LogFilter::All,
    });
    assert_eq!(logs.total, 100);
    assert_eq!(logs.items[0].domain, "d149");
    assert_eq!(logs.items[99].domain, "d050");

    let page = collector.logs(&LogQuery {
        limit: 10,
        offset: 90,
        search: String::new(),
        filter: LogFilter::All,
    });
    assert_eq!(page.total, 100);
    assert_eq!(page.items.len(), 10);
    assert_eq!(page.items[0].domain, "d059");

    let beyond = collector.logs(&LogQuery {
        limit: 10,
        offset: 100,
        search: String::new(),
        filter: LogFilter::All,
    });
    assert_eq!(beyond.total, 100);
    assert!(beyond.items.is_empty());
}

#[tokio::test]
async fn logs_search_and_filter() {
    let collector = start(StatsConfig::default(), &clock());
    let mut blocked = event("Ads.Example.COM.");
    blocked.is_blocked = true;
    let mut cached = event("cached.example");
    cached.is_cached = true;
    collector.record(event("normal.example"));
    collector.record(blocked);
    collector.record(cached);
    collector.flush().await;

    let query = |search: &str, filter| LogQuery {
        limit: 20,
        offset: 0,
        search: search.to_string(),
        filter,
    };

    assert_eq!(collector.logs(&query("EXAMPLE", LogFilter::All)).total, 3);
    assert_eq!(collector.logs(&query("ads", LogFilter::All)).total, 1);
    assert_eq!(collector.logs(&query("", LogFilter::Blocked)).total, 1);
    assert_eq!(collector.logs(&query("", LogFilter::Cached)).total, 1);
    assert_eq!(
        collector.logs(&query("nomatch", LogFilter::Cached)).total,
        0
    );
}

#[tokio::test]
async fn clear_logs_keeps_aggregates() {
    let collector = start(StatsConfig::default(), &clock());
    for event in fixture_events() {
        collector.record(event);
    }
    collector.flush().await;

    collector.clear_logs();
    assert_eq!(default_logs(&collector).total, 0);
    assert_eq!(collector.stats().total_queries, 3);
    assert_eq!(collector.history(24).len(), 1);
    assert_eq!(collector.top(10).top_domains.len(), 3);
}

#[tokio::test]
async fn qps_uses_a_sliding_window() {
    let clock = clock();
    let collector = start(StatsConfig::default(), &clock);

    for _ in 0..5 {
        collector.record(event("qps.example"));
    }
    collector.flush().await;
    assert_eq!(collector.stats().qps, 0.5);

    clock.advance_secs(4);
    for _ in 0..5 {
        collector.record(event("qps.example"));
    }
    collector.flush().await;
    assert_eq!(collector.stats().qps, 1.0);

    clock.advance_secs(10);
    assert_eq!(collector.stats().qps, 0.0);

    for _ in 0..5 {
        collector.record(event("qps.example"));
    }
    collector.flush().await;
    assert_eq!(collector.stats().qps, 0.5);

    clock.advance_secs(10);
    assert_eq!(collector.stats().qps, 0.0);
}

#[tokio::test]
async fn history_rolls_buckets_without_zero_fill() {
    let clock = clock();
    let config = StatsConfig {
        history_bucket_secs: 60,
        ..StatsConfig::default()
    };
    let collector = start(config, &clock);

    for _ in 0..5 {
        collector.record(event("h.example"));
    }
    collector.flush().await;

    clock.advance_secs(60);
    for _ in 0..3 {
        collector.record(event("h.example"));
    }
    collector.flush().await;

    clock.advance_secs(120);
    for _ in 0..2 {
        collector.record(event("h.example"));
    }
    collector.flush().await;

    let points = collector.history(24);
    assert_eq!(points.len(), 3);
    assert_eq!(points[0].total, 5);
    assert_eq!(points[1].total, 3);
    assert_eq!(points[2].total, 2);
    assert_eq!(points[1].time - points[0].time, 60);
    assert_eq!(points[2].time - points[1].time, 120);

    let last = collector.history(2);
    assert_eq!(last, points[1..].to_vec());
    assert!(collector.history(0).is_empty());
}

#[tokio::test]
async fn top_sorts_count_desc_then_name_asc() {
    let collector = start(StatsConfig::default(), &clock());
    for domain in [
        "a.example",
        "c.example",
        "a.example",
        "c.example",
        "b.example",
    ] {
        collector.record(event(domain));
    }
    let mut blocked_a = event("z.example");
    blocked_a.is_blocked = true;
    let mut blocked_b = event("a.example");
    blocked_b.is_blocked = true;
    collector.record(blocked_a);
    collector.record(blocked_b);
    collector.flush().await;

    let top = collector.top(10);
    assert_eq!(
        top.top_domains
            .iter()
            .map(|entry| (entry.domain.as_str(), entry.count))
            .collect::<Vec<_>>(),
        vec![
            ("a.example", 3),
            ("c.example", 2),
            ("b.example", 1),
            ("z.example", 1)
        ]
    );
    assert_eq!(
        top.top_blocked
            .iter()
            .map(|entry| (entry.domain.as_str(), entry.count))
            .collect::<Vec<_>>(),
        vec![("a.example", 1), ("z.example", 1)]
    );
}

#[tokio::test]
async fn thousand_event_qps_history_top() {
    let collector = start(StatsConfig::default(), &clock());
    for index in 0..1000u64 {
        let mut event = event("load.example");
        event.client_ip = if index % 2 == 0 {
            "192.168.1.10".to_string()
        } else {
            "192.168.1.11".to_string()
        };
        event.is_blocked = index % 10 == 0;
        event.is_cached = index % 3 == 0;
        event.elapsed_ms = 1;
        collector.record(event);
    }
    collector.flush().await;

    let stats = collector.stats();
    assert_eq!(stats.total_queries, 1000);
    assert_eq!(stats.blocked_queries, 100);
    assert_eq!(stats.cached_queries, 334);
    assert_eq!(stats.blocked_percentage, 10.0);
    assert_eq!(stats.cached_percentage, 33.4);
    assert_eq!(stats.avg_latency_ms, 1.0);
    assert_eq!(stats.qps, 100.0);

    let history = collector.history(24);
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].total, 1000);
    assert_eq!(history[0].blocked, 100);
    assert_eq!(history[0].cached, 334);

    let top = collector.top(10);
    assert_eq!(top.top_domains[0].domain, "load.example");
    assert_eq!(top.top_domains[0].count, 1000);
    assert_eq!(top.top_blocked[0].count, 100);
    assert_eq!(
        top.top_clients
            .iter()
            .map(|entry| (entry.client_ip.as_str(), entry.count))
            .collect::<Vec<_>>(),
        vec![("192.168.1.10", 500), ("192.168.1.11", 500)]
    );
}

#[tokio::test]
async fn dump_and_load_round_trip() {
    let dir = unique_dir();
    let path = dir.join("stats.dump");
    let config = StatsConfig {
        capacity: 100,
        ..StatsConfig::default()
    };

    let clock = clock();
    let collector = start(config.clone(), &clock);
    for index in 0..150 {
        collector.record(event(&format!("d{index:03}")));
    }
    collector.flush().await;
    collector.dump(&path).await.unwrap();

    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(raw["magic"], "RDNSS");
    assert_eq!(raw["version"], 1);

    let restored = start(config.clone(), &clock);
    let outcome = restored.load(&path).unwrap();
    assert!(matches!(outcome, LoadOutcome::Loaded(_)));

    assert_eq!(restored.stats().total_queries, 150);
    assert_eq!(restored.stats().qps, 0.0);
    let logs = default_logs(&restored);
    assert_eq!(logs.total, 100);
    assert_eq!(logs.items[0].domain, "d149");
    let history = restored.history(24);
    assert_eq!(history[0].total, 150);
    let top = restored.top(10);
    assert_eq!(top.top_domains.len(), 10);
    assert_eq!(top.top_domains[0].domain, "d000");
    assert_eq!(top.top_domains[0].count, 1);

    // A fresh event after a clock jump starts a new bucket instead of
    // merging into the restored bucket.
    clock.advance_secs(3600);
    restored.record(event("new.example"));
    restored.flush().await;
    let history = restored.history(24);
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].total, 150);
    assert_eq!(history[1].total, 1);
    assert!(history[1].time > history[0].time);

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn corrupt_dump_is_quarantined() {
    let dir = unique_dir();
    let path = dir.join("bad.dump");
    std::fs::write(&path, b"not a dump").unwrap();

    let collector = start(StatsConfig::default(), &clock());
    let outcome = collector.load(&path).unwrap();
    assert!(matches!(outcome, LoadOutcome::Corrupt));
    assert_eq!(default_logs(&collector).total, 0);

    let quarantined: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".corrupt."))
        .collect();
    assert_eq!(quarantined.len(), 1);

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn shutdown_writes_configured_dump() {
    let dir = unique_dir();
    let path = dir.join("shutdown.dump");
    let config = StatsConfig {
        dump_path: Some(path.clone()),
        ..StatsConfig::default()
    };
    let collector = start(config, &clock());
    collector.record(event("shutdown.example"));
    collector.shutdown().await;

    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(raw["magic"], "RDNSS");
    assert_eq!(raw["counters"]["total_queries"], 1);

    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn missing_dump_is_not_an_error() {
    let dir = unique_dir();
    let collector = start(StatsConfig::default(), &clock());
    let outcome = collector.load(&dir.join("absent.dump")).unwrap();
    assert!(matches!(outcome, LoadOutcome::Missing));
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn disabled_collector_serves_empty_state() {
    let config = StatsConfig {
        enabled: false,
        ..StatsConfig::default()
    };
    let collector = start(config, &clock());
    collector.record(event("ignored.example"));
    collector.flush().await;

    assert!(!collector.enabled());
    assert_eq!(collector.stats().total_queries, 0);
    assert_eq!(collector.stats().qps, 0.0);
    assert!(collector.history(24).is_empty());
    assert!(default_logs(&collector).items.is_empty());
    assert_eq!(collector.dropped_events(), 0);
}

#[tokio::test]
async fn answers_are_truncated_and_mapped() {
    let collector = start(StatsConfig::default(), &clock());
    let answers = (0..12u16)
        .map(|index| RawAnswer {
            record_type: 1,
            data: format!("192.0.2.{index}"),
            ttl: 60,
        })
        .collect();
    collector.record(QueryEvent {
        answers,
        ..event("answers.example")
    });
    collector.flush().await;

    let logs = default_logs(&collector);
    assert_eq!(logs.items[0].answers.len(), 8);
    assert_eq!(logs.items[0].answers[0].record_type, "A");
}
