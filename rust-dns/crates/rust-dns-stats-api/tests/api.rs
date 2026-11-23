// SPDX-License-Identifier: GPL-3.0-only
//! End-to-end HTTP contract tests over a real ephemeral TCP listener.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rust_dns_stats_api::{
    CacheFlush, LogsResponse, ManualClock, QueryEvent, StatsCollector, StatsConfig,
    serve_with_listener,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const WALL: i64 = 1_763_900_000;

#[derive(Clone)]
struct FixtureData;

impl FixtureData {
    fn events() -> Vec<QueryEvent> {
        [
            include_str!("fixtures/records/normal.json"),
            include_str!("fixtures/records/blocked.json"),
            include_str!("fixtures/records/cached.json"),
        ]
        .iter()
        .map(|json| serde_json::from_str(json).unwrap())
        .collect()
    }
}

struct FailingCache;

#[async_trait]
impl CacheFlush for FailingCache {
    async fn flush(&self) -> Result<(), String> {
        Err("boom".to_string())
    }
}

struct OkCache;

#[async_trait]
impl CacheFlush for OkCache {
    async fn flush(&self) -> Result<(), String> {
        Ok(())
    }
}

async fn spawn_api(
    collector: Arc<StatsCollector>,
    cache: Option<Arc<dyn CacheFlush>>,
) -> (
    SocketAddr,
    oneshot::Sender<()>,
    JoinHandle<std::io::Result<()>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        serve_with_listener(listener, collector, cache, async move {
            let _ = rx.await;
        })
        .await
    });
    (addr, tx, handle)
}

async fn request(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, String, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut bytes = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut bytes)).await;
    assert!(read.is_ok(), "response timed out");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .expect("status line")
        .parse()
        .unwrap();
    (status, head.to_string(), body.to_string())
}

fn assert_json(actual: &str, expected: &str) {
    let actual: Value = serde_json::from_str(actual).expect("actual JSON");
    let expected: Value = serde_json::from_str(expected).expect("expected JSON");
    assert_eq!(actual, expected, "actual body: {actual}");
}

fn json_body<T: serde::de::DeserializeOwned>(body: &str) -> T {
    serde_json::from_str(body).expect("typed response")
}

fn fixture_collector() -> Arc<StatsCollector> {
    let clock = Arc::new(ManualClock::new(WALL, 0));
    let collector = StatsCollector::start_with_clock(StatsConfig::default(), clock);
    for event in FixtureData::events() {
        collector.record(event);
    }
    collector
}

async fn stop(tx: oneshot::Sender<()>, handle: JoinHandle<std::io::Result<()>>) {
    tx.send(()).ok();
    let _ = handle.await;
}

#[tokio::test]
async fn contract_responses_match_fixtures() {
    let collector = fixture_collector();
    collector.flush().await;
    let (addr, tx, handle) = spawn_api(collector, None).await;

    let (status, head, body) = request(addr, "GET", "/api/v1/health", "").await;
    assert_eq!(status, 200);
    let lower = head.to_ascii_lowercase();
    assert!(lower.contains("content-type: application/json; charset=utf-8"));
    assert!(lower.contains("cache-control: no-store"));
    assert_json(&body, include_str!("fixtures/api/health.json"));

    let (status, _, body) = request(addr, "GET", "/api/v1/version", "").await;
    assert_eq!(status, 200);
    assert_json(&body, include_str!("fixtures/api/version.json"));

    let (status, _, body) = request(addr, "GET", "/api/v1/stats", "").await;
    assert_eq!(status, 200);
    assert_json(&body, include_str!("fixtures/api/stats.json"));

    let (status, _, body) = request(addr, "GET", "/api/v1/history", "").await;
    assert_eq!(status, 200);
    assert_json(&body, include_str!("fixtures/api/history.json"));

    let (status, _, body) = request(addr, "GET", "/api/v1/top", "").await;
    assert_eq!(status, 200);
    assert_json(&body, include_str!("fixtures/api/top.json"));

    let (status, _, body) = request(addr, "GET", "/api/v1/logs", "").await;
    assert_eq!(status, 200);
    assert_json(&body, include_str!("fixtures/api/logs.json"));

    stop(tx, handle).await;
}

