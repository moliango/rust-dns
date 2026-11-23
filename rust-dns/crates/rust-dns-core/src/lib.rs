// SPDX-License-Identifier: GPL-3.0-only
//! DNS core for rust-dns: config, matchers, sequence, cache, upstream, servers.

use std::net::SocketAddr;

use async_trait::async_trait;
use hickory_proto::op::Message;

pub mod cache;
pub mod config;
pub mod context;
pub mod error;
pub mod forward;
pub mod hosts;
pub mod matcher;
pub mod redirect;
pub mod response;
pub mod sequence;
pub mod server;
pub mod stats_bridge;
pub mod upstream;

pub use config::{PluginConfig, RuntimeConfig};
pub use context::QueryContext;
pub use error::{CoreError, Result};

/// A request/response matcher (domain set, response IP set, ...).
pub trait Matcher: Send + Sync {
    fn matches(&self, ctx: &QueryContext) -> bool;
}

/// A sequence action (forward, cache, hosts, redirect, modifier, ...).
#[async_trait]
pub trait Executable: Send + Sync {
    async fn execute(&self, ctx: &mut QueryContext) -> Result<()>;
}

/// A DNS upstream transport (UDP/TCP/DoT).
#[async_trait]
pub trait Upstream: Send + Sync {
    async fn exchange(&self, query: &Message) -> Result<Message>;
}

/// A DNS listener handler (the sequence entry).
#[async_trait]
pub trait Handler: Send + Sync {
    async fn handle(&self, query: Message, client: SocketAddr) -> Option<Message>;
}
