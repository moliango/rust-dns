// SPDX-License-Identifier: GPL-3.0-only
//! Sequence DSL and plugin registry.

use std::collections::HashMap;
use std::sync::Arc;

use hickory_proto::op::{Message, MessageType, ResponseCode};

use crate::config::RuntimeConfig;
use crate::context::QueryContext;
use crate::error::{CoreError, Result};
use crate::{Executable, Matcher};

/// Tag-indexed plugin registry used to resolve sequence rules.
///
/// A tag may be registered as a matcher, an executable, or both (for example
/// `hosts`, which is a `domain_set` in the config but acts as a file action
/// when it appears bare in a sequence).
#[derive(Default)]
pub struct Registry {
    matchers: HashMap<String, Arc<dyn Matcher>>,
    executables: HashMap<String, Arc<dyn Executable>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_matcher(&mut self, tag: impl Into<String>, matcher: Arc<dyn Matcher>) {
        self.matchers.insert(tag.into(), matcher);
    }

    pub fn insert_executable(&mut self, tag: impl Into<String>, executable: Arc<dyn Executable>) {
        self.executables.insert(tag.into(), executable);
    }

    pub fn matcher(&self, tag: &str) -> Option<Arc<dyn Matcher>> {
        self.matchers.get(tag).cloned()
    }

    pub fn executable(&self, tag: &str) -> Option<Arc<dyn Executable>> {
        self.executables.get(tag).cloned()
    }
}

enum Action {
    Execute(Arc<dyn Executable>),
    Reject,
    /// A bare matcher tag: it always matches and does nothing.
    Noop,
}

struct Rule {
    /// Label recorded in [`QueryContext::rule`] for stats.
    label: String,
    /// `None` means the rule is unconditional.
    matcher: Option<Arc<dyn Matcher>>,
    action: Action,
}

/// An ordered list of rules from a `sequence` plugin.
pub struct Sequence {
    rules: Vec<Rule>,
}

impl Sequence {
    /// Build the entry sequence from the runtime config.
    ///
    /// The `sequence` plugin tagged `main` is preferred; otherwise the first
    /// `sequence` plugin is used. A config without a sequence plugin is an
    /// error, because nothing could answer queries.
    pub fn build(config: &RuntimeConfig, registry: &Registry) -> Result<Self> {
        let plugin = config
            .plugins
            .iter()
            .find(|plugin| plugin.tag == "main" && plugin.kind == "sequence")
            .or_else(|| {
                config
                    .plugins
                    .iter()
                    .find(|plugin| plugin.kind == "sequence")
            })
            .ok_or_else(|| {
                CoreError::Config("no sequence plugin (expected tag `main`)".to_string())
            })?;

        let mut rules = Vec::new();
        match plugin.args.get("rules") {
            Some(serde_json::Value::Array(items)) => {
                for item in items {
                    match item.as_str() {
                        Some(text) => {
                            if let Some(rule) = parse_rule(text, registry) {
                                rules.push(rule);
                            }
                        }
                        None => eprintln!(
                            "rust-dns: warning: ignoring non-string sequence rule in tag `{}`",
                            plugin.tag
                        ),
                    }
                }
            }
            Some(serde_json::Value::String(text)) => {
                if let Some(rule) = parse_rule(text, registry) {
                    rules.push(rule);
                }
            }
            Some(_) => eprintln!(
                "rust-dns: warning: `{}` has an invalid `rules` value (expected an array)",
                plugin.tag
            ),
            None => {}
        }

        Ok(Self { rules })
    }

    /// Evaluate the rules in order.
    ///
    /// Evaluation stops as soon as an action produces a response. When no rule
    /// produces one, `ctx.response` is left untouched so the caller can answer
    /// with SERVFAIL.
    pub async fn execute(&self, ctx: &mut QueryContext) -> Result<()> {
        for rule in &self.rules {
            if let Some(matcher) = &rule.matcher
                && !matcher.matches(ctx)
            {
                continue;
            }

            match &rule.action {
                Action::Noop => continue,
                Action::Reject => {
                    ctx.rule = Some(rule.label.clone());
                    ctx.is_blocked = true;
                    let reply = nxdomain(ctx);
                    ctx.set_response(reply);
                    return Ok(());
                }
                Action::Execute(executable) => {
                    ctx.rule = Some(rule.label.clone());
                    executable.execute(ctx).await?;
                    if ctx.has_response() {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

fn parse_rule(text: &str, registry: &Registry) -> Option<Rule> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    let mut parts = text.splitn(2, "->");
    let left = parts.next().unwrap_or("").trim();
    match parts.next() {
        None => parse_bare(left, registry),
        Some(right) => {
            let right = right.trim();
            if left.is_empty() || right.is_empty() {
                eprintln!("rust-dns: warning: invalid sequence rule `{text}`");
                return None;
            }
            let Some(matcher) = registry.matcher(left) else {
                warn_unknown(left);
                return None;
            };
            let action = if right == "reject" {
                Action::Reject
            } else {
                match registry.executable(right) {
                    Some(executable) => Action::Execute(executable),
                    None => {
                        warn_unknown(right);
                        return None;
                    }
                }
            };
            Some(Rule {
                label: left.to_string(),
                matcher: Some(matcher),
                action,
            })
        }
    }
}

fn parse_bare(tag: &str, registry: &Registry) -> Option<Rule> {
    if tag.is_empty() {
        return None;
    }
    if tag == "reject" {
        return Some(Rule {
            label: "reject".to_string(),
            matcher: None,
            action: Action::Reject,
        });
    }
    // Executables take precedence so `hosts`/`redirect` behave as file
    // actions when they appear bare, even if also registered as a matcher.
    if let Some(executable) = registry.executable(tag) {
        return Some(Rule {
            label: tag.to_string(),
            matcher: None,
            action: Action::Execute(executable),
        });
    }
    if registry.matcher(tag).is_some() {
        return Some(Rule {
            label: tag.to_string(),
            matcher: None,
            action: Action::Noop,
        });
    }
    warn_unknown(tag);
    None
}

fn warn_unknown(tag: &str) {
    eprintln!("rust-dns: warning: unknown sequence plugin tag `{tag}`; rule skipped");
}

/// Build an NXDOMAIN reply that echoes the query's id and question.
fn nxdomain(ctx: &QueryContext) -> Message {
    let mut reply = Message::new(
        ctx.query.metadata.id,
        MessageType::Response,
        ctx.query.metadata.op_code,
    );
    reply.metadata.response_code = ResponseCode::NXDomain;
    reply.metadata.recursion_desired = ctx.query.metadata.recursion_desired;
    reply.metadata.recursion_available = true;
    if let Some(query) = ctx.query.queries.first() {
        reply.queries.push(query.clone());
    }
    reply
}