#[tokio::test]
async fn defaults_pagination_and_unknown_params() {
    let collector = fixture_collector();
    collector.flush().await;
    let (addr, tx, handle) = spawn_api(collector, None).await;

    let (status, _, body) = request(addr, "GET", "/api/v1/logs?limit=1&offset=1", "").await;
    assert_eq!(status, 200);
    let logs: LogsResponse = json_body(&body);
    assert_eq!(logs.total, 3);
    assert_eq!(logs.items.len(), 1);
    assert_eq!(logs.items[0].domain, "ads.example");

    let (status, _, body) =
        request(addr, "GET", "/api/v1/logs?search=CACHED&filter=cached", "").await;
    assert_eq!(status, 200);
    let logs: LogsResponse = json_body(&body);
    assert_eq!(logs.total, 1);
    assert_eq!(logs.items[0].domain, "cached.example");

    let (status, _, body) = request(addr, "GET", "/api/v1/logs?filter=blocked", "").await;
    assert_eq!(status, 200);
    let logs: LogsResponse = json_body(&body);
    assert_eq!(logs.total, 1);
    assert_eq!(logs.items[0].domain, "ads.example");

    // Unknown parameters are ignored for forward compatibility.
    let (status, _, _) = request(addr, "GET", "/api/v1/top?limit=1&unknown=1", "").await;
    assert_eq!(status, 200);

    // Pagination beyond the retained set is an empty page with the true total.
    let (status, _, body) = request(addr, "GET", "/api/v1/logs?limit=20&offset=99", "").await;
    assert_eq!(status, 200);
    let logs: LogsResponse = json_body(&body);
    assert_eq!(logs.total, 3);
    assert!(logs.items.is_empty());

    stop(tx, handle).await;
}

#[tokio::test]
async fn invalid_parameters_return_400() {
    let collector = fixture_collector();
    collector.flush().await;
    let (addr, tx, handle) = spawn_api(collector, None).await;

    for path in [
        "/api/v1/history?points=abc",
        "/api/v1/history?points=0",
        "/api/v1/history?points=169",
    ] {
        let (status, _, body) = request(addr, "GET", path, "").await;
        assert_eq!(status, 400, "path {path}");
        assert_json(&body, include_str!("fixtures/api/error_points.json"));
    }

    for path in ["/api/v1/top?limit=999", "/api/v1/top?limit=0"] {
        let (status, _, body) = request(addr, "GET", path, "").await;
        assert_eq!(status, 400, "path {path}");
        assert_json(&body, include_str!("fixtures/api/error_top_limit.json"));
    }

    for path in ["/api/v1/logs?limit=999", "/api/v1/logs?limit=0"] {
        let (status, _, body) = request(addr, "GET", path, "").await;
        assert_eq!(status, 400, "path {path}");
        assert_json(&body, include_str!("fixtures/api/error_logs_limit.json"));
    }

    let (status, _, body) = request(addr, "GET", "/api/v1/logs?offset=-1", "").await;
    assert_eq!(status, 400);
    assert_json(&body, include_str!("fixtures/api/error_offset.json"));

    let (status, _, body) = request(addr, "GET", "/api/v1/logs?filter=nope", "").await;
    assert_eq!(status, 400);
    assert_json(&body, include_str!("fixtures/api/error_filter.json"));

    let long_search = "a".repeat(129);
    let (status, _, body) = request(
        addr,
        "GET",
        &format!("/api/v1/logs?search={long_search}"),
        "",
    )
    .await;
    assert_eq!(status, 400);
    assert_json(&body, include_str!("fixtures/api/error_search.json"));

    stop(tx, handle).await;
}

#[tokio::test]
async fn unknown_path_and_method_errors() {
    let collector = fixture_collector();
    collector.flush().await;
    let (addr, tx, handle) = spawn_api(collector, None).await;

    let (status, head, body) = request(addr, "GET", "/api/v1/nope", "").await;
    assert_eq!(status, 404);
    assert!(head.to_ascii_lowercase().contains("application/json"));
    assert_json(&body, include_str!("fixtures/api/error_not_found.json"));

    let (status, _, body) = request(addr, "GET", "/api/v1/logs/clear", "").await;
    assert_eq!(status, 405);
    assert_json(
        &body,
        include_str!("fixtures/api/error_method_not_allowed.json"),
    );

    let (status, _, body) = request(addr, "DELETE", "/api/v1/stats", "").await;
    assert_eq!(status, 405);
    assert_json(
        &body,
        include_str!("fixtures/api/error_method_not_allowed.json"),
    );

    stop(tx, handle).await;
}

#[tokio::test]
async fn oversized_post_body_returns_413() {
    let collector = fixture_collector();
    collector.flush().await;
    let (addr, tx, handle) = spawn_api(collector, None).await;

    let body = "x".repeat(2048);
    let (status, _, response) = request(addr, "POST", "/api/v1/logs/clear", &body).await;
    assert_eq!(status, 413);
    assert_json(&response, include_str!("fixtures/api/error_payload.json"));

    stop(tx, handle).await;
}

