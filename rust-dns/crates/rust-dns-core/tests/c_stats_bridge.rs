// SPDX-License-Identifier: GPL-3.0-only
//! `QueryContext -> QueryEvent` mapping.

use std::net::SocketAddr;

use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
use hickory_proto::rr::rdata::{A, AAAA, CNAME};
use hickory_proto::rr::{Name, RData, Record, RecordType};
use rust_dns_core::context::QueryContext;
use rust_dns_core::stats_bridge::event_from_context;
use rust_dns_stats_api::types::MAX_ANSWERS;

fn name(value: &str) -> Name {
    Name::from_ascii(value).expect("valid name")
}

fn client() -> SocketAddr {
    "192.168.1.10:5555".parse().expect("client addr")
}

fn context(qtype: RecordType) -> QueryContext {
    let mut query = Message::new(0x4242, MessageType::Query, OpCode::Query);
    query.add_query(Query::query(name("example.com."), qtype));
    QueryContext::new(query, client())
}

fn response() -> Message {
    let mut message = Message::new(0x4242, MessageType::Response, OpCode::Query);
    message.add_query(Query::query(name("example.com."), RecordType::A));
    message.add_answer(Record::from_rdata(
        name("example.com."),
        60,
        RData::A(A::new(93, 184, 216, 34)),
    ));
    message.add_answer(Record::from_rdata(
        name("example.com."),
        30,
        RData::AAAA(AAAA::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
    ));
    // Non-address records are not part of the summarized answers.
    message.add_answer(Record::from_rdata(
        name("example.com."),
        120,
        RData::CNAME(CNAME(name("target.example.com."))),
    ));
    message
}

#[test]
fn maps_query_and_answer_fields() {
    let mut ctx = context(RecordType::A);
    ctx.response = Some(response());
    ctx.upstream = Some("forward_remote".to_string());
    ctx.rule = Some("geoip_cn".to_string());
    ctx.is_cached = true;
    ctx.is_blocked = true;

    let event = event_from_context(&ctx);

    assert_eq!(event.client_ip, "192.168.1.10", "port is dropped");
    assert_eq!(event.domain, "example.com");
    assert_eq!(event.qtype, 1);
    assert_eq!(event.status, 0);
    assert!(event.is_cached);
    assert!(event.is_blocked);
    assert_eq!(event.upstream, "forward_remote");
    assert_eq!(event.rule, "geoip_cn");

    assert_eq!(event.answers.len(), 2, "only A/AAAA are recorded");
    assert_eq!(event.answers[0].record_type, 1);
    assert_eq!(event.answers[0].data, "93.184.216.34");
    assert_eq!(event.answers[0].ttl, 60);
    assert_eq!(event.answers[1].record_type, 28);
    assert_eq!(event.answers[1].data, "2001:db8::1");
    assert_eq!(event.answers[1].ttl, 30);
}

#[test]
fn response_code_and_defaults_are_mapped() {
    let mut ctx = context(RecordType::AAAA);
    let mut nx = response();
    nx.metadata.response_code = ResponseCode::NXDomain;
    ctx.response = Some(nx);

    let event = event_from_context(&ctx);
    assert_eq!(event.qtype, 28);
    assert_eq!(event.status, 3, "NXDOMAIN");
    assert_eq!(event.upstream, "");
    assert_eq!(event.rule, "");
    assert!(!event.is_cached);
    assert!(!event.is_blocked);
}

#[test]
fn missing_response_is_servfail_with_no_answers() {
    let ctx = context(RecordType::A);
    let event = event_from_context(&ctx);
    assert_eq!(event.status, 2, "no response means SERVFAIL");
    assert!(event.answers.is_empty());
    assert_eq!(event.domain, "example.com");
    assert_eq!(event.qtype, 1);
}

#[test]
fn answers_are_capped_at_eight() {
    let mut ctx = context(RecordType::A);
    let mut many = Message::new(0x4242, MessageType::Response, OpCode::Query);
    for index in 0..10u8 {
        many.add_answer(Record::from_rdata(
            name("example.com."),
            10,
            RData::A(A::new(10, 0, 0, index)),
        ));
    }
    ctx.response = Some(many);

    let event = event_from_context(&ctx);
    assert_eq!(event.answers.len(), MAX_ANSWERS);
}
