// SPDX-License-Identifier: GPL-3.0-only
//! Domain redirect (`old new` per line).

use std::fs;
use std::path::PathBuf;

use async_trait::async_trait;
use hickory_proto::rr::Name;

use crate::Executable;
use crate::context::QueryContext;
use crate::error::Result;

fn normalize_name(name: &str) -> Option<String> {
    let name = name
        .trim()
        .trim_start_matches('.')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if name.is_empty() { None } else { Some(name) }
}

/// Redirect table: rewrite `old` to `new`.
///
/// A match is on a label boundary, so an entry `old.example new.example`
/// rewrites both `old.example` and `www.old.example` (to `new.example` and
/// `www.new.example`). Rules are tried longest-first so the most specific
/// suffix wins.
#[derive(Debug, Default)]
pub struct Redirect {
    rules: Vec<(String, Name)>,
}

impl Redirect {
    pub fn load(files: &[PathBuf]) -> Result<Self> {
        let mut redirect = Self::default();
        for path in files {
            let text = match fs::read_to_string(path) {
                Ok(text) => text,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!(
                        "rust-dns: warning: redirect file {} not found; skipping",
                        path.display()
                    );
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            redirect.add_lines(&text, &path.display().to_string());
        }
        redirect.finish();
        Ok(redirect)
    }

    /// Build a redirect table from inline text (used by tests).
    pub fn from_text(text: &str) -> Self {
        let mut redirect = Self::default();
        redirect.add_lines(text, "<inline>");
        redirect.finish();
        redirect
    }

    fn add_lines(&mut self, text: &str, origin: &str) {
        for raw in text.lines() {
            let line = raw.split(['#', ';']).next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut tokens = line.split_whitespace();
            let (Some(old), Some(new)) = (tokens.next(), tokens.next()) else {
                continue;
            };
            let Some(old) = normalize_name(old) else {
                continue;
            };
            // Store the replacement as an absolute name; sequence rewrites
            // must never produce a relative DNS name.
            let target = if new.ends_with('.') {
                new.to_string()
            } else {
                format!("{new}.")
            };
            match Name::from_ascii(&target) {
                Ok(new) => self.rules.push((old, new)),
                Err(err) => eprintln!(
                    "rust-dns: warning: invalid redirect target `{new}` in {origin}: {err}"
                ),
            }
        }
    }

    fn finish(&mut self) {
        self.rules
            .sort_by_key(|(old, _)| std::cmp::Reverse(old.len()));
    }

    /// Rewrite the query name when a rule matches. Returns true if changed.
    pub fn apply(&self, ctx: &mut QueryContext) -> bool {
        let Some(query) = ctx.query.queries.first_mut() else {
            return false;
        };
        let Some(current) = normalize_name(&query.name().to_utf8()) else {
            return false;
        };

        for (old, new) in &self.rules {
            if current == *old {
                query.set_name(new.clone());
                return true;
            }
            if !current.ends_with(&format!(".{old}")) {
                continue;
            }
            let prefix = &current[..current.len() - old.len()];
            let target = format!("{prefix}{}", new.to_utf8());
            match Name::from_ascii(&target) {
                Ok(name) => {
                    query.set_name(name);
                    return true;
                }
                Err(_) => continue,
            }
        }
        false
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

#[async_trait]
impl Executable for Redirect {
    async fn execute(&self, ctx: &mut QueryContext) -> Result<()> {
        self.apply(ctx);
        Ok(())
    }
}
