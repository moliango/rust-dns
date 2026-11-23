// SPDX-License-Identifier: GPL-3.0-only
//! OpenClash bypass plumbing: temporary policy-routing plan, cleanup guard, and
//! socket identity for probe connections.
//!
//! Nothing here mutates the host during tests: command execution goes through
//! [`CommandRunner`], and tests use [`MockCommandRunner`]. On a real router the
//! plan only adds/deletes its own fwmark rule, route, and nft table.

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio_rustls::rustls;

use crate::probe::{ProbeSocketOptions, build_tls_client, https_get_body};
use crate::types::{BypassInfo, BypassMode, Family, OptimizerConfig};

/// Stable names owned by this feature (design §4.2).
pub const NFT_TABLE: &str = "rust_dns_cf";
/// Init script used by the explicit `--pause-openclash` fallback.
pub const OPENCLASH_INIT: &str = "/etc/init.d/openclash";
/// Cloudflare trace endpoint used for direct-egress verification.
pub const TRACE_HOST: &str = "www.cloudflare.com";
pub const TRACE_PATH: &str = "/cdn-cgi/trace";

/// One external command, passed as an argument vector (never through a shell).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
}

impl Command {
    pub fn new<I, S>(program: &str, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            program: program.to_string(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.program)?;
        for arg in &self.args {
            write!(f, " {arg}")?;
        }
        Ok(())
    }
}

