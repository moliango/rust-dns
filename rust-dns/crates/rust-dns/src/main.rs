// SPDX-License-Identifier: GPL-3.0-only
//! rust-dns binary entrypoint.
//!
//! The DNS core is implemented incrementally. This CLI already provides the
//! OpenWrt service contract (`version`, `check`, `start`), the standalone stats
//! API (`stats-api`) and the Cloudflare optimizer (`cf-optimize`).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use rust_dns::dns::{self, DnsService, ServerKind};
use rust_dns_core::Handler;
use rust_dns_core::config::RuntimeConfig;
use rust_dns_core::server::{serve_tcp_listener, serve_udp_socket};
use rust_dns_stats_api::{
    CacheFlush, LoadOutcome, StatsCollector, StatsConfig, http::is_loopback_addr,
};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::watch;
use tokio::task::JoinSet;

/// Default dump path from design §5, used when `dump_file` is a boolean flag.
const DEFAULT_STATS_DUMP_PATH: &str = "/var/rust-dns/stats.dump";

#[derive(Debug, Parser)]
#[command(name = "rust-dns", version, about = "Plugin-based DNS forwarder")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the version.
    Version,
    /// Validate a generated JSON or YAML configuration.
    Check {
        #[arg(short = 'c', long = "config")]
        config: PathBuf,
    },
    /// Start the daemon with a configuration file and working directory.
    Start {
        #[arg(short = 'c', long = "config")]
        config: PathBuf,
        #[arg(short = 'd', long = "dir", default_value = ".")]
        dir: PathBuf,
    },
    /// Run only the local stats/admin API (useful for LuCI development).
    StatsApi {
        #[arg(long, default_value = "127.0.0.1:9091")]
        listen: String,
        #[arg(long, default_value_t = 2000)]
        capacity: usize,
        #[arg(long, default_value_t = 168)]
        history_max: usize,
        #[arg(long, default_value_t = 100)]
        top_max: usize,
        #[arg(long, default_value_t = 200)]
        logs_max: usize,
        #[arg(long)]
        dump: Option<PathBuf>,
    },
    /// Select fast Cloudflare IPs. See `rust-dns cf-optimize --help`.
    #[command(
        name = "cf-optimize",
        disable_help_flag = true,
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    CfOptimize {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Write the cf-optimizer managed block into the rust-dns hosts file.
    ///
    /// The managed block is replaced atomically, a `.bak` copy is kept, and the
    /// reload is an argument-vector exec of the fixed init script (no shell).
    CfApplyHosts {
        #[arg(long, default_value = "/etc/rust-dns/rule/hosts.txt")]
        file: PathBuf,
        /// Repeatable `DOMAIN=IP` entry.
        #[arg(long = "entry", required = true, value_name = "DOMAIN=IP")]
        entry: Vec<String>,
        /// Init script to run with `reload`; defaults to none.
        #[arg(long)]
        reload_init: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Version => {
            let target = option_env!("RUST_DNS_TARGET").unwrap_or("unknown-target");
            println!("rust-dns {} ({target})", env!("CARGO_PKG_VERSION"));
        }
        Command::Check { config } => {
            let value = load_config_value(&config)?;
            validate_config_value(&value)?;
            println!("config ok: {}", config.display());
        }
        Command::Start { config, dir } => {
            start(&config, &dir).await?;
        }
        Command::StatsApi {
            listen,
            capacity,
            history_max,
            top_max,
            logs_max,
            dump,
        } => {
            let addr: SocketAddr = listen
                .parse()
                .with_context(|| format!("invalid --listen address {listen:?}"))?;
            if !is_loopback_addr(&addr) {
                bail!("stats API must bind to a loopback address in v1, got {addr}");
            }
            let cfg = StatsConfig {
                capacity,
                history_max,
                top_max,
                logs_max,
                dump_path: dump,
                ..StatsConfig::default()
            };
            run_api(addr, cfg).await?;
        }
        Command::CfApplyHosts {
            file,
            entry,
            reload_init,
        } => {
            let entries = entry
                .iter()
                .map(|spec| {
                    rust_dns_cf_optimizer::hosts::parse_entry(spec)
                        .with_context(|| format!("invalid --entry {spec:?}"))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let outcome =
                rust_dns_cf_optimizer::hosts::apply_hosts(&file, &entries, reload_init.as_deref())?;
            println!(
                "cf-apply-hosts: wrote {} entry/entries to {} (backup {}); reloaded: {}",
                outcome.entries,
                outcome.path.display(),
                outcome.backup.display(),
                outcome.reloaded
            );
        }
        Command::CfOptimize { args } => {
            let mut argv = Vec::with_capacity(args.len() + 1);
            argv.push("rust-dns cf-optimize".to_string());
            argv.extend(args);
            // Parse errors must exit 1, not clap's default 2, because 2 means
            // "cancelled" in the cf-optimize contract. Help/version still exit 0.
            let args = match rust_dns_cf_optimizer::Args::try_parse_from(argv) {
                Ok(args) => args,
                Err(err) => {
                    let _ = err.print();
                    std::process::exit(rust_dns_cf_optimizer::exit_code_for_parse_error(&err));
                }
            };
            let config = args.into_config();
            clear_stale_cancel_file(&config.cancel_file);
            match rust_dns_cf_optimizer::pipeline::run(&config).await {
                Ok(outcome) => {
                    eprintln!(
                        "cf-optimize: {} result(s); JSON {} CSV {}",
                        outcome.report.results.len(),
                        config.output.display(),
                        outcome.csv_path.display()
                    );
                    std::process::exit(outcome.exit_code);
                }
                Err(err) => {
                    eprintln!("cf-optimize: {err:#}");
                    std::process::exit(1);
                }
            }
        }
    }

    Ok(())
}

/// A DNS listener bound before any task starts, so a busy port fails fast.
enum BoundListener {
    Udp(Arc<UdpSocket>),
    Tcp(TcpListener),
}

async fn start(config: &Path, dir: &Path) -> anyhow::Result<()> {
    // A malformed or non-mapping config is a hard error: the service contract
    // requires `check` to fail before the process is started.
    let value = load_config_value(config)?;
    let runtime = RuntimeConfig::load(config)
        .map_err(|err| anyhow::anyhow!("invalid DNS config {}: {err}", config.display()))?;

    // v1 policy: never exit because the admin API is misconfigured; the DNS
    // core keeps running and the API subsystem is disabled.
    let mut api_addr = None;
    match api_socket_addr(&value) {
        Ok(Some(addr)) if is_loopback_addr(&addr) => api_addr = Some(addr),
        Ok(Some(addr)) => eprintln!(
            "rust-dns: api.http {addr} is not a loopback address; v1 serves the stats API on loopback only, \
             stats API disabled, DNS continues"
        ),
        Ok(None) => {}
        Err(reason) => eprintln!("rust-dns: {reason}; stats API disabled, DNS continues"),
    }

    let listeners =
        dns::listeners(&runtime).map_err(|err| anyhow::anyhow!("invalid DNS config: {err}"))?;

    // Build the pipeline before touching the network so config errors are
    // reported without a half-started service. The collector is attached after
    // the stats subsystem starts.
    let mut service = if listeners.is_empty() {
        None
    } else {
        Some(
            DnsService::build(&runtime, None)
                .map_err(|err| anyhow::anyhow!("invalid DNS config: {err}"))?,
        )
    };

    let mut bound = Vec::with_capacity(listeners.len());
    for spec in &listeners {
        match spec.kind {
            ServerKind::Udp => {
                let socket = UdpSocket::bind(spec.listen)
                    .await
                    .with_context(|| format!("cannot bind UDP DNS listener on {}", spec.listen))?;
                let addr = socket.local_addr().unwrap_or(spec.listen);
                eprintln!("rust-dns: DNS UDP listening on {addr}");
                bound.push(BoundListener::Udp(Arc::new(socket)));
            }
            ServerKind::Tcp => {
                let listener = TcpListener::bind(spec.listen)
                    .await
                    .with_context(|| format!("cannot bind TCP DNS listener on {}", spec.listen))?;
                let addr = listener.local_addr().unwrap_or(spec.listen);
                eprintln!("rust-dns: DNS TCP listening on {addr}");
                bound.push(BoundListener::Tcp(listener));
            }
        }
    }

    let stats_cfg = stats_config_from_value(&value);
    let dump_path = stats_cfg.dump_path.clone();
    let collector = StatsCollector::start(stats_cfg);
    if let Some(path) = &dump_path {
        load_stats_dump(&collector, path);
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut dns_tasks: JoinSet<(&'static str, String, rust_dns_core::Result<()>)> = JoinSet::new();

    if let Some(service) = service.as_mut() {
        service.set_collector(collector.clone());
    }
    let service = service.map(Arc::new);

    // Expose the DNS cache to `POST /api/v1/cache/flush`.
    let cache_flush: Option<Arc<dyn CacheFlush>> = service
        .as_ref()
        .and_then(|service| service.cache())
        .map(|cache| cache as Arc<dyn CacheFlush>);

    for listener in bound {
        let handler = shared_handler(
            service
                .as_ref()
                .expect("a DNS service exists whenever listeners exist"),
        );
        let rx = shutdown_rx.clone();
        match listener {
            BoundListener::Udp(socket) => {
                let addr = socket.local_addr().ok();
                dns_tasks.spawn(async move {
                    let result = serve_udp_socket(socket, handler, wait_for_shutdown(rx)).await;
                    ("udp", format!("{addr:?}"), result)
                });
            }
            BoundListener::Tcp(listener) => {
                let addr = listener.local_addr().ok();
                dns_tasks.spawn(async move {
                    let result = serve_tcp_listener(listener, handler, wait_for_shutdown(rx)).await;
                    ("tcp", format!("{addr:?}"), result)
                });
            }
        }
    }

    // The stats API is best-effort: a bind failure only disables it.
    let api_task = match api_addr {
        Some(addr) => match TcpListener::bind(addr).await {
            Ok(listener) => {
                let collector = collector.clone();
                let cache_flush = cache_flush.clone();
                let rx = shutdown_rx.clone();
                Some(tokio::spawn(async move {
                    if let Err(err) = rust_dns_stats_api::serve_with_listener(
                        listener,
                        collector,
                        cache_flush,
                        wait_for_shutdown(rx),
                    )
                    .await
                    {
                        eprintln!("rust-dns: stats API on {addr} failed: {err}; DNS continues");
                    }
                }))
            }
            Err(err) => {
                eprintln!("rust-dns: stats API disabled on {addr}: {err}; DNS continues");
                None
            }
        },
        None => None,
    };

    if dns_tasks.is_empty() {
        // Preserve the historical stats-only mode: idle until a signal, then
        // flush the collector.
        eprintln!(
            "rust-dns: no DNS server plugins configured (config={}, dir={}); idling",
            config.display(),
            dir.display()
        );
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
        if let Some(task) = api_task {
            let _ = task.await;
        }
        collector.shutdown().await;
        return Ok(());
    }

    eprintln!(
        "rust-dns: DNS service started (config={}, dir={})",
        config.display(),
        dir.display()
    );

    let mut failure: Option<String> = None;
    tokio::select! {
        _ = shutdown_signal() => {}
        joined = dns_tasks.join_next() => {
            match joined {
                Some(Ok((proto, addr, Ok(())))) => {
                    failure = Some(format!("{proto} listener {addr} stopped unexpectedly"));
                }
                Some(Ok((proto, addr, Err(err)))) => {
                    failure = Some(format!("{proto} listener {addr} failed: {err}"));
                }
                Some(Err(err)) => failure = Some(format!("DNS listener task panicked: {err}")),
                None => {}
            }
        }
    }

    // Tell the listeners and the API to stop, then drain them.
    let _ = shutdown_tx.send(true);
    while let Some(joined) = dns_tasks.join_next().await {
        match joined {
            Ok((proto, addr, Err(err))) => {
                eprintln!("rust-dns: {proto} listener {addr} failed during shutdown: {err}")
            }
            Ok(_) => {}
            Err(err) => eprintln!("rust-dns: DNS listener task panicked during shutdown: {err}"),
        }
    }
    if let Some(task) = api_task {
        let _ = task.await;
    }

    // Flush queued events and write the final dump on SIGINT/SIGTERM.
    collector.shutdown().await;

    if let Some(failure) = failure {
        bail!("rust-dns: {failure}");
    }
    Ok(())
}

/// Coerce the concrete service into the listener's trait object.
fn shared_handler(service: &Arc<DnsService>) -> Arc<dyn Handler> {
    Arc::clone(service) as Arc<dyn Handler>
}

/// Resolve when the shared shutdown flag flips to `true` (or the sender drops).
async fn wait_for_shutdown(mut rx: watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

async fn run_api(addr: SocketAddr, cfg: StatsConfig) -> anyhow::Result<()> {
    let dump_path = cfg.dump_path.clone();
    let collector = StatsCollector::start(cfg);
    if let Some(path) = &dump_path {
        load_stats_dump(&collector, path);
    }

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("stats API bind failed on {addr}"))?;
    let result = rust_dns_stats_api::serve_with_listener(
        listener,
        collector.clone(),
        None,
        shutdown_signal(),
    )
    .await;
    // Always flush, even when serving returns an error.
    collector.shutdown().await;
    result.with_context(|| format!("stats API failed on {addr}"))?;
    Ok(())
}

/// Parse `api.http` from a config value.
///
/// Returns `Ok(None)` when the key is absent or empty. The caller applies the
/// loopback-only policy so a bad value only disables the API.
fn api_socket_addr(value: &serde_json::Value) -> Result<Option<SocketAddr>, String> {
    let Some(raw) = value
        .pointer("/api/http")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    raw.parse::<SocketAddr>()
        .map(Some)
        .map_err(|err| format!("invalid api.http {raw:?}: {err}"))
}

/// Load a previously saved stats dump, logging the outcome.
fn load_stats_dump(collector: &StatsCollector, path: &Path) {
    match collector.load(path) {
        Ok(LoadOutcome::Loaded(data)) => eprintln!(
            "rust-dns: loaded stats dump {} ({} records)",
            path.display(),
            data.records.len()
        ),
        Ok(LoadOutcome::Corrupt) => eprintln!(
            "rust-dns: corrupt stats dump {} quarantined; starting empty",
            path.display()
        ),
        Ok(LoadOutcome::Missing) => {}
        Err(err) => eprintln!(
            "rust-dns: failed to load stats dump {}: {err}",
            path.display()
        ),
    }
}

/// Remove a stale cancellation flag left by an earlier run.
fn clear_stale_cancel_file(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => eprintln!(
            "cf-optimize: warning: cannot clear stale cancel file {}: {err}",
            path.display()
        ),
    }
}

fn stats_config_from_value(value: &serde_json::Value) -> StatsConfig {
    let stats = value
        .get("plugins")
        .and_then(|v| v.as_array())
        .and_then(|plugins| {
            plugins
                .iter()
                .find(|p| p.get("type").and_then(|v| v.as_str()) == Some("stats_collector"))
        });

    let args = stats.and_then(|p| p.get("args"));
    let enabled = args
        .and_then(|a| a.get("enabled"))
        .map(json_truthy)
        .unwrap_or(stats.is_some());

    let mut cfg = StatsConfig {
        enabled,
        ..StatsConfig::default()
    };

    if let Some(args) = args {
        if let Some(v) = args.get("capacity").and_then(json_u64) {
            cfg.capacity = v as usize;
        }
        if let Some(v) = args.get("history_max").and_then(json_u64) {
            cfg.history_max = v as usize;
        }
        if let Some(v) = args.get("top_max").and_then(json_u64) {
            cfg.top_max = v as usize;
        }
        if let Some(v) = args.get("logs_max").and_then(json_u64) {
            cfg.logs_max = v as usize;
        }
        if let Some(v) = args.get("dump_interval").and_then(json_u64) {
            cfg.dump_interval_secs = v;
        }
        cfg.dump_path = dump_path_from_args(args);
    }

    cfg.sanitized()
}

/// Map `dump_file` / `dump_path` args to an optional dump path.
///
/// The generated config writes the path (or `""`) into `dump_file`. For
/// hand-written configs where `dump_file` is a boolean flag, fall back to
/// `dump_path` and then the documented default path.
fn dump_path_from_args(args: &serde_json::Value) -> Option<PathBuf> {
    let dump_file = args.get("dump_file");
    if !dump_file.is_some_and(json_truthy) {
        return None;
    }
    if let Some(path) = args
        .get("dump_path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return Some(PathBuf::from(path));
    }
    match dump_file.and_then(|v| v.as_str()) {
        Some(text) if !text.is_empty() && text != "1" && text != "true" => {
            Some(PathBuf::from(text))
        }
        _ => Some(PathBuf::from(DEFAULT_STATS_DUMP_PATH)),
    }
}

fn json_truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(v) => *v,
        serde_json::Value::Number(v) => v.as_i64().is_some_and(|n| n != 0),
        serde_json::Value::String(v) => !v.is_empty() && v != "0" && v != "false",
        _ => false,
    }
}

fn json_u64(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

fn validate_config_value(value: &serde_json::Value) -> anyhow::Result<()> {
    if let Some(http) = value
        .pointer("/api/http")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        let addr: SocketAddr = http
            .parse()
            .with_context(|| format!("invalid api.http {http:?}"))?;
        if !is_loopback_addr(&addr) {
            bail!("api.http must be a loopback address in v1, got {addr}");
        }
    }

    if let Some(plugins) = value.get("plugins") {
        let plugins = plugins
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("plugins must be an array"))?;
        for (index, plugin) in plugins.iter().enumerate() {
            let kind = plugin
                .get("type")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("plugins[{index}].type is required"))?;
            if matches!(
                kind,
                "udp_server" | "tcp_server" | "http_server" | "quic_server"
            ) && let Some(listen) = plugin.pointer("/args/listen").and_then(|v| v.as_str())
            {
                let _: SocketAddr = listen.parse().with_context(|| {
                    format!("plugins[{index}].args.listen is invalid: {listen:?}")
                })?;
            }
        }
    }

    Ok(())
}

