// SPDX-License-Identifier: GPL-3.0-only
//! End-to-end CLI tests for the `rust-dns` binary.
//!
//! These cover the cross-crate integration points: `cf-optimize` raw-argument
//! forwarding and exit codes, `check` JSON/YAML handling, loopback enforcement,
//! stale cancel-file handling, and the `start` policy of keeping the process
//! alive when the admin API cannot be served.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rust-dns")
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rust-dns-cli-e2e-{}-{}",
        std::process::id(),
        TEST_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
}

#[test]
fn cf_optimize_forwards_args_and_writes_json_csv_progress() {
    let dir = temp_dir();
    let output = dir.join("last.json");
    let progress = dir.join("progress.json");
    let cancel = dir.join("cancel");

    let status = Command::new(bin())
        .args([
            "cf-optimize",
            "--dry-run",
            "--threads",
            "3",
            "--output",
            output.to_str().unwrap(),
            "--progress",
            progress.to_str().unwrap(),
            "--cancel-file",
            cancel.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "cf-optimize dry-run should succeed");

    assert!(output.is_file(), "result JSON should exist");
    assert!(progress.is_file(), "progress JSON should exist");
    let csv = dir.join("result.csv");
    assert!(csv.is_file(), "result CSV should exist");

    let json: serde_json::Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    assert_eq!(json["params"]["threads"], 3);
    assert_eq!(json["bypass"]["verified"], true);
    assert_eq!(json["results"].as_array().unwrap().len(), 10);

    let csv_text = fs::read_to_string(&csv).unwrap();
    assert_eq!(
        csv_text.lines().next().unwrap(),
        "IP,family,latency_min_ms,latency_avg_ms,loss_pct,speed_mbps,passed"
    );

    let progress: serde_json::Value =
        serde_json::from_slice(&fs::read(&progress).unwrap()).unwrap();
    assert_eq!(progress["state"], "done");

    fs::remove_dir_all(dir).ok();
}

#[test]
fn cf_optimize_parse_error_exits_1_not_2() {
    let out = Command::new(bin())
        .args(["cf-optimize", "--definitely-not-a-flag"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn cf_optimize_help_and_version_exit_0() {
    for flag in ["--help", "--version"] {
        let out = Command::new(bin())
            .args(["cf-optimize", flag])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "flag {flag}");
        assert!(!out.stdout.is_empty(), "flag {flag} should print to stdout");
    }
}

#[test]
fn cf_optimize_clears_a_stale_cancel_file() {
    let dir = temp_dir();
    let cancel = dir.join("cancel");
    write(&cancel, "");
    let output = dir.join("last.json");

    let status = Command::new(bin())
        .args([
            "cf-optimize",
            "--dry-run",
            "--output",
            output.to_str().unwrap(),
            "--progress",
            dir.join("progress.json").to_str().unwrap(),
            "--cancel-file",
            cancel.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(
        status.success(),
        "a stale cancel flag must not abort the run"
    );
    assert!(output.is_file());

    fs::remove_dir_all(dir).ok();
}

#[test]
fn check_accepts_json_and_yaml_and_rejects_invalid() {
    let dir = temp_dir();
    let yaml = dir.join("config.yaml");
    write(
        &yaml,
        "api:\n  http: 127.0.0.1:9091\nplugins:\n  - type: stats_collector\n    args:\n      enabled: true\n",
    );
    let json = dir.join("config.json");
    write(&json, r#"{"api":{"http":"127.0.0.1:9091"},"plugins":[]}"#);
    let scalar = dir.join("scalar.yaml");
    write(&scalar, "just a scalar\n");

    for path in [&yaml, &json] {
        let status = Command::new(bin())
            .args(["check", "-c", path.to_str().unwrap()])
            .status()
            .unwrap();
        assert!(status.success(), "check should accept {}", path.display());
    }

    let status = Command::new(bin())
        .args(["check", "-c", scalar.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(
        !status.success(),
        "check should reject a non-mapping config"
    );

    fs::remove_dir_all(dir).ok();
}

#[test]
fn start_with_non_loopback_api_keeps_running() {
    let dir = temp_dir();
    let config = dir.join("config.json");
    write(&config, r#"{"api":{"http":"0.0.0.0:9091"},"plugins":[]}"#);

    let mut child = Command::new(bin())
        .args([
            "start",
            "-c",
            config.to_str().unwrap(),
            "-d",
            dir.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    thread::sleep(Duration::from_millis(500));
    let still_running = child.try_wait().unwrap().is_none();
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        still_running,
        "start must keep the DNS process alive when the API is disabled"
    );

    fs::remove_dir_all(dir).ok();
}

#[test]
fn stats_api_rejects_non_loopback_listen() {
    let out = Command::new(bin())
        .args(["stats-api", "--listen", "0.0.0.0:9091"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("loopback"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn cf_apply_hosts_writes_managed_block_backup_and_dedupes() {
    let dir = temp_dir();
    let hosts = dir.join("hosts.txt");
    write(&hosts, "# user rules\nintranet.local 192.168.1.2\n");

    let out = Command::new(bin())
        .args([
            "cf-apply-hosts",
            "--file",
            hosts.to_str().unwrap(),
            "--entry",
            "cloudflare.com=104.16.0.1",
            "--entry",
            "speed.cloudflare.com=104.16.0.2",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("reloaded: false"));

    let written = fs::read_to_string(&hosts).unwrap();
    assert!(written.contains("# user rules\nintranet.local 192.168.1.2\n"));
    assert!(written.contains("cloudflare.com 104.16.0.1"));
    assert!(written.contains("speed.cloudflare.com 104.16.0.2"));
    assert!(written.contains("# BEGIN rust-dns cf-optimizer"));

    let backup = fs::read_to_string(dir.join("hosts.txt.bak")).unwrap();
    assert_eq!(backup, "# user rules\nintranet.local 192.168.1.2\n");

    // Re-applying replaces the block (no duplicate marker).
    let out = Command::new(bin())
        .args([
            "cf-apply-hosts",
            "--file",
            hosts.to_str().unwrap(),
            "--entry",
            "cloudflare.com=104.16.0.1",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let written = fs::read_to_string(&hosts).unwrap();
    assert_eq!(written.matches("# BEGIN rust-dns cf-optimizer").count(), 1);
    assert!(!written.contains("speed.cloudflare.com"));

    // Invalid/private entries are rejected without rewriting the file.
    let before = fs::read_to_string(&hosts).unwrap();
    let out = Command::new(bin())
        .args([
            "cf-apply-hosts",
            "--file",
            hosts.to_str().unwrap(),
            "--entry",
            "cloudflare.com=192.168.1.1",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(fs::read_to_string(&hosts).unwrap(), before);

    fs::remove_dir_all(dir).ok();
}