/// Result of executing one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn success() -> Self {
        Self {
            status: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    pub fn failure(code: i32, stderr: impl Into<String>) -> Self {
        Self {
            status: Some(code),
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    pub fn is_success(&self) -> bool {
        self.status == Some(0)
    }
}

/// Executes commands. The production implementation shells out without a shell;
/// tests inject a mock.
pub trait CommandRunner: Send + Sync {
    fn run(&self, command: &Command) -> Result<CommandOutput>;
}

/// Runs commands through `std::process::Command`.
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, command: &Command) -> Result<CommandOutput> {
        let output = std::process::Command::new(&command.program)
            .args(&command.args)
            .output()
            .with_context(|| format!("execute `{command}`"))?;
        Ok(CommandOutput {
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Deterministic runner for unit tests. Responses are consumed in order; once
/// the queue is empty every command succeeds silently.
#[derive(Default)]
pub struct MockCommandRunner {
    state: Mutex<MockState>,
}

#[derive(Default)]
struct MockState {
    calls: Vec<Command>,
    responses: VecDeque<CommandOutput>,
}

impl MockCommandRunner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_response(&self, output: CommandOutput) {
        self.state
            .lock()
            .expect("mock runner poisoned")
            .responses
            .push_back(output);
    }

    pub fn push_failure(&self, code: i32, stderr: impl Into<String>) {
        self.push_response(CommandOutput::failure(code, stderr));
    }

    pub fn calls(&self) -> Vec<Command> {
        self.state
            .lock()
            .expect("mock runner poisoned")
            .calls
            .clone()
    }
}

impl CommandRunner for MockCommandRunner {
    fn run(&self, command: &Command) -> Result<CommandOutput> {
        let mut state = self.state.lock().expect("mock runner poisoned");
        state.calls.push(command.clone());
        Ok(state
            .responses
            .pop_front()
            .unwrap_or_else(CommandOutput::success))
    }
}

/// The temporary routing/nft objects needed by one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BypassPlan {
    pub mode: BypassMode,
    pub mark: u32,
    pub table: u32,
    pub device: Option<String>,
    pub source_ip: Option<std::net::IpAddr>,
}

impl BypassPlan {
    pub fn from_config(config: &OptimizerConfig) -> Result<Self> {
        match config.bypass_mode {
            BypassMode::None => Ok(Self {
                mode: BypassMode::None,
                mark: 0,
                table: 0,
                device: None,
                source_ip: config.source_ip,
            }),
            BypassMode::Mark => {
                if config.mark == 0 {
                    bail!("--mark must be non-zero in mark bypass mode");
                }
                if config.table == 0 {
                    bail!("--table must be non-zero in mark bypass mode");
                }
                let device = config
                    .device
                    .clone()
                    .context("--device is required in mark bypass mode (direct WAN interface)")?;
                validate_interface(&device)?;
                Ok(Self {
                    mode: BypassMode::Mark,
                    mark: config.mark,
                    table: config.table,
                    device: Some(device),
                    source_ip: config.source_ip,
                })
            }
            BypassMode::Device => {
                let device = config
                    .device
                    .clone()
                    .context("--device is required in device bypass mode")?;
                validate_interface(&device)?;
                Ok(Self {
                    mode: BypassMode::Device,
                    mark: 0,
                    table: config.table,
                    device: Some(device),
                    source_ip: config.source_ip,
                })
            }
        }
    }

    /// Socket identity used for latency and download probes.
    pub fn probe_socket_options(&self) -> ProbeSocketOptions {
        ProbeSocketOptions {
            mark: (self.mode == BypassMode::Mark).then_some(self.mark),
            device: (self.mode == BypassMode::Device)
                .then(|| self.device.clone())
                .flatten(),
            source_ip: self.source_ip,
        }
    }

    /// Commands that detect a conflicting mark/table/interface.
    pub fn conflict_check_commands(&self) -> Vec<Command> {
        let mut commands = Vec::new();
        if self.mode == BypassMode::None {
            return commands;
        }
        commands.push(Command::new("ip", ["rule", "show"]));
        if self.mode == BypassMode::Mark {
            commands.push(Command::new("nft", ["list", "table", "inet", NFT_TABLE]));
        }
        if let Some(device) = &self.device {
            commands.push(Command::new("ip", ["link", "show", "dev", device.as_str()]));
        }
        commands
    }

    /// Commands that install the plan, in order.
    pub fn setup_commands(&self) -> Vec<Command> {
        let mut commands = Vec::new();
        match self.mode {
            BypassMode::None => return commands,
            BypassMode::Mark => {
                commands.push(Command::new("nft", ["add", "table", "inet", NFT_TABLE]));
                commands.push(Command::new(
                    "nft",
                    [
                        "add",
                        "chain",
                        "inet",
                        NFT_TABLE,
                        "output",
                        "{ type filter hook output priority mangle; policy accept; }",
                    ],
                ));
                commands.push(Command::new(
                    "nft",
                    [
                        "add",
                        "rule",
                        "inet",
                        NFT_TABLE,
                        "output",
                        "meta",
                        "mark",
                        &format!("0x{:x}", self.mark),
                        "counter",
                        "return",
                    ],
                ));
                commands.push(Command::new(
                    "ip",
                    [
                        "rule".to_string(),
                        "add".to_string(),
                        "fwmark".to_string(),
                        format!("0x{:x}", self.mark),
                        "lookup".to_string(),
                        self.table.to_string(),
                    ],
                ));
                if let Some(device) = &self.device {
                    let mut args = vec![
                        "route".to_string(),
                        "add".to_string(),
                        "default".to_string(),
                        "dev".to_string(),
                        device.clone(),
                        "table".to_string(),
                        self.table.to_string(),
                    ];
                    if let Some(source) = self.source_ip {
                        args.push("src".to_string());
                        args.push(source.to_string());
                    }
                    commands.push(Command::new("ip", args));
                }
            }
            BypassMode::Device => {
                // Device binding is socket-level; only the nft table is needed
                // as a visible, owned marker (no rules, no routing change).
                commands.push(Command::new("nft", ["add", "table", "inet", NFT_TABLE]));
            }
        }
        commands
    }

    /// Commands that remove exactly what [`setup_commands`] created.
    pub fn cleanup_commands(&self) -> Vec<Command> {
        let mut commands = Vec::new();
        match self.mode {
            BypassMode::None => return commands,
            BypassMode::Mark => {
                if let Some(device) = &self.device {
                    commands.push(Command::new(
                        "ip",
                        [
                            "route".to_string(),
                            "del".to_string(),
                            "default".to_string(),
                            "dev".to_string(),
                            device.clone(),
                            "table".to_string(),
                            self.table.to_string(),
                        ],
                    ));
                }
                commands.push(Command::new(
                    "ip",
                    [
                        "rule".to_string(),
                        "del".to_string(),
                        "fwmark".to_string(),
                        format!("0x{:x}", self.mark),
                        "lookup".to_string(),
                        self.table.to_string(),
                    ],
                ));
                commands.push(Command::new("nft", ["delete", "table", "inet", NFT_TABLE]));
            }
            BypassMode::Device => {
                commands.push(Command::new("nft", ["delete", "table", "inet", NFT_TABLE]));
            }
        }
        commands
    }

    /// Human-readable conflict detection for `ip rule show` output.
    pub fn rule_conflicts(&self, ip_rule_show: &str) -> Vec<String> {
        if self.mode != BypassMode::Mark {
            return Vec::new();
        }
        let hex_mark = format!("0x{:x}", self.mark);
        let dec_mark = self.mark.to_string();
        let table = self.table.to_string();
        let mut conflicts = Vec::new();
        for line in ip_rule_show.lines() {
            let matches_mark = line.contains(&format!("fwmark {hex_mark}"))
                || line.contains(&format!("fwmark {dec_mark}"));
            let matches_table = line.contains(&format!("lookup {table}"))
                || line.contains(&format!("table {table}"));
            if matches_mark {
                conflicts.push(format!("mark {hex_mark} already used: {}", line.trim()));
            } else if matches_table {
                conflicts.push(format!("table {table} already used: {}", line.trim()));
            }
        }
        conflicts
    }

    pub fn info(
        &self,
        verified: bool,
        egress_ip: Option<String>,
        note: impl Into<String>,
    ) -> BypassInfo {
        BypassInfo {
            mode: self.mode.as_str().to_string(),
            mark: (self.mode == BypassMode::Mark).then(|| format!("0x{:x}", self.mark)),
            device: self.device.clone(),
            verified,
            egress_ip,
            note: note.into(),
        }
    }
}

/// Outcome of the direct-egress check performed after installing a bypass plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressCheck {
    pub egress_ip: Option<IpAddr>,
    pub verified: bool,
    pub note: String,
}

/// Verifies that probe traffic actually reaches the Internet directly.
///
/// The trait boundary lets tests inject a deterministic verifier; the real
/// implementation reads `https://www.cloudflare.com/cdn-cgi/trace` with the
/// same socket identity (mark/device/source) as the probes.
pub trait EgressVerifier: Send + Sync {
    fn verify<'a>(
        &'a self,
        options: &'a ProbeSocketOptions,
    ) -> Pin<Box<dyn Future<Output = Result<EgressCheck>> + Send + 'a>>;
}

