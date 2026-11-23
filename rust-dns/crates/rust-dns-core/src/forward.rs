// SPDX-License-Identifier: GPL-3.0-only
//! Forward plugin: run a query against one or more upstreams.
//!
//! Upstreams are tried in batches of `concurrent`: all upstreams in a batch
//! race and the first successful response wins. If a whole batch fails the
//! next batch is tried, so a larger upstream list still gets a chance when
//! the preferred servers are down (design §4).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hickory_proto::op::Message;
use tokio::task::JoinSet;
use tokio::time::{Instant, timeout_at};

use crate::context::QueryContext;
use crate::error::{CoreError, Result};
use crate::{Executable, Upstream};

pub struct Forward {
    pub upstreams: Vec<Arc<dyn Upstream>>,
    /// Number of upstreams raced at the same time (at least 1).
    pub concurrent: usize,
    /// Overall timeout for the whole forward step; zero disables the deadline.
    pub timeout: Duration,
    /// Optional plugin tag used to name the chosen upstream in stats.
    pub tag: Option<String>,
}

impl Forward {
    pub fn new(upstreams: Vec<Arc<dyn Upstream>>, concurrent: usize, timeout: Duration) -> Self {
        Self {
            upstreams,
            concurrent,
            timeout,
            tag: None,
        }
    }

    /// Attach the plugin tag; `ctx.upstream` becomes `<tag>[<index>]`.
    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    fn upstream_name(&self, index: usize) -> String {
        match &self.tag {
            Some(tag) => format!("{tag}[{index}]"),
            None => format!("upstream-{index}"),
        }
    }
}

#[async_trait]
impl Executable for Forward {
    async fn execute(&self, ctx: &mut QueryContext) -> Result<()> {
        if self.upstreams.is_empty() {
            return Err(CoreError::Upstream("forward has no upstreams".into()));
        }

        let batch_size = self.concurrent.max(1);
        let deadline = if self.timeout.is_zero() {
            None
        } else {
            Some(Instant::now() + self.timeout)
        };
        let mut errors: Vec<String> = Vec::new();

        for (batch_index, batch) in self.upstreams.chunks(batch_size).enumerate() {
            let mut tasks: JoinSet<(usize, Result<Message>)> = JoinSet::new();
            for (offset, upstream) in batch.iter().enumerate() {
                let index = batch_index * batch_size + offset;
                let upstream = Arc::clone(upstream);
                let query = ctx.query.clone();
                tasks.spawn(async move { (index, upstream.exchange(&query).await) });
            }

            let (winner, batch_errors) = wait_for_winner(&mut tasks, deadline).await;
            errors.extend(batch_errors);

            if let Some((index, mut response)) = winner {
                // Some servers rewrite the ID; keep the client's transaction id.
                if response.metadata.id != ctx.query.metadata.id {
                    response.metadata.id = ctx.query.metadata.id;
                }
                ctx.response = Some(response);
                ctx.upstream = Some(self.upstream_name(index));
                return Ok(());
            }

            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                errors.push(format!("forward timed out after {:?}", self.timeout));
                break;
            }
        }

        Err(CoreError::Upstream(if errors.is_empty() {
            "no upstream answered".to_string()
        } else {
            errors.join("; ")
        }))
    }
}

/// Wait for the first successful exchange in `tasks`.
///
/// Returns `(Some((index, message)), errors)` on success and `(None, errors)`
/// when every task failed or the deadline elapsed. Remaining tasks are aborted.
async fn wait_for_winner(
    tasks: &mut JoinSet<(usize, Result<Message>)>,
    deadline: Option<Instant>,
) -> (Option<(usize, Message)>, Vec<String>) {
    let mut errors = Vec::new();
    loop {
        let joined = match deadline {
            None => tasks.join_next().await,
            Some(deadline) => match timeout_at(deadline, tasks.join_next()).await {
                Ok(joined) => joined,
                Err(_) => {
                    tasks.abort_all();
                    errors.push("forward deadline elapsed".to_string());
                    return (None, errors);
                }
            },
        };

        match joined {
            None => return (None, errors),
            Some(Ok((index, Ok(response)))) => {
                tasks.abort_all();
                return (Some((index, response)), errors);
            }
            Some(Ok((index, Err(err)))) => {
                errors.push(format!("upstream {index}: {err}"));
            }
            Some(Err(join_err)) => {
                errors.push(format!("upstream task failed: {join_err}"));
            }
        }
    }
}
