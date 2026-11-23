// SPDX-License-Identifier: GPL-3.0-only
//! Unit coverage for matchers, file actions, response modifiers and the
//! sequence DSL (owned by the matcher/sequence agent).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
use hickory_proto::rr::{Name, RData, Record, RecordType};
use rust_dns_core::config::RuntimeConfig;
use rust_dns_core::context::QueryContext;
use rust_dns_core::error::Result;
use rust_dns_core::hosts::Hosts;
use rust_dns_core::matcher::{DomainSet, IpSet, ResponseIpMatcher};
use rust_dns_core::redirect::Redirect;
use rust_dns_core::response::ResponseModifier;
use rust_dns_core::sequence::{Registry, Sequence};
use rust_dns_core::{Executable, Matcher};

const CLIENT: &str = "127.0.0.1:53535";

fn make_query(id: u16, name: &str, query_type: RecordType) -> Message {
    let mut message = Message::new(id, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;
    message.add_query(Query::query(
        Name::from_ascii(name).expect("valid name"),
        query_type,
    ));
    message
}

fn context(name: &str, query_type: RecordType) -> QueryContext {
    let client: SocketAddr = CLIENT.parse().expect("client addr");
    QueryContext::new(make_query(0x1234, name, query_type), client)
}

fn config_with_rules(rules: &[&str]) -> RuntimeConfig {
    let value = serde_json::json!({
        "plugins": [{
            "tag": "main",
            "type": "sequence",
            "args": {"rules": rules},
        }],
    });
    serde_json::from_value(value).expect("runtime config")
}

#[test]
fn domain_set_supports_all_rule_forms() {
    let text = "\
# comment
; another comment

example.com.
domain:domain.test
full:full.test
keyword:tracker
regexp:^ads[0-9]+\\.zone\\.
";
    let set = DomainSet::from_text(text);

    assert!(set.contains("example.com"));
    assert!(set.contains("www.example.com"));
    assert!(set.contains("EXAMPLE.COM"));
    assert!(!set.contains("notexample.com"));

    assert!(set.contains("a.domain.test"));
    assert!(set.contains("domain.test"));

    assert!(set.contains("full.test"));
    assert!(!set.contains("sub.full.test"));

    assert!(set.contains("my-tracker-host.net"));
    assert!(!set.contains("clean.example"));

    assert!(set.contains("ads42.zone.net"));
    assert!(!set.contains("ads.zone.net"));

    assert!(!set.is_empty());
}

#[test]
fn ip_set_supports_cidr_and_bare_addresses() {
    let set = IpSet::from_text("10.0.0.0/8\n192.168.1.5\n2001:db8::/32\n");

    assert!(set.contains("10.1.2.3".parse().unwrap()));
    assert!(!set.contains("11.0.0.1".parse().unwrap()));
    assert!(set.contains("192.168.1.5".parse().unwrap()));
    assert!(!set.contains("192.168.1.6".parse().unwrap()));
    assert!(set.contains("2001:db8::1".parse().unwrap()));
    assert!(!set.contains("2001:db9::1".parse().unwrap()));
}

#[test]
fn missing_rule_files_are_skipped() {
    let missing = PathBuf::from(format!(
        "/nonexistent/rust-dns-test-{}.txt",
        std::process::id()
    ));
    let files = std::slice::from_ref(&missing);

    assert!(DomainSet::load(files).unwrap().is_empty());
    assert!(IpSet::load(files).unwrap().is_empty());
    let mut ctx = context("x.test", RecordType::A);
    assert!(!Hosts::load(files).unwrap().apply(&mut ctx).unwrap());
    assert!(Redirect::load(files).unwrap().is_empty());
}

#[test]
fn response_ip_matcher_scans_answers() {
    let matcher = ResponseIpMatcher {
        set: Arc::new(IpSet::from_text("203.0.113.0/24\n")),
    };
    let mut ctx = context("x.test", RecordType::A);
    assert!(!matcher.matches(&ctx));

    let mut response = Message::new(1, MessageType::Response, OpCode::Query);
    response.add_answer(Record::from_rdata(
        Name::from_ascii("x.test.").unwrap(),
        60,
        RData::from(Ipv4Addr::new(203, 0, 113, 9)),
    ));
    ctx.set_response(response);
    assert!(matcher.matches(&ctx));
}

#[test]
fn hosts_synthesizes_a_and_aaaa() {
    let hosts = Hosts::from_text("example.com 1.2.3.4 5.6.7.8\nv6.test ::1\n");

    let mut ctx = context("example.com.", RecordType::A);
    assert!(hosts.apply(&mut ctx).unwrap());
    let response = ctx.response.as_ref().expect("hosts response");
    assert_eq!(response.metadata.id, 0x1234);
    assert_eq!(response.metadata.response_code, ResponseCode::NoError);
    assert_eq!(response.queries.len(), 1);
    assert_eq!(
        response.queries[0].name().to_utf8().trim_end_matches('.'),
        "example.com"
    );
    let ips: Vec<IpAddr> = response
        .answers
        .iter()
        .filter_map(|record| record.data.ip_addr())
        .collect();
    assert_eq!(ips.len(), 2);
    assert!(ips.contains(&"1.2.3.4".parse().unwrap()));
    assert!(ips.contains(&"5.6.7.8".parse().unwrap()));
    assert!(
        response
            .answers
            .iter()
            .all(|record| record.record_type() == RecordType::A && record.ttl == 600)
    );

    let mut ctx = context("v6.test", RecordType::AAAA);
    assert!(hosts.apply(&mut ctx).unwrap());
    let response = ctx.response.as_ref().expect("hosts response");
    let ips: Vec<IpAddr> = response
        .answers
        .iter()
        .filter_map(|record| record.data.ip_addr())
        .collect();
    assert_eq!(ips, vec!["::1".parse::<IpAddr>().unwrap()]);

    // A known name queried for the other family is authoritative: empty NOERROR.
    let mut ctx = context("v6.test", RecordType::A);
    assert!(hosts.apply(&mut ctx).unwrap());
    assert!(
        ctx.response
            .as_ref()
            .expect("hosts response")
            .answers
            .is_empty()
    );

    // Unknown names fall through to the rest of the sequence.
    let mut ctx = context("unknown.test", RecordType::A);
    assert!(!hosts.apply(&mut ctx).unwrap());
    assert!(!ctx.has_response());
}

#[test]
fn redirect_rewrites_name_and_preserves_type() {
    let redirect = Redirect::from_text("old.example new.example\n");

    let mut ctx = context("www.old.example.", RecordType::AAAA);
    assert!(redirect.apply(&mut ctx));
    let query = &ctx.query.queries[0];
    assert_eq!(query.name().to_utf8(), "www.new.example.");
    assert!(query.name().is_fqdn());
    assert_eq!(query.query_type(), RecordType::AAAA);

    let mut ctx = context("old.example", RecordType::A);
    assert!(redirect.apply(&mut ctx));
    assert_eq!(ctx.query.queries[0].name().to_utf8(), "new.example.");
    assert!(ctx.query.queries[0].name().is_fqdn());

    let mut ctx = context("other.example", RecordType::A);
    assert!(!redirect.apply(&mut ctx));
}

#[test]
fn response_modifier_clamps_ttl_and_drops_https() {
    let mut ctx = context("x.test", RecordType::A);
    let name = Name::from_ascii("x.test.").unwrap();
    let mut response = Message::new(7, MessageType::Response, OpCode::Query);
    response.add_answer(Record::from_rdata(
        name.clone(),
        5,
        RData::from(Ipv4Addr::new(1, 2, 3, 4)),
    ));
    response.add_answer(Record::from_rdata(
        name.clone(),
        1000,
        RData::from(Ipv4Addr::new(5, 6, 7, 8)),
    ));
    response.add_answer(Record::from_rdata(
        name,
        120,
        RData::Update0(RecordType::HTTPS),
    ));
    ctx.set_response(response);

    let modifier = ResponseModifier {
        minimal_ttl: 60,
        maximum_ttl: 300,
        reject_type65: true,
    };
    modifier.apply(&mut ctx).unwrap();

    let response = ctx.response.as_ref().expect("modified response");
    assert_eq!(response.answers.len(), 2);
    assert!(
        response
            .answers
            .iter()
            .all(|record| (60..=300).contains(&record.ttl))
    );
    assert!(
        response
            .answers
            .iter()
            .all(|record| !matches!(record.record_type(), RecordType::HTTPS | RecordType::SVCB))
    );
}

#[test]
fn response_modifier_without_response_is_a_noop() {
    let modifier = ResponseModifier {
        minimal_ttl: 60,
        maximum_ttl: 300,
        reject_type65: true,
    };
    let mut ctx = context("x.test", RecordType::A);
    modifier.apply(&mut ctx).unwrap();
    assert!(!ctx.has_response());
}

struct NameMatcher(String);

impl Matcher for NameMatcher {
    fn matches(&self, ctx: &QueryContext) -> bool {
        ctx.qname() == self.0
    }
}

struct RecordExec {
    hits: Arc<AtomicUsize>,
    rcode: ResponseCode,
}

#[async_trait]
impl Executable for RecordExec {
    async fn execute(&self, ctx: &mut QueryContext) -> Result<()> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        let mut reply = Message::new(
            ctx.query.metadata.id,
            MessageType::Response,
            ctx.query.metadata.op_code,
        );
        reply.metadata.response_code = self.rcode;
        if let Some(query) = ctx.query.queries.first() {
            reply.queries.push(query.clone());
        }
        ctx.set_response(reply);
        Ok(())
    }
}