/// HTTPS Cloudflare-trace verifier.
pub struct HttpsEgressVerifier {
    family: Family,
    timeout: Duration,
    tls: Arc<rustls::ClientConfig>,
}

impl HttpsEgressVerifier {
    pub fn new(family: Family, timeout: Duration) -> Result<Self> {
        Ok(Self {
            family,
            timeout,
            tls: build_tls_client()?,
        })
    }
}

impl EgressVerifier for HttpsEgressVerifier {
    fn verify<'a>(
        &'a self,
        options: &'a ProbeSocketOptions,
    ) -> Pin<Box<dyn Future<Output = Result<EgressCheck>> + Send + 'a>> {
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((TRACE_HOST, 443u16))
                .await
                .with_context(|| format!("resolve {TRACE_HOST}"))?
                .filter(|addr| self.family.matches(&addr.ip()))
                .collect();
            if addrs.is_empty() {
                bail!("no IPv{} address for {TRACE_HOST}", self.family.as_u8());
            }
            let mut body = None;
            let mut last_error = None;
            for addr in addrs {
                match https_get_body(
                    addr,
                    TRACE_HOST,
                    TRACE_HOST,
                    TRACE_PATH,
                    options,
                    self.timeout,
                    &self.tls,
                )
                .await
                {
                    Ok(bytes) => {
                        body = Some(bytes);
                        break;
                    }
                    Err(err) => last_error = Some(err),
                }
            }
            let body = body.ok_or_else(|| {
                last_error.unwrap_or_else(|| anyhow!("no usable address for {TRACE_HOST}"))
            })?;
            let text = String::from_utf8_lossy(&body);
            match parse_trace_ip(&text) {
                Some(ip) => Ok(EgressCheck {
                    egress_ip: Some(ip),
                    verified: true,
                    note: "direct egress confirmed via Cloudflare trace".to_string(),
                }),
                None => Ok(EgressCheck {
                    egress_ip: None,
                    verified: false,
                    note: "Cloudflare trace returned no ip= field".to_string(),
                }),
            }
        })
    }
}

