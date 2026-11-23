// SPDX-License-Identifier: GPL-3.0-only
//! Managed-block helpers for `/etc/rust-dns/rule/hosts.txt` (design §5).
//!
//! Only the block between the markers is owned by cf-optimizer; everything
//! outside is preserved byte-for-byte.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::bypass::{Command, CommandRunner, SystemCommandRunner};
use crate::ip_source::is_public_ip;
use crate::output::atomic_write;

pub const HOSTS_BEGIN: &str = "# BEGIN rust-dns cf-optimizer";
pub const HOSTS_END: &str = "# END rust-dns cf-optimizer";
/// Default target for `rust-dns cf-apply-hosts` (design §5).
pub const DEFAULT_HOSTS_FILE: &str = "/etc/rust-dns/rule/hosts.txt";
/// Default init script invoked for the optional reload.
pub const DEFAULT_RELOAD_INIT: &str = "/etc/init.d/rust-dns";

/// Validate and deduplicate entries, returning them in the sorted block order.
pub fn normalize_entries(entries: &[(String, IpAddr)]) -> Result<BTreeSet<(String, IpAddr)>> {
    let mut unique = BTreeSet::new();
    for (domain, ip) in entries {
        validate_domain(domain)?;
        if !is_public_ip(ip) {
            bail!("hosts entry address {ip} is not a public address");
        }
        unique.insert((domain.clone(), *ip));
    }
    Ok(unique)
}

/// Parse a `DOMAIN=IP` CLI entry.
pub fn parse_entry(spec: &str) -> Result<(String, IpAddr)> {
    let (domain, ip) = spec
        .split_once('=')
        .with_context(|| format!("hosts entry {spec:?} must be DOMAIN=IP"))?;
    let domain = domain.trim();
    let ip_text = ip.trim();
    validate_domain(domain)?;
    let ip: IpAddr = ip_text
        .parse()
        .with_context(|| format!("hosts entry {spec:?} has an invalid IP address"))?;
    if !is_public_ip(&ip) {
        bail!("hosts entry address {ip} is not a public address");
    }
    Ok((domain.to_string(), ip))
}

/// Merge `entries` into the managed block of `original`.
///
/// Existing managed blocks are removed first, entries are validated,
/// deduplicated and sorted, and the block is appended at the end. An empty
/// `entries` slice removes the managed block.
pub fn merge_managed_hosts(original: &str, entries: &[(String, IpAddr)]) -> Result<String> {
    let unique = normalize_entries(entries)?;
    let base = strip_managed_block(original);
    let base = base.trim_end();

    if unique.is_empty() {
        return Ok(if base.is_empty() {
            String::new()
        } else {
            format!("{base}\n")
        });
    }

    let mut block = String::new();
    block.push_str(HOSTS_BEGIN);
    block.push('\n');
    for (domain, ip) in &unique {
        writeln!(block, "{domain} {ip}").expect("writing to String cannot fail");
    }
    block.push_str(HOSTS_END);

    if base.is_empty() {
        Ok(format!("{block}\n"))
    } else {
        Ok(format!("{base}\n{block}\n"))
    }
}

/// Remove every managed block (including markers) from `text`.
pub fn strip_managed_block(text: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == HOSTS_BEGIN {
            inside = true;
            continue;
        }
        if trimmed == HOSTS_END {
            inside = false;
            continue;
        }
        if !inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Validate a domain/hostname suitable for a hosts line.
pub fn validate_domain(domain: &str) -> Result<()> {
    if domain.is_empty() {
        bail!("hosts domain is empty");
    }
    if domain.len() > 253 {
        bail!("hosts domain {domain:?} is too long");
    }
    if !domain.is_ascii() {
        bail!("hosts domain {domain:?} must be ASCII (use punycode)");
    }
    if domain
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '#' | '/' | '\\' | ':'))
    {
        bail!("hosts domain {domain:?} contains invalid characters");
    }
    if domain.starts_with('.') || domain.ends_with('.') {
        bail!("hosts domain {domain:?} has an empty label");
    }
    for label in domain.split('.') {
        if label.is_empty() {
            bail!("hosts domain {domain:?} has an empty label");
        }
        if label.len() > 63 {
            bail!("hosts domain {domain:?} has a label longer than 63 characters");
        }
        if label.starts_with('-') || label.ends_with('-') {
            bail!("hosts domain {domain:?} has a label starting or ending with '-'");
        }
        if !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("hosts domain {domain:?} has an invalid label character");
        }
    }
    Ok(())
}