#[tokio::test]
async fn clear_logs_does_not_reset_stats() {
    let collector = fixture_collector();
    collector.flush().await;
    let (addr, tx, handle) = spawn_api(collector, None).await;

    let (status, _, body) = request(addr, "POST", "/api/v1/logs/clear", "").await;
    assert_eq!(status, 200);
    assert_json(&body, include_str!("fixtures/api/success.json"));

    let (status, _, body) = request(addr, "GET", "/api/v1/logs", "").await;
    assert_eq!(status, 200);
    let logs: LogsResponse = json_body(&body);
    assert_eq!(logs.total, 0);
    assert!(logs.items.is_empty());

    let (status, _, body) = request(addr, "GET", "/api/v1/stats", "").await;
    assert_eq!(status, 200);
    let stats: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(stats["total_queries"], 3);

    stop(tx, handle).await;
}

#[tokio::test]
async fn cache_flush_success_noop_and_failure() {
    let collector = fixture_collector();
    collector.flush().await;
    let (addr, tx, handle) = spawn_api(collector, Some(Arc::new(OkCache))).await;

    let (status, _, body) = request(addr, "POST", "/api/v1/cache/flush", "").await;
    assert_eq!(status, 200);
    assert_json(&body, include_str!("fixtures/api/success.json"));
    stop(tx, handle).await;

    let collector = fixture_collector();
    collector.flush().await;
    let (addr, tx, handle) = spawn_api(collector, None).await;
    let (status, _, body) = request(addr, "POST", "/api/v1/cache/flush", "").await;
    assert_eq!(status, 200);
    assert_json(&body, include_str!("fixtures/api/success.json"));
    stop(tx, handle).await;

    let collector = fixture_collector();
    collector.flush().await;
    let (addr, tx, handle) = spawn_api(collector, Some(Arc::new(FailingCache))).await;
    let (status, _, body) = request(addr, "POST", "/api/v1/cache/flush", "").await;
    assert_eq!(status, 500);
    assert_json(&body, include_str!("fixtures/api/error_flush.json"));
    stop(tx, handle).await;
}

#[tokio::test]
async fn empty_collector_returns_empty_shapes() {
    let clock = Arc::new(ManualClock::new(WALL, 0));
    let collector = StatsCollector::start_with_clock(StatsConfig::default(), clock);
    let (addr, tx, handle) = spawn_api(collector, None).await;

    let (status, _, body) = request(addr, "GET", "/api/v1/stats", "").await;
    assert_eq!(status, 200);
    let stats: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(stats["total_queries"], 0);
    assert_eq!(stats["blocked_percentage"], 0.0);
    assert_eq!(stats["qps"], 0.0);

    let (status, _, body) = request(addr, "GET", "/api/v1/history", "").await;
    assert_eq!(status, 200);
    assert_json(&body, r#"{"points":[]}"#);

    let (status, _, body) = request(addr, "GET", "/api/v1/top", "").await;
    assert_eq!(status, 200);
    assert_json(
        &body,
        r#"{"top_domains":[],"top_blocked":[],"top_clients":[]}"#,
    );

    let (status, _, body) = request(addr, "GET", "/api/v1/logs", "").await;
    assert_eq!(status, 200);
    assert_json(&body, r#"{"total":0,"items":[]}"#);

    let (status, _, body) = request(addr, "GET", "/api/v1/health", "").await;
    assert_eq!(status, 200);
    let health: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(health["status"], "ok");
    assert!(health["version"].is_string());
    assert!(health["uptime_s"].is_u64());

    stop(tx, handle).await;
}

#[tokio::test]
async fn disabled_collector_still_serves_admin_endpoints() {
    let clock = Arc::new(ManualClock::new(WALL, 0));
    let config = StatsConfig {
        enabled: false,
        ..StatsConfig::default()
    };
    let collector = StatsCollector::start_with_clock(config, clock);
    let (addr, tx, handle) = spawn_api(collector, None).await;

    let (status, _, body) = request(addr, "GET", "/api/v1/stats", "").await;
    assert_eq!(status, 200);
    let stats: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(stats["total_queries"], 0);

    let (status, _, body) = request(addr, "POST", "/api/v1/logs/clear", "").await;
    assert_eq!(status, 200);
    assert_json(&body, include_str!("fixtures/api/success.json"));

    let (status, _, body) = request(addr, "POST", "/api/v1/cache/flush", "").await;
    assert_eq!(status, 200);
    assert_json(&body, include_str!("fixtures/api/success.json"));

    stop(tx, handle).await;
}
