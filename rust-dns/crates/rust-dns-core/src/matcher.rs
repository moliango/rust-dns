// SPDX-License-Identifier: GPL-3.0-only
//! Domain and response-IP matchers.

use std::collections::HashSet;
use std::fs;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use ipnet::IpNet;
use regex::{Regex, RegexBuilder};

use crate::Matcher;
use crate::context::QueryContext;
use crate::error::Result;

/// Normalize a domain name: lowercase, trim surrounding whitespace, drop a
/// leading dot and the trailing root dot.
fn normalize_name(name: &str) -> Option<String> {
    let name = name
        .trim()
        .trim_start_matches('.')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if name.is_empty() { None } else { Some(name) }
}

/// A set of domain rules loaded from one or more text files.
///
/// Supported line forms (comments start with `#` or `;`, blank lines are
/// ignored):
///
/// - bare `example.com` and `domain:example.com` - the name itself and any
///   subdomain (`www.example.com`) match;
/// - `full:example.com` - exact match only;
/// - `keyword:tracker` - substring match;
/// - `regexp:^ads[0-9]+\.` - regular-expression match (case-insensitive).
#[derive(Debug, Default)]
pub struct DomainSet {
    full: HashSet<String>,
    suffix: HashSet<String>,
    keywords: Vec<String>,
    regexes: Vec<Regex>,
}

impl DomainSet {
    /// Load all files into one set.
    pub fn load(files: &[PathBuf]) -> Result<Self> {
        let mut set = Self::default();
        for path in files {
            let text = match fs::read_to_string(path) {
                Ok(text) => text,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!(
                        "rust-dns: warning: domain set file {} not found; skipping",
                        path.display()
                    );
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            set.add_lines(&text, &path.display().to_string());
        }
        Ok(set)
    }

    /// Build a set from inline text (used by tests and small configs).
    pub fn from_text(text: &str) -> Self {
        let mut set = Self::default();
        set.add_lines(text, "<inline>");
        set
    }

    fn add_lines(&mut self, text: &str, origin: &str) {
        for raw in text.lines() {
            self.add_rule(raw, origin);
        }
    }

    fn add_rule(&mut self, raw: &str, origin: &str) {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            return;
        }

        let (kind, value) = if let Some(v) = line.strip_prefix("domain:") {
            ("domain", v)
        } else if let Some(v) = line.strip_prefix("full:") {
            ("full", v)
        } else if let Some(v) = line.strip_prefix("keyword:") {
            ("keyword", v)
        } else if let Some(v) = line.strip_prefix("regexp:") {
            ("regexp", v)
        } else {
            ("domain", line)
        };
        let value = value.trim();
        if value.is_empty() {
            return;
        }

        match kind {
            "full" => {
                if let Some(name) = normalize_name(value) {
                    self.full.insert(name);
                }
            }
            "keyword" => self.keywords.push(value.to_ascii_lowercase()),
            "regexp" => match RegexBuilder::new(value).case_insensitive(true).build() {
                Ok(regex) => self.regexes.push(regex),
                Err(err) => {
                    eprintln!(
                        "rust-dns: warning: invalid domain regexp `{value}` in {origin}: {err}"
                    );
                }
            },
            // Bare names and `domain:` entries match the name and its label
            // boundaries (example.com, www.example.com).
            _ => {
                if let Some(name) = normalize_name(value) {
                    self.suffix.insert(name);
                }
            }
        }
    }

    /// Test whether a query name matches any rule.
    pub fn contains(&self, name: &str) -> bool {
        let Some(name) = normalize_name(name) else {
            return false;
        };

        if self.full.contains(&name) {
            return true;
        }
        if self.keywords.iter().any(|keyword| name.contains(keyword)) {
            return true;
        }
        if self.regexes.iter().any(|regex| regex.is_match(&name)) {
            return true;
        }

        // Bare/`domain:` rules match the name itself and its label suffixes.
        if self.suffix.contains(&name) {
            return true;
        }
        let mut suffix = name.as_str();
        while let Some(index) = suffix.find('.') {
            suffix = &suffix[index + 1..];
            if self.suffix.contains(suffix) {
                return true;
            }
        }
        false
    }

    pub fn len(&self) -> usize {
        self.full.len() + self.suffix.len() + self.keywords.len() + self.regexes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A set of IP addresses and CIDR ranges.
#[derive(Debug, Default)]
pub struct IpSet {
    nets: Vec<IpNet>,
}

impl IpSet {
    /// Load all files into one set. Unparseable lines are skipped with a
    /// warning so a single bad line cannot bring the whole service down.
    pub fn load(files: &[PathBuf]) -> Result<Self> {
        let mut set = Self::default();
        for path in files {
            let text = match fs::read_to_string(path) {
                Ok(text) => text,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!(
                        "rust-dns: warning: IP set file {} not found; skipping",
                        path.display()
                    );
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            set.add_lines(&text, &path.display().to_string());
        }
        Ok(set)
    }

    /// Build a set from inline text (used by tests and small configs).
    pub fn from_text(text: &str) -> Self {
        let mut set = Self::default();
        set.add_lines(text, "<inline>");
        set
    }

    fn add_lines(&mut self, text: &str, origin: &str) {
        for raw in text.lines() {
            let line = raw.split(['#', ';']).next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            match line.parse::<IpNet>() {
                Ok(net) => self.nets.push(net),
                Err(_) => match line.parse::<IpAddr>() {
                    Ok(ip) => self.nets.push(IpNet::from(ip)),
                    Err(_) => {
                        eprintln!("rust-dns: warning: invalid IP/CIDR `{line}` in {origin}");
                    }
                },
            }
        }
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        self.nets.iter().any(|net| net.contains(&ip))
    }

    pub fn len(&self) -> usize {
        self.nets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nets.is_empty()
    }
}

/// Matches the query name against a [`DomainSet`].
pub struct DomainMatcher {
    pub set: Arc<DomainSet>,
}

impl Matcher for DomainMatcher {
    fn matches(&self, ctx: &QueryContext) -> bool {
        self.set.contains(&ctx.qname())
    }
}

/// Matches when any A/AAAA answer in the current response falls inside an
/// [`IpSet`]. With no response yet this is always false.
pub struct ResponseIpMatcher {
    pub set: Arc<IpSet>,
}

impl Matcher for ResponseIpMatcher {
    fn matches(&self, ctx: &QueryContext) -> bool {
        let Some(response) = &ctx.response else {
            return false;
        };
        response
            .answers
            .iter()
            .filter_map(|record| record.data.ip_addr())
            .any(|ip| self.set.contains(ip))
    }
}
