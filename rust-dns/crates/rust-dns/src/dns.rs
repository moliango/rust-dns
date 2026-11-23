// SPDX-License-Identifier: GPL-3.0-only
//! Build the DNS query pipeline from a generated runtime configuration.
//!
//! `config.sh` emits typed plugins (domain/ip sets, forwards, cache, response
//! modifier, sequence, servers). This module turns them into the
//! [`rust_dns_core`] registry, builds the entry [`Sequence`] and exposes a
//! [`Handler`] the UDP/TCP listeners can share.
//!
//! Only the plugin types used by the generated configuration are understood;
//! `stats_collector` and `bootstrap` are consumed by other layers and are
//! skipped here. Unknown plugin types are warned about and ignored so a newer
//! generator cannot take the service down.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use hickory_proto::op::{Message, MessageType};
use rust_dns_core::cache::Cache;
use rust_dns_core::config::{PluginConfig, RuntimeConfig};
use rust_dns_core::context::QueryContext;
use rust_dns_core::error::{CoreError, Result};
use rust_dns_core::forward::Forward;
use rust_dns_core::hosts::Hosts;
use rust_dns_core::matcher::{DomainMatcher, DomainSet, IpSet, ResponseIpMatcher};
use rust_dns_core::redirect::Redirect;
use rust_dns_core::response::ResponseModifier;
use rust_dns_core::sequence::{Registry, Sequence};
use rust_dns_core::stats_bridge::event_from_context;
use rust_dns_core::upstream::{DEFAULT_TIMEOUT, build_upstream};
use rust_dns_core::{Handler, Upstream};
use rust_dns_stats_api::StatsCollector;
use serde_json::Value;

/// Default number of upstreams raced by a `forward` plugin.
const DEFAULT_CONCURRENT: usize = 2;

/// A DNS listener declared by a `udp_server` / `tcp_server` plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListenerSpec {
    pub kind: ServerKind,
    pub listen: SocketAddr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerKind {
    Udp,
    Tcp,
}

/// The shared query handler: entry sequence plus the post-sequence hooks.
pub struct DnsService {
    sequence: Arc<Sequence>,
    cache: Option<Arc<Cache>>,
    modifier: ResponseModifier,
    collector: Option<Arc<StatsCollector>>,
}