struct NoopExec {
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl Executable for NoopExec {
    async fn execute(&self, _ctx: &mut QueryContext) -> Result<()> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn sequence_reject_builds_nxdomain() {
    let mut registry = Registry::new();
    registry.insert_matcher("block", Arc::new(NameMatcher("block.test".to_string())));

    let sequence = Sequence::build(&config_with_rules(&["block -> reject"]), &registry).unwrap();

    let mut ctx = context("block.test.", RecordType::A);
    sequence.execute(&mut ctx).await.unwrap();
    let response = ctx.response.as_ref().expect("nxdomain response");
    assert_eq!(response.metadata.response_code, ResponseCode::NXDomain);
    assert_eq!(response.metadata.id, 0x1234);
    assert_eq!(response.queries.len(), 1);
    assert!(ctx.is_blocked);
    assert_eq!(ctx.rule.as_deref(), Some("block"));

    let mut ctx = context("ok.test", RecordType::A);
    sequence.execute(&mut ctx).await.unwrap();
    assert!(!ctx.has_response());
    assert!(!ctx.is_blocked);
}

#[tokio::test]
async fn sequence_stops_after_first_response() {
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));

    let mut registry = Registry::new();
    registry.insert_executable(
        "first",
        Arc::new(RecordExec {
            hits: first_hits.clone(),
            rcode: ResponseCode::NoError,
        }),
    );
    registry.insert_executable(
        "second",
        Arc::new(RecordExec {
            hits: second_hits.clone(),
            rcode: ResponseCode::NoError,
        }),
    );

