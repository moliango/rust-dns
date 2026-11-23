// SPDX-License-Identifier: GPL-3.0-only
//! Hosts file rewrite: synthesize A/AAAA answers from `domain ip...` lines.

use std::collections::HashMap;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;

use async_trait::async_trait;
use hickory_proto::op::{Message, MessageType, ResponseCode};
use hickory_proto::rr::{Name, RData, Record, RecordType};

use crate::Executable;
use crate::context::QueryContext;
use crate::error::Result;

/// TTL used for synthesized hosts answers.
pub const HOSTS_TTL: u32 = 600;

fn normalize_name(name: &str) -> Option<String> {
    let name = name
        .trim()
        .trim_start_matches('.')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if name.is_empty() { None } else { Some(name) }
}

/// In-memory hosts table.
#[derive(Debug)]
pub struct Hosts {
    v4: HashMap<String, Vec<Ipv4Addr>>,
    v6: HashMap<String, Vec<Ipv6Addr>>,
    ttl: u32,
}

impl Default for Hosts {
    fn default() -> Self {
        Self {
            v4: HashMap::new(),
            v6: HashMap::new(),
            ttl: HOSTS_TTL,
        }
    }
}

impl Hosts {
    /// Load every file into one hosts table.
    ///
    /// Each line is `name ip [ip...]` (the project's generated format). A
    /// standard `/etc/hosts` shape (`ip name [name...]`) is also accepted.
    pub fn load(files: &[PathBuf]) -> Result<Self> {
        let mut hosts = Self::default();
        for path in files {
            let text = match fs::read_to_string(path) {
                Ok(text) => text,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!(
                        "rust-dns: warning: hosts file {} not found; skipping",
                        path.display()
                    );
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            hosts.add_lines(&text, &path.display().to_string());
        }
        Ok(hosts)
    }

    /// Build a hosts table from inline text (used by tests and small configs).
    pub fn from_text(text: &str) -> Self {
        let mut hosts = Self::default();
        hosts.add_lines(text, "<inline>");
        hosts
    }

    /// Override the TTL used for synthesized records.
    pub fn with_ttl(mut self, ttl: u32) -> Self {
        self.ttl = ttl;
        self
    }

    fn add_lines(&mut self, text: &str, origin: &str) {
        for raw in text.lines() {
            let line = raw.split(['#', ';']).next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let tokens: Vec<&str> = line.split_whitespace().collect();
            if tokens.len() < 2 {
                continue;
            }

            if let Ok(ip) = tokens[0].parse::<IpAddr>() {
                // Standard `/etc/hosts`: `ip name [name...]`.
                for name in &tokens[1..] {
                    if let Some(name) = normalize_name(name) {
                        self.add_record(&name, ip);
                    }
                }
                continue;
            }

            // Project format: `name ip [ip...]`.
            let Some(name) = normalize_name(tokens[0]) else {
                continue;
            };
            for token in &tokens[1..] {
                match token.parse::<IpAddr>() {
                    Ok(ip) => self.add_record(&name, ip),
                    Err(_) => {
                        eprintln!("rust-dns: warning: invalid hosts address `{token}` in {origin}")
                    }
                }
            }
        }
    }

    fn add_record(&mut self, name: &str, ip: IpAddr) {
        match ip {
            IpAddr::V4(ip) => {
                let addresses = self.v4.entry(name.to_string()).or_default();
                if !addresses.contains(&ip) {
                    addresses.push(ip);
                }
            }
            IpAddr::V6(ip) => {
                let addresses = self.v6.entry(name.to_string()).or_default();
                if !addresses.contains(&ip) {
                    addresses.push(ip);
                }
            }
        }
    }

    fn name_exists(&self, name: &str) -> bool {
        self.v4.contains_key(name) || self.v6.contains_key(name)
    }

    /// Synthesize a response for the queried name.
    ///
    /// Returns `true` when a response was produced. Hosts are treated as
    /// authoritative: a known name queried for the other address family (or
    /// with no address at all) produces an empty NOERROR answer, which stops
    /// the sequence instead of forwarding.
    pub fn apply(&self, ctx: &mut QueryContext) -> Result<bool> {
        let Some(query) = ctx.query.queries.first() else {
            return Ok(false);
        };
        let Some(name) = normalize_name(&query.name().to_utf8()) else {
            return Ok(false);
        };
        let qname: Name = query.name().clone();
        let query_type = query.query_type();

        let records: Vec<Record> = match query_type {
            RecordType::A => self
                .v4
                .get(&name)
                .map(|addresses| {
                    addresses
                        .iter()
                        .map(|ip| Record::from_rdata(qname.clone(), self.ttl, RData::from(*ip)))
                        .collect()
                })
                .unwrap_or_default(),
            RecordType::AAAA => self
                .v6
                .get(&name)
                .map(|addresses| {
                    addresses
                        .iter()
                        .map(|ip| Record::from_rdata(qname.clone(), self.ttl, RData::from(*ip)))
                        .collect()
                })
                .unwrap_or_default(),
            _ => return Ok(false),
        };

        if !self.name_exists(&name) {
            return Ok(false);
        }

        let mut reply = reply_for(ctx);
        for record in records {
            reply.add_answer(record);
        }
        ctx.set_response(reply);
        Ok(true)
    }
}

/// Build an empty NOERROR reply for the current query.
fn reply_for(ctx: &QueryContext) -> Message {
    let mut reply = Message::new(
        ctx.query.metadata.id,
        MessageType::Response,
        ctx.query.metadata.op_code,
    );
    reply.metadata.response_code = ResponseCode::NoError;
    reply.metadata.recursion_desired = ctx.query.metadata.recursion_desired;
    reply.metadata.recursion_available = true;
    if let Some(query) = ctx.query.queries.first() {
        reply.queries.push(query.clone());
    }
    reply
}

#[async_trait]
impl Executable for Hosts {
    async fn execute(&self, ctx: &mut QueryContext) -> Result<()> {
        self.apply(ctx)?;
        Ok(())
    }
}