impl DnsService {
    /// Build the pipeline from a parsed runtime configuration.
    ///
    /// `collector` is optional so the caller can build (and validate) the
    /// pipeline before starting the stats subsystem; use [`Self::set_collector`]
    /// to attach it afterwards.
    pub fn build(config: &RuntimeConfig, collector: Option<Arc<StatsCollector>>) -> Result<Self> {
        let mut registry = Registry::new();
        let mut cache: Option<Arc<Cache>> = None;
        let mut modifier = ResponseModifier::default();

        for plugin in &config.plugins {
            match plugin.kind.as_str() {
                "domain_set" => {
                    let files = files_from(&plugin.args);
                    let set = Arc::new(DomainSet::load(&files)?);
                    registry.insert_matcher(
                        plugin.tag.clone(),
                        Arc::new(DomainMatcher {
                            set: Arc::clone(&set),
                        }),
                    );

                    // `hosts` and `redirect` are emitted as `domain_set` but act
                    // as file actions when they appear bare in a sequence.
                    match plugin.tag.as_str() {
                        "hosts" => {
                            let hosts = Hosts::load(&files)?;
                            registry.insert_executable(plugin.tag.clone(), Arc::new(hosts));
                        }
                        "redirect" => {
                            let redirect = Redirect::load(&files)?;
                            registry.insert_executable(plugin.tag.clone(), Arc::new(redirect));
                        }
                        _ => {}
                    }
                }
                "ip_set" => {
                    let set = Arc::new(ip_set_from(plugin)?);
                    registry
                        .insert_matcher(plugin.tag.clone(), Arc::new(ResponseIpMatcher { set }));
                }
                "forward" => {
                    let upstreams = upstreams_from(plugin)?;
                    let concurrent = plugin
                        .args
                        .get("concurrent")
                        .and_then(json_u64)
                        .unwrap_or(DEFAULT_CONCURRENT as u64)
                        as usize;
                    let forward = Forward::new(upstreams, concurrent.max(1), DEFAULT_TIMEOUT)
                        .with_tag(plugin.tag.clone());
                    registry.insert_executable(plugin.tag.clone(), Arc::new(forward));
                }
                "cache" => {
                    let size = plugin.args.get("size").and_then(json_u64).unwrap_or(0) as usize;
                    let lazy = plugin
                        .args
                        .get("lazy_cache_ttl")
                        .and_then(json_u64)
                        .unwrap_or(0)
                        .min(u64::from(u32::MAX)) as u32;
                    let cache_plugin = Arc::new(Cache::new(size, lazy));
                    cache = Some(Arc::clone(&cache_plugin));
                    registry.insert_executable(plugin.tag.clone(), cache_plugin);
                }
                "response_modifier" => {
                    let built = ResponseModifier {
                        minimal_ttl: arg_u32(&plugin.args, "minimal_ttl"),
                        maximum_ttl: arg_u32(&plugin.args, "maximum_ttl"),
                        reject_type65: plugin
                            .args
                            .get("reject_type65")
                            .and_then(json_bool)
                            .unwrap_or(false),
                    };
                    modifier = built;
                    registry.insert_executable(plugin.tag.clone(), Arc::new(built));
                }
                // Consumed by other layers: aggregation, bootstrap, the
                // sequence builder and the listener factory.
                "stats_collector" | "bootstrap" | "sequence" | "udp_server" | "tcp_server" => {}
                other => eprintln!(
                    "rust-dns: warning: plugin `{}` has unsupported type `{other}`; skipping",
                    plugin.tag
                ),
            }
        }

        let sequence = Arc::new(Sequence::build(config, &registry)?);
        if sequence.is_empty() {
            eprintln!("rust-dns: warning: entry sequence is empty; queries will answer SERVFAIL");
        }

        Ok(Self {
            sequence,
            cache,
            modifier,
            collector,
        })
    }

    /// Attach the stats collector after construction.
    pub fn set_collector(&mut self, collector: Arc<StatsCollector>) {
        self.collector = Some(collector);
    }

    /// The cache used by this pipeline, for `/api/v1/cache/flush` wiring.
    pub fn cache(&self) -> Option<Arc<Cache>> {
        self.cache.clone()
    }
}

#[async_trait]
impl Handler for DnsService {
    async fn handle(&self, query: Message, client: SocketAddr) -> Option<Message> {
        let mut ctx = QueryContext::new(query, client);

        match self.sequence.execute(&mut ctx).await {
            Ok(()) if ctx.has_response() => {}
            Ok(()) => {
                // Every sequence rule missed; answer SERVFAIL.
                let reply = ctx.servfail();
                ctx.set_response(reply);
            }
            Err(err) => {
                eprintln!(
                    "rust-dns: query `{}` failed in sequence: {err}",
                    ctx.qname()
                );
                let reply = ctx.servfail();
                ctx.set_response(reply);
            }
        }

        // `modify_response` sits before `forward_*` in the generated sequence,
        // where it is a no-op. Apply it again to the final response.
        if let Err(err) = self.modifier.apply(&mut ctx) {
            eprintln!("rust-dns: response modifier failed: {err}");
        }

        if let Some(response) = ctx.response.as_mut() {
            normalize_response(response, &ctx.query);
        }

        // The cache sees the post-modifier response. `Cache::put` itself rejects
        // negative, truncated and answerless responses.
        if !ctx.is_cached
            && let Some(cache) = &self.cache
        {
            cache.put(&ctx);
        }

        // Stats must observe the final response, so record after everything.
        if let Some(collector) = &self.collector {
            collector.record(event_from_context(&ctx));
        }

        ctx.take_response()
    }
}

/// Ensure a forwarded/synthesized response is a well-formed reply to `query`.
fn normalize_response(response: &mut Message, query: &Message) {
    response.metadata.id = query.metadata.id;
    response.metadata.message_type = MessageType::Response;
    response.metadata.op_code = query.metadata.op_code;
    response.metadata.recursion_desired = query.metadata.recursion_desired;
    response.metadata.recursion_available = true;
    if response.queries.is_empty() {
        response.queries = query.queries.clone();
    }
}