    let sequence = Sequence::build(&config_with_rules(&["first", "second"]), &registry).unwrap();
    let mut ctx = context("x.test", RecordType::A);
    sequence.execute(&mut ctx).await.unwrap();

    assert_eq!(first_hits.load(Ordering::SeqCst), 1);
    assert_eq!(second_hits.load(Ordering::SeqCst), 0);
    assert!(ctx.has_response());
    assert_eq!(ctx.rule.as_deref(), Some("first"));
}

#[tokio::test]
async fn sequence_continues_after_non_matching_and_noop_rules() {
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));

    let mut registry = Registry::new();
    registry.insert_matcher("block", Arc::new(NameMatcher("block.test".to_string())));
    registry.insert_matcher("matching", Arc::new(NameMatcher("x.test".to_string())));
    registry.insert_executable(
        "first",
        Arc::new(NoopExec {
            hits: first_hits.clone(),
        }),
    );
    registry.insert_executable(
        "second",
        Arc::new(RecordExec {
            hits: second_hits.clone(),
            rcode: ResponseCode::NoError,
        }),
    );

    // `block` does not match, `unknown` is skipped, `matching` is a bare
    // matcher (match with no action), then `second` answers.
    let sequence = Sequence::build(
        &config_with_rules(&["block -> first", "unknown", "matching", "second"]),
        &registry,
    )
    .unwrap();

    let mut ctx = context("x.test", RecordType::A);
    sequence.execute(&mut ctx).await.unwrap();

    assert_eq!(first_hits.load(Ordering::SeqCst), 0);
    assert_eq!(second_hits.load(Ordering::SeqCst), 1);
    assert!(ctx.has_response());
}

#[tokio::test]
async fn sequence_continues_after_action_without_response() {
    let noop_hits = Arc::new(AtomicUsize::new(0));
    let answer_hits = Arc::new(AtomicUsize::new(0));

    let mut registry = Registry::new();
    registry.insert_matcher("block", Arc::new(NameMatcher("block.test".to_string())));
    registry.insert_executable(
        "noop",
        Arc::new(NoopExec {
            hits: noop_hits.clone(),
        }),
    );
    registry.insert_executable(
        "answer",
        Arc::new(RecordExec {
            hits: answer_hits.clone(),
            rcode: ResponseCode::NoError,
        }),
    );

    let sequence =
        Sequence::build(&config_with_rules(&["block -> noop", "answer"]), &registry).unwrap();

    let mut ctx = context("block.test", RecordType::A);
    sequence.execute(&mut ctx).await.unwrap();

    assert_eq!(noop_hits.load(Ordering::SeqCst), 1);
    assert_eq!(answer_hits.load(Ordering::SeqCst), 1);
    assert!(ctx.has_response());
}

#[test]
fn sequence_build_requires_a_sequence_plugin() {
    let config: RuntimeConfig = serde_json::from_str(r#"{"plugins": []}"#).expect("empty config");
    let registry = Registry::new();
    assert!(Sequence::build(&config, &registry).is_err());
}
