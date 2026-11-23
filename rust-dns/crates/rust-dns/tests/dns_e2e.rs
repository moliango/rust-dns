// SPDX-License-Identifier: GPL-3.0-only
//! End-to-end DNS tests: generated-config pipeline over real UDP/TCP sockets
//! with in-process fake upstreams.
//!
//! No test touches the network outside `127.0.0.1` and none shells out to
//! `dig`; every upstream is a fake server started by the test itself.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
use hickory_proto::rr::rdata::A;
use hickory_proto::rr::{Name, RData, Record, RecordType};
use rust_dns::dns::DnsService;
use rust_dns_core::Handler;
use rust_dns_core::config::RuntimeConfig;
use rust_dns_core::server::{serve_tcp_listener, serve_udp_socket};
use rust_dns_stats_api::{LogQuery, StatsCollector, StatsConfig};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::oneshot;

static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rust-dns-e2e-{}-{}",
        std::process::id(),
        TEST_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_rule(dir: &Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

fn make_query(name: &str, qtype: RecordType) -> Message {
    let mut message = Message::new(0x1234, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;
    message.add_query(Query::query(Name::from_ascii(name).unwrap(), qtype));
    message
}

fn a_record(message: &Message) -> Ipv4Addr {
    let record = message.answers.first().expect("expected an A answer");
    match &record.data {
        RData::A(address) => address.0,
        other => panic!("unexpected answer rdata: {other:?}"),
    }
}

async fn udp_query(server: SocketAddr, name: &str) -> Message {
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let query = make_query(name, RecordType::A);
    client
        .send_to(&query.to_vec().unwrap(), server)
        .await
        .unwrap();

    let mut buf = vec![0u8; 65535];
    let (len, _peer) = tokio::time::timeout(Duration::from_secs(3), client.recv_from(&mut buf))
        .await
        .expect("udp query timed out")
        .expect("udp recv failed");
    Message::from_vec(&buf[..len]).expect("invalid udp response")
}

async fn tcp_query(server: SocketAddr, name: &str) -> Message {
    let query = make_query(name, RecordType::A);
    let bytes = query.to_vec().unwrap();
    let mut stream = TcpStream::connect(server).await.unwrap();
    stream
        .write_all(&(bytes.len() as u16).to_be_bytes())
        .await
        .unwrap();
    stream.write_all(&bytes).await.unwrap();

    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf).await.unwrap();
    let len = u16::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await.unwrap();
    Message::from_vec(&buf).unwrap()
}

/// Reply to an A query with `answer`.
fn answer_query(query: &Message, answer: Ipv4Addr) -> Option<Message> {
    let question = query.queries.first()?;
    let mut reply = Message::new(
        query.metadata.id,
        MessageType::Response,
        query.metadata.op_code,
    );
    reply.metadata.recursion_desired = query.metadata.recursion_desired;
    reply.metadata.recursion_available = true;
    reply.queries = query.queries.clone();
    reply.add_answer(Record::from_rdata(
        question.name().clone(),
        60,
        RData::A(A(answer)),
    ));
    Some(reply)
}

async fn fake_udp_upstream(answer: Ipv4Addr) -> (SocketAddr, oneshot::Sender<()>) {
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr = socket.local_addr().unwrap();
    let (stop, mut stop_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                received = socket.recv_from(&mut buf) => {
                    let Ok((len, peer)) = received else { continue };
                    let Ok(query) = Message::from_vec(&buf[..len]) else { continue };
                    if let Some(reply) = answer_query(&query, answer) {
                        let _ = socket.send_to(&reply.to_vec().unwrap(), peer).await;
                    }
                }
            }
        }
    });

    (addr, stop)
}