/// Parse the `udp_server` / `tcp_server` plugins into listener specs.
pub fn listeners(config: &RuntimeConfig) -> Result<Vec<ListenerSpec>> {
    let mut specs = Vec::new();
    for plugin in &config.plugins {
        let kind = match plugin.kind.as_str() {
            "udp_server" => ServerKind::Udp,
            "tcp_server" => ServerKind::Tcp,
            // Accepted by `check` but not implemented in the MVP.
            "http_server" | "quic_server" => {
                eprintln!(
                    "rust-dns: warning: server plugin `{}` type `{}` is not supported in v1; no listener started",
                    plugin.tag, plugin.kind
                );
                continue;
            }
            _ => continue,
        };

        let listen = plugin
            .args
            .get("listen")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                CoreError::Config(format!(
                    "server plugin `{}` is missing a string `listen`",
                    plugin.tag
                ))
            })?;
        let listen: SocketAddr = listen.parse().map_err(|err| {
            CoreError::Config(format!(
                "server plugin `{}` has invalid `listen` {listen:?}: {err}",
                plugin.tag
            ))
        })?;

        if let Some(entry) = plugin.args.get("entry").and_then(|value| value.as_str()) {
            let known = config
                .plugins
                .iter()
                .any(|other| other.tag == entry && other.kind == "sequence");
            if !known {
                return Err(CoreError::Config(format!(
                    "server plugin `{}` references unknown sequence entry `{entry}`",
                    plugin.tag
                )));
            }
        }

        specs.push(ListenerSpec { kind, listen });
    }
    Ok(specs)
}

fn upstreams_from(plugin: &PluginConfig) -> Result<Vec<Arc<dyn Upstream>>> {
    let entries = plugin
        .args
        .get("upstreams")
        .and_then(|value| value.as_array())
        .ok_or_else(|| {
            CoreError::Config(format!(
                "forward plugin `{}` requires an `upstreams` array",
                plugin.tag
            ))
        })?;

    let mut upstreams: Vec<Arc<dyn Upstream>> = Vec::new();
    for entry in entries {
        let Some((addr, verify)) = parse_upstream_entry(entry) else {
            eprintln!(
                "rust-dns: warning: forward plugin `{}` has an invalid upstream entry; skipping",
                plugin.tag
            );
            continue;
        };
        match build_upstream(&addr, verify, DEFAULT_TIMEOUT) {
            Ok(upstream) => upstreams.push(upstream),
            // One unsupported upstream (for example DoH) must not take the
            // whole service down; the remaining upstreams still serve.
            Err(err) => eprintln!(
                "rust-dns: warning: forward plugin `{}` upstream {addr:?} skipped: {err}",
                plugin.tag
            ),
        }
    }

    if upstreams.is_empty() {
        eprintln!(
            "rust-dns: warning: forward plugin `{}` has no usable upstreams; queries will SERVFAIL",
            plugin.tag
        );
    }
    Ok(upstreams)
}

/// Read `(addr, verify)` from an upstream object or a bare address string.
fn parse_upstream_entry(entry: &Value) -> Option<(String, bool)> {
    match entry {
        Value::String(addr) => Some((addr.clone(), true)),
        Value::Object(map) => {
            let addr = map.get("addr")?.as_str()?.to_string();
            // `verify` wins; `insecure_skip_verify` is the inverted alias.
            let verify = map
                .get("verify")
                .and_then(json_bool)
                .or_else(|| {
                    map.get("insecure_skip_verify")
                        .and_then(json_bool)
                        .map(|skip| !skip)
                })
                .unwrap_or(true);
            Some((addr, verify))
        }
        _ => None,
    }
}

fn ip_set_from(plugin: &PluginConfig) -> Result<IpSet> {
    let files = files_from(&plugin.args);
    let inline: Vec<String> = plugin
        .args
        .get("ips")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    if files.is_empty() {
        if inline.is_empty() {
            return Ok(IpSet::default());
        }
        return Ok(IpSet::from_text(&inline.join("\n")));
    }

    if !inline.is_empty() {
        eprintln!(
            "rust-dns: warning: ip_set `{}` ignores inline `ips` when `files` is set",
            plugin.tag
        );
    }
    IpSet::load(&files)
}