fn load_config_value(path: &Path) -> anyhow::Result<serde_json::Value> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;

    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(json_err) => serde_yaml_ng::from_str(&text).map_err(|yaml_err| {
            anyhow::anyhow!(
                "invalid config {}: JSON error: {json_err}; YAML error: {yaml_err}",
                path.display()
            )
        })?,
    };

    if !value.is_object() {
        bail!("config {} must be a mapping", path.display());
    }
    Ok(value)
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut term = match signal(SignalKind::terminate()) {
            Ok(sig) => sig,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_dns_stats_api::{LogQuery, QueryEvent};
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rust-dns-cli-test-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn api_socket_addr_honours_missing_empty_and_loopback() {
        let value = json!({"api": {"http": "127.0.0.1:9091"}});
        assert_eq!(
            api_socket_addr(&value).unwrap(),
            Some("127.0.0.1:9091".parse().unwrap())
        );

        let value = json!({"api": {"http": "[::1]:9091"}});
        let addr = api_socket_addr(&value).unwrap().unwrap();
        assert!(is_loopback_addr(&addr));

        let value = json!({"api": {"http": "0.0.0.0:9091"}});
        let addr = api_socket_addr(&value).unwrap().unwrap();
        assert!(!is_loopback_addr(&addr));

        assert_eq!(api_socket_addr(&json!({})).unwrap(), None);
        assert_eq!(
            api_socket_addr(&json!({"api": {"http": ""}})).unwrap(),
            None
        );
        assert!(api_socket_addr(&json!({"api": {"http": "not-an-addr"}})).is_err());
    }

    #[test]
    fn stats_plugin_args_are_mapped() {
        let value = json!({
            "api": {"http": "127.0.0.1:9091"},
            "plugins": [{
                "tag": "stats_collector",
                "type": "stats_collector",
                "args": {
                    "enabled": true,
                    "capacity": 500,
                    "history_max": 48,
                    "top_max": 20,
                    "logs_max": 50,
                    "dump_file": "/tmp/stats.dump",
                    "dump_interval": 120
                }
            }]
        });
        let cfg = stats_config_from_value(&value);
        assert!(cfg.enabled);
        assert_eq!(cfg.capacity, 500);
        assert_eq!(cfg.history_max, 48);
        assert_eq!(cfg.top_max, 20);
        assert_eq!(cfg.logs_max, 50);
        assert_eq!(cfg.dump_path, Some(PathBuf::from("/tmp/stats.dump")));
        assert_eq!(cfg.dump_interval_secs, 120);
    }

    #[test]
    fn stats_plugin_is_disabled_when_absent_or_flagged() {
        let cfg = stats_config_from_value(&json!({"plugins": []}));
        assert!(!cfg.enabled);

        let value = json!({
            "plugins": [{
                "type": "stats_collector",
                "args": {"enabled": false}
            }]
        });
        assert!(!stats_config_from_value(&value).enabled);

        let value = json!({
            "plugins": [{
                "type": "stats_collector",
                "args": {"enabled": true}
            }]
        });
        assert!(stats_config_from_value(&value).enabled);
    }

    #[test]
    fn stats_plugin_values_are_clamped() {
        let value = json!({
            "plugins": [{
                "type": "stats_collector",
                "args": {
                    "enabled": true,
                    "capacity": 10,
                    "history_max": 500,
                    "top_max": 5000,
                    "logs_max": 100_000,
                    "dump_interval": 5
                }
            }]
        });
        let cfg = stats_config_from_value(&value);
        assert_eq!(cfg.capacity, 100);
        assert_eq!(cfg.history_max, 168);
        assert_eq!(cfg.top_max, 1000);
        assert_eq!(cfg.logs_max, 100);
        assert_eq!(cfg.dump_interval_secs, 60);
    }

    #[test]
    fn dump_path_supports_paths_flags_and_defaults() {
        assert_eq!(
            dump_path_from_args(&json!({"dump_file": "/tmp/a.dump"})),
            Some(PathBuf::from("/tmp/a.dump"))
        );
        assert_eq!(
            dump_path_from_args(&json!({"dump_file": "1"})),
            Some(PathBuf::from(DEFAULT_STATS_DUMP_PATH))
        );
        assert_eq!(
            dump_path_from_args(&json!({"dump_file": true})),
            Some(PathBuf::from(DEFAULT_STATS_DUMP_PATH))
        );
        assert_eq!(dump_path_from_args(&json!({"dump_file": 0})), None);
        assert_eq!(dump_path_from_args(&json!({"dump_file": ""})), None);
        assert_eq!(
            dump_path_from_args(&json!({"dump_file": "1", "dump_path": "/tmp/b.dump"})),
            Some(PathBuf::from("/tmp/b.dump"))
        );
    }

    #[test]
    fn odd_plugin_args_do_not_panic() {
        for value in [
            json!(null),
            json!([]),
            json!({"plugins": 5}),
            json!({"plugins": [{"type": "stats_collector", "args": null}]}),
            json!({"plugins": [{"type": "stats_collector", "args": {
                "capacity": -1, "enabled": "maybe", "dump_file": {}, "dump_interval": []
            }}]}),
        ] {
            let cfg = stats_config_from_value(&value);
            assert!(cfg.capacity >= 100);
        }
    }

    #[test]
    fn config_loader_accepts_json_and_yaml() {
        let dir = temp_dir();
        let yaml = dir.join("config.yaml");
        std::fs::write(&yaml, "api:\n  http: 127.0.0.1:9091\nplugins: []\n").unwrap();
        let value = load_config_value(&yaml).unwrap();
        assert_eq!(
            value.pointer("/api/http").and_then(|v| v.as_str()),
            Some("127.0.0.1:9091")
        );

        let json_path = dir.join("config.json");
        std::fs::write(&json_path, r#"{"api":{"http":"127.0.0.1:9091"}}"#).unwrap();
        assert_eq!(
            load_config_value(&json_path)
                .unwrap()
                .pointer("/api/http")
                .and_then(|v| v.as_str()),
            Some("127.0.0.1:9091")
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn config_loader_rejects_scalars_and_invalid_syntax() {
        let dir = temp_dir();
        let scalar = dir.join("scalar.yaml");
        std::fs::write(&scalar, "just-a-string\n").unwrap();
        assert!(load_config_value(&scalar).is_err());

        let broken = dir.join("broken.yaml");
        std::fs::write(&broken, "api: [unclosed\n").unwrap();
        assert!(load_config_value(&broken).is_err());

        assert!(load_config_value(&dir.join("missing.json")).is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn stats_dump_is_loaded_into_a_fresh_collector() {
        let dir = temp_dir();
        let path = dir.join("stats.dump");

        let source = StatsCollector::start(StatsConfig::default());
        source.record(QueryEvent {
            client_ip: "192.0.2.1".to_string(),
            domain: "Example.COM.".to_string(),
            qtype: 1,
            status: 0,
            elapsed_ms: 4,
            ..QueryEvent::default()
        });
        source.flush().await;
        source.dump(&path).await.unwrap();

        let restored = StatsCollector::start(StatsConfig::default());
        load_stats_dump(&restored, &path);
        restored.flush().await;

        assert_eq!(restored.stats().total_queries, 1);
        let logs = restored.logs(&LogQuery::default());
        assert_eq!(logs.total, 1);
        assert_eq!(logs.items[0].domain, "example.com");

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cf_apply_hosts_parses_repeated_entries_and_optional_reload() {
        let cli = Cli::try_parse_from([
            "rust-dns",
            "cf-apply-hosts",
            "--file",
            "/tmp/hosts.txt",
            "--entry",
            "cloudflare.com=104.16.0.1",
            "--entry",
            "speed.cloudflare.com=104.16.0.2",
            "--reload-init",
            "/etc/init.d/rust-dns",
        ])
        .unwrap();
        match cli.command {
            Command::CfApplyHosts {
                file,
                entry,
                reload_init,
            } => {
                assert_eq!(file, PathBuf::from("/tmp/hosts.txt"));
                assert_eq!(
                    entry,
                    vec![
                        "cloudflare.com=104.16.0.1".to_string(),
                        "speed.cloudflare.com=104.16.0.2".to_string()
                    ]
                );
                assert_eq!(reload_init, Some(PathBuf::from("/etc/init.d/rust-dns")));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // At least one --entry is required.
        assert!(Cli::try_parse_from(["rust-dns", "cf-apply-hosts"]).is_err());
    }

    #[test]
    fn cf_subcommand_forwards_raw_hyphenated_args() {
        let cli = Cli::try_parse_from([
            "rust-dns",
            "cf-optimize",
            "--dry-run",
            "--output",
            "/tmp/out.json",
            "--mark",
            "0x10",
        ])
        .unwrap();
        match cli.command {
            Command::CfOptimize { args } => assert_eq!(
                args,
                vec!["--dry-run", "--output", "/tmp/out.json", "--mark", "0x10"]
            ),
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