async fn fake_tcp_upstream(answer: Ipv4Addr) -> (SocketAddr, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, mut stop_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                accepted = listener.accept() => {
                    let Ok((mut stream, _peer)) = accepted else { continue };
                    tokio::spawn(async move {
                        let mut len_buf = [0u8; 2];
                        if stream.read_exact(&mut len_buf).await.is_err() {
                            return;
                        }
                        let len = u16::from_be_bytes(len_buf) as usize;
                        let mut buf = vec![0u8; len];
                        if stream.read_exact(&mut buf).await.is_err() {
                            return;
                        }
                        let Ok(query) = Message::from_vec(&buf) else { return };
                        let Some(reply) = answer_query(&query, answer) else { return };
                        let bytes = reply.to_vec().unwrap();
                        let _ = stream.write_all(&(bytes.len() as u16).to_be_bytes()).await;
                        let _ = stream.write_all(&bytes).await;
                        let _ = stream.flush().await;
                    });
                }
            }
        }
    });

    (addr, stop)
}

/// Build the JSON shape emitted by `config.sh` for a small forwarding pipeline.
fn pipeline_config(hosts: &Path, blocklist: &Path, upstream: &str) -> Value {
    json!({
        "api": {"http": "127.0.0.1:9091"},
        "plugins": [
            {"tag": "hosts", "type": "domain_set", "args": {"files": [hosts]}},
            {"tag": "blocklist", "type": "domain_set", "args": {"files": [blocklist]}},
            {"tag": "cache", "type": "cache", "args": {"size": 128, "lazy_cache_ttl": 60}},
            {
                "tag": "forward_local",
                "type": "forward",
                "args": {"upstreams": [{"addr": upstream}], "concurrent": 1, "idle_timeout": 30}
            },
            {
                "tag": "modify_response",
                "type": "response_modifier",
                "args": {"minimal_ttl": 0, "maximum_ttl": 30, "reject_type65": false}
            },
            {
                "tag": "main",
                "type": "sequence",
                "args": {"rules": ["hosts", "blocklist -> reject", "cache", "modify_response", "forward_local"]}
            }
        ]
    })
}

fn runtime_config(value: Value) -> RuntimeConfig {
    serde_json::from_value(value).expect("valid runtime config")
}

fn handler(service: &Arc<DnsService>) -> Arc<dyn Handler> {
    Arc::clone(service) as Arc<dyn Handler>
}

