// SPDX-License-Identifier: GPL-3.0-only
//! Cache behavior: keys, TTL handling, lazy stale, flush and capacity.

use std::net::SocketAddr;
use std::thread::sleep;
use std::time::Duration;

use hickory_proto::op::{Edns, Message, MessageType, OpCode, Query};
use hickory_proto::rr::rdata::A;
use hickory_proto::rr::{Name, RData, Record, RecordType};
use rust_dns_core::Executable;
use rust_dns_core::cache::Cache;
use rust_dns_core::context::QueryContext;
use rust_dns_stats_api::CacheFlush;

fn client() -> SocketAddr {
    "127.0.0.1:53000".parse().expect("client addr")
}

fn name(value: &str) -> Name {
    Name::from_ascii(value).expect("valid name")
}

fn query_with_id(value: &str, qtype: RecordType, id: u16) -> Message {
    let mut message = Message::new(id, MessageType::Query, OpCode::Query);
    message.add_query(Query::query(name(value), qtype));
    message
}

fn context_with_id(value: &str, qtype: RecordType, id: u16) -> QueryContext {
    QueryContext::new(query_with_id(value, qtype, id), client())
}

fn context(value: &str, qtype: RecordType) -> QueryContext {
    context_with_id(value, qtype, 0x1234)
}

fn response_for(ctx: &QueryContext, ttl: u32) -> Message {
    let mut response = Message::new(ctx.query.metadata.id, MessageType::Response, OpCode::Query);
    response.queries = ctx.query.queries.clone();
    response.add_answer(Record::from_rdata(
        name("example.com."),
        ttl,
        RData::A(A::new(93, 184, 216, 34)),
    ));
    response
}

/// Seed the cache with a positive A response for `ctx`.
fn seed(cache: &Cache, ctx: &mut QueryContext, ttl: u32) {
    ctx.response = Some(response_for(ctx, ttl));
    cache.put(ctx);
}

#[test]
fn key_distinguishes_qtype_qclass_and_do_bit() {
    let cache = Cache::new(0, 0);
    let mut original = context("example.com.", RecordType::A);
    seed(&cache, &mut original, 300);

    // Same question: hit.
    let mut hit = context("example.com.", RecordType::A);
    assert!(cache.get(&mut hit));

    // Different qtype: miss.
    let mut aaaa = context("example.com.", RecordType::AAAA);
    assert!(!cache.get(&mut aaaa));

    // Different EDNS DO bit: miss.
    let mut do_query = query_with_id("example.com.", RecordType::A, 0x1234);
    let mut edns = Edns::new();
    edns.set_dnssec_ok(true);
    do_query.set_edns(edns);
    let mut with_do = QueryContext::new(do_query, client());
    assert!(!cache.get(&mut with_do));

    // Different name: miss.
    let mut other = context("other.example.", RecordType::A);
    assert!(!cache.get(&mut other));

    // Names are compared case-insensitively.
    let mut uppercase = context("EXAMPLE.COM.", RecordType::A);
    assert!(cache.get(&mut uppercase));
}

#[test]
fn hit_decrements_ttl_and_rewrites_id() {
    let cache = Cache::new(0, 0);
    let mut original = context_with_id("example.com.", RecordType::A, 0x1111);
    seed(&cache, &mut original, 5);

    sleep(Duration::from_millis(1_200));

    let mut second = context_with_id("example.com.", RecordType::A, 0x2222);
    assert!(cache.get(&mut second));
    assert!(second.is_cached);

    let response = second.response.as_ref().expect("cached response");
    assert_eq!(response.metadata.id, 0x2222, "id follows the new query");
    let ttl = response.answers[0].ttl;
    assert!(ttl < 5, "ttl must be decremented, got {ttl}");
    assert!(ttl > 0, "fresh entries keep a positive ttl, got {ttl}");
}

