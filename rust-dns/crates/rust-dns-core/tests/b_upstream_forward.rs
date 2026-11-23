// SPDX-License-Identifier: GPL-3.0-only
//! Upstream transports and forward batching, against in-process fake servers.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use hickory_proto::op::{Message, MessageType, OpCode, Query};
use hickory_proto::rr::rdata::A;
use hickory_proto::rr::{Name, RData, Record, RecordType};
use rust_dns_core::context::QueryContext;
use rust_dns_core::forward::Forward;
use rust_dns_core::upstream::{build_upstream, parse_upstream_addr};
use rust_dns_core::{CoreError, Executable};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};

const ANSWER: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 53);

fn make_query(name: &str, qtype: RecordType) -> Message {
    let mut message = Message::new(0x1234, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;
    message.add_query(Query::query(Name::from_ascii(name).unwrap(), qtype));
    message
}

fn make_response(query: &Message, answer: Ipv4Addr) -> Message {
    let mut response = Message::new(
        query.metadata.id,
        MessageType::Response,
        query.metadata.op_code,
    );
    response.metadata.recursion_desired = query.metadata.recursion_desired;
    response.metadata.recursion_available = true;
    response.queries = query.queries.clone();
    if let Some(question) = query.queries.first() {
        response.add_answer(Record::from_rdata(
            question.name().clone(),
            60,
            RData::A(A(answer)),
        ));
    }
    response
}

fn assert_answer(response: &Message) {
    let record = response.answers.first().expect("expected an answer");
    match &record.data {
        RData::A(a) => assert_eq!(a.0, ANSWER),
        other => panic!("unexpected answer rdata: {other:?}"),
    }
}

fn local_client() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 1))
}

/// Fake UDP resolver that answers every valid query.
async fn spawn_udp_fake() -> SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        loop {
            let Ok((len, peer)) = socket.recv_from(&mut buf).await else {
                return;
            };
            let Ok(query) = Message::from_vec(&buf[..len]) else {
                continue;
            };
            let Ok(bytes) = make_response(&query, ANSWER).to_vec() else {
                continue;
            };
            let _ = socket.send_to(&bytes, peer).await;
        }
    });
    addr
}

/// Fake TCP resolver with 2-byte length framing.
async fn spawn_tcp_fake() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _peer)) = listener.accept().await else {
                return;
            };
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
                let Ok(query) = Message::from_vec(&buf) else {
                    return;
                };
                let Ok(bytes) = make_response(&query, ANSWER).to_vec() else {
                    return;
                };
                let Ok(len) = u16::try_from(bytes.len()) else {
                    return;
                };
                if stream.write_all(&len.to_be_bytes()).await.is_ok() {
                    let _ = stream.write_all(&bytes).await;
                }
            });
        }
    });
    addr
}

/// Bind a UDP port but never answer. The socket must stay alive.
async fn silent_udp() -> (UdpSocket, SocketAddr) {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    (socket, addr)
}

#[test]
fn address_parsing_rejects_hostnames() {
    for raw in [
        "udp://dns.google",
        "tls://dns.google",
        "https://1.1.1.1",
        "nope",
    ] {
        assert!(
            matches!(parse_upstream_addr(raw), Err(CoreError::Unsupported(_))),
            "{raw} should be rejected"
        );
    }
}

#[tokio::test]
async fn udp_exchange_round_trips() {
    let addr = spawn_udp_fake().await;
    let upstream = build_upstream(&format!("udp://{addr}"), true, Duration::from_secs(2)).unwrap();
    let query = make_query("example.com.", RecordType::A);

    let response = upstream.exchange(&query).await.unwrap();
    assert_eq!(response.metadata.id, query.metadata.id);
    assert_answer(&response);
}

#[tokio::test]
async fn tcp_exchange_uses_length_framing() {
    let addr = spawn_tcp_fake().await;
    let upstream = build_upstream(&format!("tcp://{addr}"), true, Duration::from_secs(2)).unwrap();
    let query = make_query("example.com.", RecordType::A);

    let response = upstream.exchange(&query).await.unwrap();
    assert_eq!(response.metadata.id, query.metadata.id);
    assert_answer(&response);
}

#[tokio::test]
async fn udp_exchange_times_out_against_silent_server() {
    let (_socket, addr) = silent_udp().await;
    let upstream =
        build_upstream(&format!("udp://{addr}"), true, Duration::from_millis(100)).unwrap();
    let query = make_query("example.com.", RecordType::A);

    let err = upstream.exchange(&query).await.unwrap_err();
    assert!(matches!(err, CoreError::Upstream(_)), "unexpected: {err}");
}

#[tokio::test]
async fn forward_returns_first_successful_upstream() {
    let good = spawn_udp_fake().await;
    let (_silent, dead) = silent_udp().await;

    let upstreams = vec![
        build_upstream(&format!("udp://{dead}"), true, Duration::from_millis(100)).unwrap(),
        build_upstream(&format!("udp://{good}"), true, Duration::from_secs(2)).unwrap(),
    ];
    let forward = Forward::new(upstreams, 2, Duration::from_secs(2)).with_tag("main");

    let mut ctx = QueryContext::new(make_query("example.com.", RecordType::A), local_client());
    forward.execute(&mut ctx).await.unwrap();

    let response = ctx
        .response
        .as_ref()
        .expect("forward should set a response");
    assert_answer(response);
    assert_eq!(ctx.upstream.as_deref(), Some("main[1]"));
}

#[tokio::test]
async fn forward_tries_later_batches_after_failures() {
    let good = spawn_udp_fake().await;
    let (_silent, dead) = silent_udp().await;

    let upstreams = vec![
        build_upstream(&format!("udp://{dead}"), true, Duration::from_millis(80)).unwrap(),
        build_upstream(&format!("udp://{good}"), true, Duration::from_secs(2)).unwrap(),
    ];
    // concurrent=1 forces two sequential batches.
    let forward = Forward::new(upstreams, 1, Duration::from_secs(3));

    let mut ctx = QueryContext::new(make_query("example.com.", RecordType::A), local_client());
    forward.execute(&mut ctx).await.unwrap();

    assert!(ctx.response.is_some());
    assert_eq!(ctx.upstream.as_deref(), Some("upstream-1"));
}

#[tokio::test]
async fn forward_reports_upstream_error_when_all_fail() {
    let (_a, addr_a) = silent_udp().await;
    let (_b, addr_b) = silent_udp().await;

    let upstreams = vec![
        build_upstream(&format!("udp://{addr_a}"), true, Duration::from_millis(80)).unwrap(),
        build_upstream(&format!("udp://{addr_b}"), true, Duration::from_millis(80)).unwrap(),
    ];
    let forward = Forward::new(upstreams, 2, Duration::from_secs(1));

    let mut ctx = QueryContext::new(make_query("example.com.", RecordType::A), local_client());
    let err = forward.execute(&mut ctx).await.unwrap_err();
    assert!(matches!(err, CoreError::Upstream(_)), "unexpected: {err}");
    assert!(ctx.response.is_none());
}

#[tokio::test]
async fn forward_without_upstreams_is_an_error() {
    let forward = Forward::new(Vec::new(), 2, Duration::from_secs(1));
    let mut ctx = QueryContext::new(make_query("example.com.", RecordType::A), local_client());
    let err = forward.execute(&mut ctx).await.unwrap_err();
    assert!(matches!(err, CoreError::Upstream(_)), "unexpected: {err}");
}
