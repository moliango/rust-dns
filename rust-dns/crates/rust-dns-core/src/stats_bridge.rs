// SPDX-License-Identifier: GPL-3.0-only
//! Convert a completed query into the stats API event.
//!
//! The collector performs its own normalization, but answers are already
//! trimmed here so the hot path never hands it more than
//! [`rust_dns_stats_api::types::MAX_ANSWERS`] records.

use hickory_proto::rr::RData;
use rust_dns_stats_api::{QueryEvent, RawAnswer, types::MAX_ANSWERS};

use crate::context::QueryContext;

/// SERVFAIL, used when the sequence produced no response at all.
const SERVFAIL: u16 = 2;

/// Build the stats event for a finished query.
pub fn event_from_context(ctx: &QueryContext) -> QueryEvent {
    let response = ctx.response.as_ref();

    let status = response
        .map(|message| u16::from(message.metadata.response_code))
        .unwrap_or(SERVFAIL);

    let answers: Vec<RawAnswer> = response.map(collect_answers).unwrap_or_default();

    QueryEvent {
        client_ip: ctx.client.ip().to_string(),
        domain: ctx.qname(),
        qtype: u16::from(ctx.qtype()),
        status,
        elapsed_ms: ctx.elapsed_ms(),
        answers,
        is_blocked: ctx.is_blocked,
        is_cached: ctx.is_cached,
        upstream: ctx.upstream.clone().unwrap_or_default(),
        rule: ctx.rule.clone().unwrap_or_default(),
    }
}

fn collect_answers(message: &hickory_proto::op::Message) -> Vec<RawAnswer> {
    message
        .answers
        .iter()
        .filter_map(|record| {
            let (record_type, data) = match &record.data {
                RData::A(address) => (1, address.0.to_string()),
                RData::AAAA(address) => (28, address.0.to_string()),
                _ => return None,
            };
            Some(RawAnswer {
                record_type,
                data,
                ttl: record.ttl,
            })
        })
        .take(MAX_ANSWERS)
        .collect()
}