fn files_from(args: &Value) -> Vec<PathBuf> {
    args.get("files")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(PathBuf::from))
                .collect()
        })
        .unwrap_or_default()
}

fn arg_u32(args: &Value, key: &str) -> u32 {
    args.get(key)
        .and_then(json_u64)
        .map(|value| value.min(u64::from(u32::MAX)) as u32)
        .unwrap_or(0)
}

fn json_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse().ok(),
        Value::Bool(flag) => Some(u64::from(*flag)),
        _ => None,
    }
}

fn json_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::Number(number) => number.as_i64().map(|value| value != 0),
        Value::String(text) => match text.as_str() {
            "1" | "true" | "yes" | "on" | "enabled" => Some(true),
            "0" | "false" | "no" | "" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_upstream_entries_and_verify_aliases() {
        assert_eq!(
            parse_upstream_entry(&json!("udp://1.2.3.4")),
            Some(("udp://1.2.3.4".to_string(), true))
        );
        assert_eq!(
            parse_upstream_entry(&json!({"addr": "tls://8.8.8.8", "verify": false})),
            Some(("tls://8.8.8.8".to_string(), false))
        );
        assert_eq!(
            parse_upstream_entry(&json!({"addr": "tls://8.8.8.8", "insecure_skip_verify": true})),
            Some(("tls://8.8.8.8".to_string(), false))
        );
        assert_eq!(
            parse_upstream_entry(
                &json!({"addr": "tls://8.8.8.8", "verify": true, "insecure_skip_verify": true})
            ),
            Some(("tls://8.8.8.8".to_string(), true))
        );
        assert_eq!(parse_upstream_entry(&json!({"nope": 1})), None);
    }

    #[test]
    fn ip_set_accepts_inline_ips() {
        let plugin = PluginConfig {
            tag: "cloudflare_ip".to_string(),
            kind: "ip_set".to_string(),
            args: json!({"ips": ["104.16.0.1", "104.16.0.2"]}),
        };
        let set = ip_set_from(&plugin).unwrap();
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn json_helpers_accept_strings_and_bools() {
        assert_eq!(json_bool(&json!(true)), Some(true));
        assert_eq!(json_bool(&json!("0")), Some(false));
        assert_eq!(json_bool(&json!("maybe")), None);
        assert_eq!(json_u64(&json!(42)), Some(42));
        assert_eq!(json_u64(&json!("7")), Some(7));
        assert_eq!(arg_u32(&json!({"maximum_ttl": 60}), "maximum_ttl"), 60);
    }

    #[test]
    fn listeners_parse_servers_and_reject_unknown_entries() {
        let config: RuntimeConfig = serde_json::from_value(json!({
            "plugins": [
                {"tag": "main", "type": "sequence", "args": {"rules": []}},
                {"tag": "dns_udp", "type": "udp_server", "args": {"entry": "main", "listen": "127.0.0.1:5335"}},
                {"tag": "dns_tcp", "type": "tcp_server", "args": {"entry": "main", "listen": "[::1]:5335"}},
                {"tag": "dns_doh", "type": "http_server", "args": {"listen": "127.0.0.1:8443"}}
            ]
        }))
        .unwrap();
        let specs = listeners(&config).unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].kind, ServerKind::Udp);
        assert_eq!(specs[0].listen.port(), 5335);
        assert_eq!(specs[1].kind, ServerKind::Tcp);

        let bad_entry: RuntimeConfig = serde_json::from_value(json!({
            "plugins": [
                {"tag": "main", "type": "sequence", "args": {"rules": []}},
                {"tag": "dns_udp", "type": "udp_server", "args": {"entry": "missing", "listen": "127.0.0.1:5335"}}
            ]
        }))
        .unwrap();
        assert!(listeners(&bad_entry).is_err());

        let bad_listen: RuntimeConfig = serde_json::from_value(json!({
            "plugins": [
                {"tag": "dns_udp", "type": "udp_server", "args": {"listen": "not-an-addr"}}
            ]
        }))
        .unwrap();
        assert!(listeners(&bad_listen).is_err());
    }
}
