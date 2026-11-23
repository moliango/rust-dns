// SPDX-License-Identifier: GPL-3.0-only
//! UDP and TCP listener behaviour, with in-process clients.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hickory_proto::op::{Message, MessageType, OpCode, Query};
use hickory_proto::rr::rdata::A;
use hickory_proto::rr::{Name, RData, Record, RecordType};
use rust_dns_core::Handler;
use rust_dns_core::server::{serve_tcp_listener, serve_udp_socket};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::oneshot;

const ANSWER: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 7);

struct EchoHandler;

#[async_trait]
impl Handler for EchoHandler {
    async fn handle(&self, query: Message, _client: SocketAddr) -> Option<Message> {
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
                RData::A(A(ANSWER)),
            ));
        }
        Some(response)
    }
}

fn handler() -> Arc<dyn Handler> {
    Arc::new(EchoHandler)
}

fn make_query() -> Message {
    let mut message = Message::new(0xbeef, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;
    message.add_query(Query::query(
        Name::from_ascii("example.com.").unwrap(),
        RecordType::A,
    ));
    message
}

async fn recv_udp_response(socket: &UdpSocket) -> Message {
    let mut buf = vec![0u8; 65535];
    let (len, _peer) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buf))
        .await
        .expect("udp response timed out")
        .expect("udp recv failed");
    Message::from_vec(&buf[..len]).expect("invalid response")
}

fn assert_answer(response: &Message) {
    let record = response.answers.first().expect("expected an answer");
    match &record.data {
        RData::A(a) => assert_eq!(a.0, ANSWER),
        other => panic!("unexpected answer rdata: {other:?}"),
    }
}

#[tokio::test]
async fn udp_server_drops_malformed_packets_and_answers_valid_ones() {
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr = socket.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    let server = tokio::spawn(serve_udp_socket(socket, handler(), async move {
        let _ = rx.await;
    }));

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    // Malformed payload: silently dropped.
    client.send_to(b"\xde\xad\xbe\xef", addr).await.unwrap();

    let query = make_query();
    client
        .send_to(&query.to_vec().unwrap(), addr)
        .await
        .unwrap();
    let response = recv_udp_response(&client).await;
    assert_eq!(response.metadata.id, query.metadata.id);
    assert_answer(&response);

    tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn tcp_server_answers_one_query_per_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    let server = tokio::spawn(serve_tcp_listener(listener, handler(), async move {
        let _ = rx.await;
    }));

    let query = make_query();
    let bytes = query.to_vec().unwrap();
    let mut stream = TcpStream::connect(addr).await.unwrap();
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
    let response = Message::from_vec(&buf).unwrap();
    assert_eq!(response.metadata.id, query.metadata.id);
    assert_answer(&response);

    tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn tcp_server_survives_malformed_frame() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    let server = tokio::spawn(serve_tcp_listener(listener, handler(), async move {
        let _ = rx.await;
    }));

    // A frame with a valid length but garbage body must not kill the listener.
    let mut bad = TcpStream::connect(addr).await.unwrap();
    bad.write_all(&2u16.to_be_bytes()).await.unwrap();
    bad.write_all(&[0x00, 0x00]).await.unwrap();
    drop(bad);

    let query = make_query();
    let bytes = query.to_vec().unwrap();
    let mut stream = TcpStream::connect(addr).await.unwrap();
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
    let response = Message::from_vec(&buf).unwrap();
    assert_answer(&response);

    tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn udp_bind_and_serve_via_public_entrypoint() {
    // Reserve a port, release it, then let `serve_udp` bind it. This exercises
    // the bind path while keeping the window small.
    let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);

    let (tx, rx) = oneshot::channel::<()>();
    let server = tokio::spawn(rust_dns_core::server::serve_udp(
        addr,
        handler(),
        async move {
            let _ = rx.await;
        },
    ));

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let query = make_query();
    client
        .send_to(&query.to_vec().unwrap(), addr)
        .await
        .unwrap();
    let response = recv_udp_response(&client).await;
    assert_answer(&response);

    tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
