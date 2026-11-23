// SPDX-License-Identifier: GPL-3.0-only
//! Per-query context passed through the sequence.

use std::net::SocketAddr;
use std::time::Instant;

use hickory_proto::op::{Message, MessageType, ResponseCode};
use hickory_proto::rr::{DNSClass, RecordType};

#[derive(Debug)]
pub struct QueryContext {
    pub query: Message,
    pub client: SocketAddr,
    pub response: Option<Message>,
    pub started: Instant,
    pub upstream: Option<String>,
    pub rule: Option<String>,
    pub is_cached: bool,
    pub is_blocked: bool,
}

impl QueryContext {
    pub fn new(query: Message, client: SocketAddr) -> Self {
        Self {
            query,
            client,
            response: None,
            started: Instant::now(),
            upstream: None,
            rule: None,
            is_cached: false,
            is_blocked: false,
        }
    }

    /// Lowercase query name without a trailing dot.
    pub fn qname(&self) -> String {
        self.query
            .queries
            .first()
            .map(|q| {
                q.name()
                    .to_utf8()
                    .trim_end_matches('.')
                    .to_ascii_lowercase()
            })
            .unwrap_or_default()
    }

    pub fn qtype(&self) -> RecordType {
        self.query
            .queries
            .first()
            .map(|q| q.query_type())
            .unwrap_or(RecordType::A)
    }

    pub fn qclass(&self) -> DNSClass {
        self.query
            .queries
            .first()
            .map(|q| q.query_class())
            .unwrap_or(DNSClass::IN)
    }

    pub fn has_response(&self) -> bool {
        self.response.is_some()
    }

    pub fn set_response(&mut self, response: Message) {
        self.response = Some(response);
    }

    pub fn take_response(&mut self) -> Option<Message> {
        self.response.take()
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Build a SERVFAIL reply for the current query.
    pub fn servfail(&self) -> Message {
        let mut reply = Message::new(
            self.query.metadata.id,
            MessageType::Response,
            self.query.metadata.op_code,
        );
        reply.metadata.response_code = ResponseCode::ServFail;
        reply.metadata.recursion_desired = self.query.metadata.recursion_desired;
        reply.metadata.recursion_available = true;
        if let Some(q) = self.query.queries.first() {
            reply.queries.push(q.clone());
        }
        reply
    }
}
