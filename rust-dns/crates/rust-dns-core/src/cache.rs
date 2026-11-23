// SPDX-License-Identifier: GPL-3.0-only
//! TTL cache for responses.
//!
//! Design §6: the key is the lowercase query name plus qtype, qclass and the
//! EDNS DO bit. The value is the response message, its insertion time and the
//! smallest answer TTL. Fresh hits decrement every answer TTL by the elapsed
//! whole seconds; expired entries are served as stale with `lazy_ttl` when
//! configured, otherwise they are dropped.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use async_trait::async_trait;
use hickory_proto::op::{Message, MessageType, ResponseCode};

use crate::Executable;
use crate::context::QueryContext;
use crate::error::Result;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    name: String,
    qtype: u16,
    qclass: u16,
    dnssec_ok: bool,
}

impl CacheKey {
    fn from_context(ctx: &QueryContext) -> Self {
        Self {
            name: ctx.qname(),
            qtype: u16::from(ctx.qtype()),
            qclass: u16::from(ctx.qclass()),
            dnssec_ok: ctx
                .query
                .edns
                .as_ref()
                .map(|edns| edns.flags().dnssec_ok)
                .unwrap_or(false),
        }
    }
}

#[derive(Debug, Clone)]
struct CacheEntry {
    message: Message,
    inserted: Instant,
    min_ttl: u32,
}

#[derive(Debug, Default)]
struct CacheInner {
    entries: HashMap<CacheKey, CacheEntry>,
}

/// A bounded TTL response cache.
#[derive(Debug)]
pub struct Cache {
    capacity: usize,
    lazy_ttl: u32,
    inner: Mutex<CacheInner>,
}

impl Default for Cache {
    fn default() -> Self {
        Self::new(0, 0)
    }
}

impl Cache {
    /// `capacity == 0` means unbounded; `lazy_ttl == 0` disables stale serving.
    pub fn new(capacity: usize, lazy_ttl: u32) -> Self {
        Self {
            capacity,
            lazy_ttl,
            inner: Mutex::new(CacheInner::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, CacheInner> {
        // A panicking query thread must not poison the cache for the rest of
        // the daemon; recover the inner map and keep serving.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Look up a response. On hit, sets `ctx.response` and returns true.
    ///
    /// Fresh entries are returned with every answer TTL decremented by the
    /// elapsed whole seconds. Expired entries are returned with `lazy_ttl`
    /// when lazy serving is enabled. The response id and question section are
    /// rewritten to match the current query so clients can correlate it.
    pub fn get(&self, ctx: &mut QueryContext) -> bool {
        let key = CacheKey::from_context(ctx);
        let mut inner = self.lock();

        let Some(entry) = inner.entries.get(&key) else {
            return false;
        };

        let elapsed = entry.inserted.elapsed().as_secs().min(u64::from(u32::MAX)) as u32;
        let mut message = entry.message.clone();

        if elapsed < entry.min_ttl {
            for answer in &mut message.answers {
                answer.ttl = answer.ttl.saturating_sub(elapsed);
            }
        } else if self.lazy_ttl > 0 {
            for answer in &mut message.answers {
                answer.ttl = self.lazy_ttl;
            }
        } else {
            inner.entries.remove(&key);
            return false;
        }

        drop(inner);

        message.metadata.id = ctx.query.metadata.id;
        message.queries = ctx.query.queries.clone();
        ctx.response = Some(message);
        ctx.is_cached = true;
        true
    }

    /// Store the current response.
    ///
    /// Only positive, non-truncated responses with at least one answer and a
    /// non-zero TTL are cached. Negative caching and dump/load are out of MVP
    /// scope.
    pub fn put(&self, ctx: &QueryContext) {
        let Some(response) = ctx.response.as_ref() else {
            return;
        };
        if response.metadata.message_type != MessageType::Response
            || response.metadata.truncation
            || response.metadata.response_code != ResponseCode::NoError
        {
            return;
        }

        let Some(min_ttl) = response.answers.iter().map(|record| record.ttl).min() else {
            return;
        };
        if min_ttl == 0 {
            // A zero TTL explicitly forbids caching.
            return;
        }

        let key = CacheKey::from_context(ctx);
        let mut inner = self.lock();

        if self.capacity > 0
            && !inner.entries.contains_key(&key)
            && inner.entries.len() >= self.capacity
            && let Some(oldest) = inner
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.inserted)
                .map(|(key, _)| key.clone())
        {
            inner.entries.remove(&oldest);
        }

        inner.entries.insert(
            key,
            CacheEntry {
                message: response.clone(),
                inserted: Instant::now(),
                min_ttl,
            },
        );
    }

    /// Drop every cached response.
    pub fn flush(&self) {
        self.lock().entries.clear();
    }

    /// Number of live entries (used by tests and diagnostics).
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Whether the cache currently holds no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl Executable for Cache {
    async fn execute(&self, ctx: &mut QueryContext) -> Result<()> {
        self.get(ctx);
        Ok(())
    }
}

#[async_trait]
impl rust_dns_stats_api::CacheFlush for Cache {
    async fn flush(&self) -> std::result::Result<(), String> {
        Cache::flush(self);
        Ok(())
    }
}