#[tokio::test]
async fn udp_hosts_blocklist_forward_cache_and_stats() {
    let dir = temp_dir();
    let hosts = write_rule(&dir, "hosts.txt", "host.local 192.0.2.10\n");
    let blocklist = write_rule(&dir, "blocklist.txt", "blocked.test\n");

    let upstream_answer = Ipv4Addr::new(203, 0, 113, 7);
    let (upstream_addr, upstream_stop) = fake_udp_upstream(upstream_answer).await;
    let config = runtime_config(pipeline_config(
        &hosts,
        &blocklist,
        &format!("udp://{upstream_addr}"),
    ));

    let collector = StatsCollector::start(StatsConfig {
        capacity: 100,
        ..StatsConfig::default()
    });
    let service =
        Arc::new(DnsService::build(&config, Some(collector.clone())).expect("build pipeline"));

    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let server_addr = socket.local_addr().unwrap();
    let (server_stop, server_stop_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(serve_udp_socket(socket, handler(&service), async move {
        let _ = server_stop_rx.await;
    }));

    // hosts: synthesized answer.
    let response = udp_query(server_addr, "host.local.").await;
    assert_eq!(response.metadata.response_code, ResponseCode::NoError);
    assert_eq!(a_record(&response), Ipv4Addr::new(192, 0, 2, 10));

    // blocklist: NXDOMAIN.
    let response = udp_query(server_addr, "blocked.test.").await;
    assert_eq!(response.metadata.response_code, ResponseCode::NXDomain);

    // forward: upstream answer with the TTL clamped by the response modifier.
    let response = udp_query(server_addr, "example.org.").await;
    assert_eq!(response.metadata.response_code, ResponseCode::NoError);
    assert_eq!(a_record(&response), upstream_answer);
    assert_eq!(response.answers[0].ttl, 30);

    // cache: the second identical query is served without the upstream.
    let response = udp_query(server_addr, "example.org.").await;
    assert_eq!(a_record(&response), upstream_answer);
    assert_eq!(response.metadata.id, 0x1234);

    collector.flush().await;
    let stats = collector.stats();
    assert!(stats.total_queries >= 4, "stats: {stats:?}");
    assert!(stats.blocked_queries >= 1, "stats: {stats:?}");
    assert!(stats.cached_queries >= 1, "stats: {stats:?}");

    let logs = collector.logs(&LogQuery {
        limit: 100,
        ..LogQuery::default()
    });
    assert!(
        logs.items
            .iter()
            .any(|record| record.domain == "example.org" && record.is_cached),
        "cached log missing: {:?}",
        logs.items
    );
    assert!(
        logs.items
            .iter()
            .any(|record| record.domain == "blocked.test" && record.is_blocked),
        "blocked log missing: {:?}",
        logs.items
    );
    assert!(
        logs.items
            .iter()
            .any(|record| record.domain == "host.local"),
        "hosts log missing: {:?}",
        logs.items
    );
    assert!(
        logs.items
            .iter()
            .any(|record| record.upstream.starts_with("forward_local")),
        "upstream tag missing: {:?}",
        logs.items
    );

    let _ = server_stop.send(());
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .expect("server did not stop")
        .unwrap()
        .unwrap();
    let _ = upstream_stop.send(());
    collector.shutdown().await;
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn tcp_listener_forwards_over_tcp() {
    let dir = temp_dir();
    let hosts = write_rule(&dir, "hosts.txt", "");
    let blocklist = write_rule(&dir, "blocklist.txt", "");

    let upstream_answer = Ipv4Addr::new(203, 0, 113, 8);
    let (upstream_addr, upstream_stop) = fake_tcp_upstream(upstream_answer).await;
    let config = runtime_config(pipeline_config(
        &hosts,
        &blocklist,
        &format!("tcp://{upstream_addr}"),
    ));

    let collector = StatsCollector::start(StatsConfig {
        capacity: 100,
        ..StatsConfig::default()
    });
    let service =
        Arc::new(DnsService::build(&config, Some(collector.clone())).expect("build pipeline"));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = listener.local_addr().unwrap();
    let (server_stop, server_stop_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(serve_tcp_listener(
        listener,
        handler(&service),
        async move {
            let _ = server_stop_rx.await;
        },
    ));

    let response = tcp_query(server_addr, "example.net.").await;
    assert_eq!(response.metadata.response_code, ResponseCode::NoError);
    assert_eq!(a_record(&response), upstream_answer);

    let _ = server_stop.send(());
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .expect("server did not stop")
        .unwrap()
        .unwrap();
    let _ = upstream_stop.send(());
    collector.shutdown().await;
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn missing_upstream_answers_servfail() {
    let dir = temp_dir();
    let hosts = write_rule(&dir, "hosts.txt", "");
    let blocklist = write_rule(&dir, "blocklist.txt", "");

    // Reserve a port, then release it so nothing listens there. The forward
    // times out (5s) and the handler answers SERVFAIL.
    let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = probe.local_addr().unwrap();
    drop(probe);

    let value = pipeline_config(&hosts, &blocklist, &format!("udp://{dead_addr}"));
    // Keep the modifier semantics but shorten the wait by hitting a closed port
    // (UDP: an ICMP port-unreachable usually fails the socket immediately).
    let config = runtime_config(value);

    let collector = StatsCollector::start(StatsConfig {
        capacity: 100,
        ..StatsConfig::default()
    });
    let service = Arc::new(DnsService::build(&config, Some(collector.clone())).unwrap());

    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let server_addr = socket.local_addr().unwrap();
    let (server_stop, server_stop_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(serve_udp_socket(socket, handler(&service), async move {
        let _ = server_stop_rx.await;
    }));

    let response = tokio::time::timeout(
        Duration::from_secs(8),
        udp_query(server_addr, "nowhere.example."),
    )
    .await
    .expect("query should answer within the forward timeout");
    assert_eq!(response.metadata.response_code, ResponseCode::ServFail);

    let _ = server_stop.send(());
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .expect("server did not stop")
        .unwrap()
        .unwrap();
    collector.shutdown().await;
    std::fs::remove_dir_all(dir).ok();
}