/// Outcome of one managed-block write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOutcome {
    pub path: PathBuf,
    pub backup: PathBuf,
    pub entries: usize,
    pub reloaded: bool,
}

/// Backup path adjacent to `file` (`hosts.txt` -> `hosts.txt.bak`).
pub fn backup_path(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".bak");
    PathBuf::from(name)
}

/// Read the file, replace its managed block, write a `.bak`, then atomically
/// replace the file. With `reload_init`, `rust-dns reload` is executed and the
/// backup is restored on failure.
pub fn apply_hosts(
    file: &Path,
    entries: &[(String, IpAddr)],
    reload_init: Option<&Path>,
) -> Result<ApplyOutcome> {
    apply_hosts_with(file, entries, reload_init, &SystemCommandRunner)
}

/// Testable variant with an injected command runner (no exec in tests).
pub fn apply_hosts_with(
    file: &Path,
    entries: &[(String, IpAddr)],
    reload_init: Option<&Path>,
    runner: &dyn CommandRunner,
) -> Result<ApplyOutcome> {
    if !file.is_absolute() {
        bail!("hosts file path {} must be absolute", file.display());
    }
    if let Some(init) = reload_init
        && !init.is_absolute()
    {
        bail!("reload init path {} must be absolute", init.display());
    }
    let unique = normalize_entries(entries)?;

    let original = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(err).with_context(|| format!("read hosts file {}", file.display()));
        }
    };
    let merged = merge_managed_hosts(&original, entries)?;

    let backup = backup_path(file);
    atomic_write(&backup, original.as_bytes())
        .with_context(|| format!("write hosts backup {}", backup.display()))?;
    atomic_write(file, merged.as_bytes())
        .with_context(|| format!("write hosts file {}", file.display()))?;

    let mut reloaded = false;
    if let Some(init) = reload_init {
        let command = Command::new(init.to_string_lossy().as_ref(), ["reload"]);
        let result = runner.run(&command);
        let failed = match &result {
            Ok(output) if output.is_success() => {
                reloaded = true;
                None
            }
            Ok(output) => Some(format!(
                "`{command}` failed (status {:?}): {}",
                output.status,
                output.stderr.trim()
            )),
            Err(err) => Some(format!("run `{command}`: {err:#}")),
        };
        if let Some(message) = failed {
            restore(file, &original);
            bail!("hosts reload failed, restored backup: {message}");
        }
    }

    Ok(ApplyOutcome {
        path: file.to_path_buf(),
        backup,
        entries: unique.len(),
        reloaded,
    })
}