/// Extract the `ip=` value from a Cloudflare trace document.
pub fn parse_trace_ip(text: &str) -> Option<IpAddr> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("ip="))
        .and_then(|value| value.trim().parse().ok())
}

/// Explicit `--pause-openclash` guard: stops OpenClash before a run and starts
/// it again on success, cancellation, error, or drop.
///
/// It is never installed unless the user asks for it, and it only invokes the
/// fixed init script with a `start`/`stop` argument (no shell string).
pub struct OpenClashGuard<'a> {
    runner: &'a dyn CommandRunner,
    paused: bool,
}

impl<'a> OpenClashGuard<'a> {
    pub fn install(runner: &'a dyn CommandRunner) -> Result<Self> {
        let command = Command::new(OPENCLASH_INIT, ["stop"]);
        let output = runner
            .run(&command)
            .with_context(|| format!("run `{command}`"))?;
        if !output.is_success() {
            bail!(
                "could not pause OpenClash: `{command}` failed (status {:?}): {}",
                output.status,
                output.stderr.trim()
            );
        }
        Ok(Self {
            runner,
            paused: true,
        })
    }

    /// Restart OpenClash. Safe to call more than once.
    pub fn cleanup(&mut self) -> Result<()> {
        if !self.paused {
            return Ok(());
        }
        self.paused = false;
        let command = Command::new(OPENCLASH_INIT, ["start"]);
        let output = self
            .runner
            .run(&command)
            .with_context(|| format!("run `{command}`"))?;
        if !output.is_success() {
            bail!(
                "could not restart OpenClash: `{command}` failed (status {:?}): {}",
                output.status,
                output.stderr.trim()
            );
        }
        Ok(())
    }
}

impl Drop for OpenClashGuard<'_> {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn validate_interface(device: &str) -> Result<()> {
    if device.is_empty() || device.len() > 15 {
        bail!("network interface name {device:?} has an invalid length");
    }
    if device
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '/' || c == ':')
    {
        bail!("network interface name {device:?} has invalid characters");
    }
    Ok(())
}

/// Installs a plan and removes it on drop.
pub struct BypassGuard<'a> {
    plan: BypassPlan,
    runner: &'a dyn CommandRunner,
    installed: bool,
}

impl<'a> BypassGuard<'a> {
    /// Validate conflicts, then install every setup command. If any command
    /// fails, already-created objects are rolled back.
    pub fn install(plan: BypassPlan, runner: &'a dyn CommandRunner) -> Result<Self> {
        if plan.mode != BypassMode::None {
            check_conflicts(&plan, runner)?;
            let setup = plan.setup_commands();
            for command in &setup {
                let output = runner
                    .run(command)
                    .with_context(|| format!("run `{command}`"))?;
                if !output.is_success() {
                    rollback(&plan, runner);
                    bail!(
                        "bypass setup command failed: `{command}` (status {:?}): {}",
                        output.status,
                        output.stderr.trim()
                    );
                }
            }
        }
        let installed = plan.mode != BypassMode::None;
        Ok(Self {
            plan,
            runner,
            installed,
        })
    }

    pub fn plan(&self) -> &BypassPlan {
        &self.plan
    }

    /// Remove the temporary objects. Safe to call more than once.
    pub fn cleanup(&mut self) -> Result<()> {
        if !self.installed {
            return Ok(());
        }
        self.installed = false;
        let mut first_error = None;
        for command in self.plan.cleanup_commands() {
            match self.runner.run(&command) {
                Ok(output) if output.is_success() => {}
                Ok(output) => {
                    first_error.get_or_insert_with(|| {
                        format!(
                            "cleanup command `{command}` failed (status {:?}): {}",
                            output.status,
                            output.stderr.trim()
                        )
                    });
                }
                Err(err) => {
                    first_error.get_or_insert_with(|| format!("run `{command}`: {err:#}"));
                }
            }
        }
        match first_error {
            Some(message) => bail!(message),
            None => Ok(()),
        }
    }
}