#[test]
fn expired_entry_is_served_stale_with_lazy_ttl() {
    let cache = Cache::new(0, 30);
    let mut original = context("example.com.", RecordType::A);
    seed(&cache, &mut original, 1);

    sleep(Duration::from_millis(1_200));

    let mut stale = context("example.com.", RecordType::A);
    assert!(cache.get(&mut stale));
    assert!(stale.is_cached);
    let response = stale.response.as_ref().expect("stale response");
    assert_eq!(response.answers[0].ttl, 30, "stale answers use lazy_ttl");
}

#[test]
fn expired_entry_without_lazy_is_dropped() {
    let cache = Cache::new(0, 0);
    let mut original = context("example.com.", RecordType::A);
    seed(&cache, &mut original, 1);
    assert_eq!(cache.len(), 1);

    sleep(Duration::from_millis(1_200));

    let mut missed = context("example.com.", RecordType::A);
    assert!(!cache.get(&mut missed));
    assert!(!missed.is_cached);
    assert!(cache.is_empty(), "expired entries are removed on lookup");
}

#[test]
fn flush_clears_all_entries() {
    let cache = Cache::new(0, 0);
    let mut first = context("a.example.", RecordType::A);
    seed(&cache, &mut first, 300);
    let mut second = context("b.example.", RecordType::A);
    seed(&cache, &mut second, 300);
    assert_eq!(cache.len(), 2);

    cache.flush();
    assert!(cache.is_empty());

    let mut missed = context("a.example.", RecordType::A);
    assert!(!cache.get(&mut missed));
}

#[test]
fn capacity_evicts_the_oldest_entry() {
    let cache = Cache::new(2, 0);
    let mut first = context("a.example.", RecordType::A);
    seed(&cache, &mut first, 300);
    let mut second = context("b.example.", RecordType::A);
    seed(&cache, &mut second, 300);

    let mut third = context("c.example.", RecordType::A);
    seed(&cache, &mut third, 300);
    assert_eq!(cache.len(), 2);

    let mut first_again = context("a.example.", RecordType::A);
    assert!(!cache.get(&mut first_again), "oldest entry was evicted");
    let mut second_again = context("b.example.", RecordType::A);
    assert!(cache.get(&mut second_again));
    let mut third_again = context("c.example.", RecordType::A);
    assert!(cache.get(&mut third_again));
}

#[test]
fn put_skips_unusable_responses() {
    let cache = Cache::new(0, 0);

    // No response at all.
    let plain = context("example.com.", RecordType::A);
    cache.put(&plain);
    assert!(cache.is_empty());

    // Truncated responses are not cacheable.
    let mut truncated_ctx = context("example.com.", RecordType::A);
    let mut truncated = response_for(&truncated_ctx, 300);
    truncated.metadata.truncation = true;
    truncated_ctx.response = Some(truncated);
    cache.put(&truncated_ctx);
    assert!(cache.is_empty());

    // NXDOMAIN is negative caching and out of MVP scope.
    let mut nx_ctx = context("example.com.", RecordType::A);
    let mut nx = response_for(&nx_ctx, 300);
    nx.metadata.response_code = hickory_proto::op::ResponseCode::NXDomain;
    nx_ctx.response = Some(nx);
    cache.put(&nx_ctx);
    assert!(cache.is_empty());

    // Zero-TTL answers forbid caching.
    let mut zero_ctx = context("example.com.", RecordType::A);
    seed(&cache, &mut zero_ctx, 0);
    assert!(cache.is_empty());

    // Empty answer sections have no TTL to cache.
    let mut empty_ctx = context("example.com.", RecordType::A);
    let mut empty = Message::new(0x1234, MessageType::Response, OpCode::Query);
    empty.queries = empty_ctx.query.queries.clone();
    empty_ctx.response = Some(empty);
    cache.put(&empty_ctx);
    assert!(cache.is_empty());
}

#[tokio::test]
async fn executable_hits_and_cache_flush_trait_clears() {
    let cache = Cache::new(0, 0);
    let mut original = context("example.com.", RecordType::A);
    seed(&cache, &mut original, 300);

    let mut hit = context("example.com.", RecordType::A);
    cache.execute(&mut hit).await.expect("execute");
    assert!(hit.has_response());

    CacheFlush::flush(&cache).await.expect("flush");
    assert!(cache.is_empty());
}