/// Best-effort rollback after a failed reload.
fn restore(file: &Path, original: &str) {
    if let Err(err) = atomic_write(file, original.as_bytes()) {
        eprintln!(
            "cf-apply-hosts: warning: could not restore {}: {err:#}",
            file.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bypass::MockCommandRunner;

    fn entry(domain: &str, ip: &str) -> (String, IpAddr) {
        (domain.to_string(), ip.parse().unwrap())
    }

    #[test]
    fn inserts_block_and_preserves_existing_content() {
        let original = "# user rules\nintranet.local 192.168.1.2\n";
        let merged =
            merge_managed_hosts(original, &[entry("cloudflare.com", "104.16.0.1")]).unwrap();
        assert!(merged.starts_with("# user rules\nintranet.local 192.168.1.2\n"));
        assert!(merged.contains(HOSTS_BEGIN));
        assert!(merged.contains("cloudflare.com 104.16.0.1"));
        assert!(merged.trim_end().ends_with(HOSTS_END));
    }

    #[test]
    fn replaces_previous_block_and_deduplicates() {
        let original =
            format!("keep me\n{HOSTS_BEGIN}\nold.example 104.16.0.9\n{HOSTS_END}\ntail\n");
        let merged = merge_managed_hosts(
            &original,
            &[
                entry("b.example", "104.16.0.2"),
                entry("a.example", "104.16.0.1"),
                entry("a.example", "104.16.0.1"),
            ],
        )
        .unwrap();
        assert!(merged.starts_with("keep me\ntail\n"));
        assert!(!merged.contains("old.example"));
        assert_eq!(merged.matches("a.example").count(), 1);
        let a = merged.find("a.example").unwrap();
        let b = merged.find("b.example").unwrap();
        assert!(a < b);
    }

    #[test]
    fn empty_entries_remove_block() {
        let original = format!("head\n{HOSTS_BEGIN}\nx.example 104.16.0.1\n{HOSTS_END}\n");
        assert_eq!(merge_managed_hosts(&original, &[]).unwrap(), "head\n");
    }

    #[test]
    fn invalid_domains_and_ips_are_rejected() {
        for domain in [
            "",
            "has space.example",
            "line\nbreak.example",
            "#hash.example",
            "-bad.example",
            "bad-.example",
            "a..b",
            "münchen.example",
        ] {
            assert!(
                validate_domain(domain).is_err(),
                "{domain:?} should be invalid"
            );
        }
        assert!(validate_domain("speed.cloudflare.com").is_ok());
        assert!(validate_domain("xn--mnchen-3ya.example").is_ok());

        assert!(merge_managed_hosts("", &[entry("x.example", "192.168.1.1")]).is_err());
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cf-hosts-{}-{name}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_domain_ip_entries() {
        assert_eq!(
            parse_entry("cloudflare.com=104.16.0.1").unwrap(),
            ("cloudflare.com".to_string(), "104.16.0.1".parse().unwrap())
        );
        assert_eq!(
            parse_entry("  speed.example=2606:4700::1111 ").unwrap(),
            (
                "speed.example".to_string(),
                "2606:4700::1111".parse().unwrap()
            )
        );
        for bad in [
            "no-separator",
            "=104.16.0.1",
            "cloudflare.com=",
            "cloudflare.com=not-an-ip",
            "bad domain=104.16.0.1",
            "cloudflare.com=192.168.1.1",
        ] {
            assert!(parse_entry(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn apply_writes_managed_block_backup_and_dedupes() {
        let dir = tempdir("apply");
        let file = dir.join("hosts.txt");
        std::fs::write(&file, "# keep\nintranet.local 192.168.1.2\n").unwrap();
        let entries = vec![
            entry("b.example", "104.16.0.2"),
            entry("a.example", "104.16.0.1"),
            entry("a.example", "104.16.0.1"),
        ];
        let outcome = apply_hosts_with(&file, &entries, None, &MockCommandRunner::new()).unwrap();
        assert_eq!(outcome.entries, 2);
        assert!(!outcome.reloaded);
        assert_eq!(outcome.backup, backup_path(&file));

        let written = std::fs::read_to_string(&file).unwrap();
        assert!(written.contains("# keep\nintranet.local 192.168.1.2\n"));
        assert_eq!(written.matches("a.example").count(), 1);
        assert!(written.contains("a.example 104.16.0.1"));
        assert!(written.contains("b.example 104.16.0.2"));

        let backup = std::fs::read_to_string(&outcome.backup).unwrap();
        assert_eq!(backup, "# keep\nintranet.local 192.168.1.2\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn apply_creates_the_file_when_missing() {
        let dir = tempdir("missing");
        let file = dir.join("hosts.txt");
        let outcome = apply_hosts_with(
            &file,
            &[entry("cloudflare.com", "104.16.0.1")],
            None,
            &MockCommandRunner::new(),
        )
        .unwrap();
        assert_eq!(outcome.entries, 1);
        let written = std::fs::read_to_string(&file).unwrap();
        assert!(written.starts_with(HOSTS_BEGIN));
        assert!(written.trim_end().ends_with(HOSTS_END));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reload_success_uses_absolute_init_without_a_shell() {
        let dir = tempdir("reload");
        let file = dir.join("hosts.txt");
        let runner = MockCommandRunner::new();
        let outcome = apply_hosts_with(
            &file,
            &[entry("cloudflare.com", "104.16.0.1")],
            Some(Path::new("/etc/init.d/rust-dns")),
            &runner,
        )
        .unwrap();
        assert!(outcome.reloaded);
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], Command::new("/etc/init.d/rust-dns", ["reload"]));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reload_failure_restores_the_original_file() {
        let dir = tempdir("reload-fail");
        let file = dir.join("hosts.txt");
        let original = "# original\n";
        std::fs::write(&file, original).unwrap();
        let runner = MockCommandRunner::new();
        runner.push_failure(1, "service busy");

        let err = apply_hosts_with(
            &file,
            &[entry("cloudflare.com", "104.16.0.1")],
            Some(Path::new("/etc/init.d/rust-dns")),
            &runner,
        )
        .unwrap_err();
        assert!(err.to_string().contains("restored backup"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn relative_paths_are_rejected() {
        let entries = [entry("cloudflare.com", "104.16.0.1")];
        assert!(
            apply_hosts_with(
                Path::new("relative/hosts.txt"),
                &entries,
                None,
                &MockCommandRunner::new()
            )
            .is_err()
        );
        let dir = tempdir("relative-init");
        let file = dir.join("hosts.txt");
        assert!(
            apply_hosts_with(
                &file,
                &entries,
                Some(Path::new("init")),
                &MockCommandRunner::new()
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