impl Drop for BypassGuard<'_> {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn check_conflicts(plan: &BypassPlan, runner: &dyn CommandRunner) -> Result<()> {
    for command in plan.conflict_check_commands() {
        let output = runner
            .run(&command)
            .with_context(|| format!("run `{command}`"))?;
        let is_nft_list =
            command.program == "nft" && command.args.first().is_some_and(|a| a == "list");
        if is_nft_list {
            if output.is_success() {
                bail!("nft table inet {NFT_TABLE} already exists; refusing to touch it");
            }
            continue;
        }
        if !output.is_success() {
            bail!(
                "pre-flight check `{command}` failed (status {:?}): {}",
                output.status,
                output.stderr.trim()
            );
        }
        if command.args.first().is_some_and(|a| a == "rule") {
            let conflicts = plan.rule_conflicts(&output.stdout);
            if let Some(first) = conflicts.first() {
                bail!("routing conflict: {first}");
            }
        }
    }
    Ok(())
}

fn rollback(plan: &BypassPlan, runner: &dyn CommandRunner) {
    // Deleting objects that were never created is harmless, and several setup
    // commands are covered by one nft delete. Run the full owned cleanup set.
    for command in plan.cleanup_commands() {
        let _ = runner.run(&command);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::OptimizerConfig;

    fn mark_config() -> OptimizerConfig {
        OptimizerConfig {
            bypass_mode: BypassMode::Mark,
            mark: 0x40_0000,
            table: 1688,
            device: Some("wan".to_string()),
            ..OptimizerConfig::default()
        }
    }

    #[test]
    fn mark_plan_has_expected_setup_and_cleanup() {
        let plan = BypassPlan::from_config(&mark_config()).unwrap();
        let setup = plan.setup_commands();
        let rendered: Vec<String> = setup.iter().map(ToString::to_string).collect();
        assert_eq!(
            rendered,
            vec![
                "nft add table inet rust_dns_cf",
                "nft add chain inet rust_dns_cf output { type filter hook output priority mangle; policy accept; }",
                "nft add rule inet rust_dns_cf output meta mark 0x400000 counter return",
                "ip rule add fwmark 0x400000 lookup 1688",
                "ip route add default dev wan table 1688",
            ]
        );
        let cleanup: Vec<String> = plan
            .cleanup_commands()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            cleanup,
            vec![
                "ip route del default dev wan table 1688",
                "ip rule del fwmark 0x400000 lookup 1688",
                "nft delete table inet rust_dns_cf",
            ]
        );
        assert_eq!(
            plan.probe_socket_options(),
            ProbeSocketOptions {
                mark: Some(0x40_0000),
                device: None,
                source_ip: None,
            }
        );
    }

    #[test]
    fn device_plan_binds_socket_and_requires_device() {
        let config = OptimizerConfig {
            bypass_mode: BypassMode::Device,
            device: Some("wan".to_string()),
            source_ip: Some("2001:db8::1".parse().unwrap()),
            ..OptimizerConfig::default()
        };
        let plan = BypassPlan::from_config(&config).unwrap();
        assert!(plan.setup_commands().iter().any(|c| c.program == "nft"));
        assert_eq!(plan.probe_socket_options().device.as_deref(), Some("wan"));

        let missing = OptimizerConfig {
            bypass_mode: BypassMode::Device,
            device: None,
            ..OptimizerConfig::default()
        };
        assert!(BypassPlan::from_config(&missing).is_err());
    }

    #[test]
    fn none_plan_has_no_commands() {
        let plan = BypassPlan::from_config(&OptimizerConfig::default()).unwrap();
        assert!(plan.setup_commands().is_empty());
        assert!(plan.cleanup_commands().is_empty());
        assert!(plan.conflict_check_commands().is_empty());
    }

    #[test]
    fn rule_conflicts_detects_mark_and_table() {
        let plan = BypassPlan::from_config(&mark_config()).unwrap();
        let show = "0:\tfrom all lookup local\n32765:\tfrom all fwmark 0x400000 lookup 1688\n";
        let conflicts = plan.rule_conflicts(show);
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].contains("mark 0x400000"));

        let show = "32765:\tfrom all lookup 1688\n";
        assert!(plan.rule_conflicts(show)[0].contains("table 1688"));
        assert!(
            plan.rule_conflicts("0:\tfrom all lookup local\n")
                .is_empty()
        );
    }

    #[test]
    fn guard_installs_and_cleans_with_mock_runner() {
        let runner = MockCommandRunner::new();
        // ip rule show, nft list (must fail: table absent), ip link show.
        runner.push_response(CommandOutput::success());
        runner.push_failure(1, "No such file or directory");
        runner.push_response(CommandOutput::success());

        {
            let plan = BypassPlan::from_config(&mark_config()).unwrap();
            let mut guard = BypassGuard::install(plan, &runner).unwrap();
            guard.cleanup().unwrap();
        }

        let calls: Vec<String> = runner.calls().iter().map(ToString::to_string).collect();
        assert_eq!(calls[0], "ip rule show");
        assert_eq!(calls[1], "nft list table inet rust_dns_cf");
        assert_eq!(calls[2], "ip link show dev wan");
        assert_eq!(calls[3], "nft add table inet rust_dns_cf");
        assert!(calls.contains(&"ip rule add fwmark 0x400000 lookup 1688".to_string()));
        assert!(calls.contains(&"ip route del default dev wan table 1688".to_string()));
        assert!(calls.contains(&"nft delete table inet rust_dns_cf".to_string()));
        let cleanup_at = calls
            .iter()
            .position(|c| c.contains("nft delete table"))
            .unwrap();
        assert!(cleanup_at > 3);
    }

    #[test]
    fn conflict_aborts_before_setup() {
        let runner = MockCommandRunner::new();
        runner.push_response(CommandOutput {
            status: Some(0),
            stdout: "32765:\tfrom all fwmark 0x400000 lookup 1688\n".to_string(),
            stderr: String::new(),
        });
        let plan = BypassPlan::from_config(&mark_config()).unwrap();
        let err = match BypassGuard::install(plan, &runner) {
            Ok(_) => panic!("expected a conflict"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("routing conflict"));
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn trace_ip_parsing() {
        let trace = "fl=abc\nh=www.cloudflare.com\nip=203.0.113.10\nts=1\n";
        assert_eq!(parse_trace_ip(trace), Some("203.0.113.10".parse().unwrap()));
        let v6 = "ip=2606:4700::1111\n";
        assert_eq!(parse_trace_ip(v6), Some("2606:4700::1111".parse().unwrap()));
        assert_eq!(parse_trace_ip("fl=abc\n"), None);
        assert_eq!(parse_trace_ip("ip=not-an-ip\n"), None);
    }

    #[test]
    fn openclash_guard_stops_then_starts_with_a_mock_runner() {
        let runner = MockCommandRunner::new();
        {
            let mut guard = OpenClashGuard::install(&runner).unwrap();
            guard.cleanup().unwrap();
            // A second cleanup must not issue another command.
            guard.cleanup().unwrap();
        }
        let calls: Vec<String> = runner.calls().iter().map(ToString::to_string).collect();
        assert_eq!(
            calls,
            vec![
                "/etc/init.d/openclash stop".to_string(),
                "/etc/init.d/openclash start".to_string(),
            ]
        );
    }

    #[test]
    fn openclash_guard_restores_on_drop() {
        let runner = MockCommandRunner::new();
        {
            let _guard = OpenClashGuard::install(&runner).unwrap();
        }
        let calls: Vec<String> = runner.calls().iter().map(ToString::to_string).collect();
        assert_eq!(calls.last().unwrap(), "/etc/init.d/openclash start");
    }

    #[test]
    fn openclash_stop_failure_is_reported() {
        let runner = MockCommandRunner::new();
        runner.push_failure(1, "not installed");
        let err = match OpenClashGuard::install(&runner) {
            Ok(_) => panic!("expected failure"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("could not pause OpenClash"));
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn failed_setup_rolls_back_partial_commands() {
        let runner = MockCommandRunner::new();
        runner.push_response(CommandOutput::success()); // ip rule show
        runner.push_failure(1, "no table"); // nft list
        runner.push_response(CommandOutput::success()); // ip link
        runner.push_response(CommandOutput::success()); // nft add table
        runner.push_failure(1, "chain failed"); // nft add chain

        let plan = BypassPlan::from_config(&mark_config()).unwrap();
        let err = match BypassGuard::install(plan, &runner) {
            Ok(_) => panic!("expected setup failure"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("bypass setup command failed"));
        let calls: Vec<String> = runner.calls().iter().map(ToString::to_string).collect();
        assert!(calls.iter().any(|c| c.contains("nft delete table")));
    }
}
