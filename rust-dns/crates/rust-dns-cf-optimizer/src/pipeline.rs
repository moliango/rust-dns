// SPDX-License-Identifier: GPL-3.0-only
//! The end-to-end optimizer pipeline: candidates -> latency -> filter ->
//! download -> score/sort -> JSON/CSV/progress.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::bypass::{
    BypassGuard, BypassPlan, CommandRunner, EgressVerifier, HttpsEgressVerifier, OpenClashGuard,
    SystemCommandRunner,
};
use crate::ip_source::{DEFAULT_RANGE_CACHE_DIR, HttpsRangeFetcher, RangeFetcher, load_candidates};
use crate::output::{
    Cancellation, ProgressWriter, format_rfc3339, new_run_id, now_utc, write_report,
    write_results_csv,
};
use crate::probe::{
    DownloadProbe, DownloadUrl, DryRunDownloadProbe, DryRunLatencyProbe, HttpDownloadProbe,
    LatencyProbe, TcpLatencyProbe,
};
use crate::types::{
    BypassInfo, BypassMode, EXIT_BYPASS_UNVERIFIED, EXIT_CANCELLED, Family, IpSourceKind,
    LatencyStats, OptimizerConfig, Progress, ProgressPhase, ProgressState, ResultReport, ResultRow,
    RunOutcome, RunParams, sort_results, summarize_latency,
};

/// Execute a run, selecting real or dry-run probes from the configuration.
pub async fn run(config: &OptimizerConfig) -> Result<RunOutcome> {
    if config.dry_run {
        let mut dry = config.clone();
        dry.source = IpSourceKind::Fixture;
        dry.bypass_mode = BypassMode::None;
        dry.pause_openclash = false;
        validate_config(&dry)?;
        let latency = DryRunLatencyProbe::new(dry.family, dry.ping);
        let download = DryRunDownloadProbe;
        return run_with(&dry, latency, download).await;
    }

    validate_config(config)?;
    let plan = BypassPlan::from_config(config)?;
    let socket_options = plan.probe_socket_options();
    let latency = TcpLatencyProbe::new(
        config.port,
        config.ping,
        Duration::from_millis(config.timeout_ms),
        socket_options.clone(),
    );
    let url = DownloadUrl::parse(&config.url)?;
    let download = HttpDownloadProbe::new(
        url,
        socket_options,
        Duration::from_millis(config.timeout_ms),
        Duration::from_secs(config.download_time.max(1)),
        config.download_bytes.max(1),
    )?;
    let ranges = HttpsRangeFetcher::new(DEFAULT_RANGE_CACHE_DIR)?;
    let verifier = HttpsEgressVerifier::new(
        config.family,
        Duration::from_millis(config.timeout_ms.max(5000)),
    )?;
    run_with_sources(config, latency, download, &ranges, &verifier).await
}

/// Testable core with default network sources: probes are injected, so unit
/// tests that select `fixture`/`file` sources never touch the network.
pub async fn run_with<L, D>(
    config: &OptimizerConfig,
    latency_probe: L,
    download_probe: D,
) -> Result<RunOutcome>
where
    L: LatencyProbe + 'static,
    D: DownloadProbe + 'static,
{
    let ranges = HttpsRangeFetcher::new(DEFAULT_RANGE_CACHE_DIR)?;
    let verifier = HttpsEgressVerifier::new(
        config.family,
        Duration::from_millis(config.timeout_ms.max(5000)),
    )?;
    run_with_sources(config, latency_probe, download_probe, &ranges, &verifier).await
}

/// Fully injectable pipeline: ranges and the egress verifier can be faked so
/// tests neither hit the network nor modify the host firewall.
pub async fn run_with_sources<L, D, R, V>(
    config: &OptimizerConfig,
    latency_probe: L,
    download_probe: D,
    ranges: &R,
    verifier: &V,
) -> Result<RunOutcome>
where
    L: LatencyProbe + 'static,
    D: DownloadProbe + 'static,
    R: RangeFetcher + ?Sized,
    V: EgressVerifier + ?Sized,
{
    let runner = SystemCommandRunner;
    run_with_all(
        config,
        latency_probe,
        download_probe,
        ranges,
        verifier,
        &runner,
    )
    .await
}

/// Like [`run_with_sources`], but the command runner is injectable too, so
/// tests can exercise mark/device setup and verification with a mock runner.
pub async fn run_with_all<L, D, R, V>(
    config: &OptimizerConfig,
    latency_probe: L,
    download_probe: D,
    ranges: &R,
    verifier: &V,
    runner: &dyn CommandRunner,
) -> Result<RunOutcome>
where
    L: LatencyProbe + 'static,
    D: DownloadProbe + 'static,
    R: RangeFetcher + ?Sized,
    V: EgressVerifier + ?Sized,
{
    validate_config(config)?;
    let started_at = now_utc();
    let run_id = new_run_id();
    let cancel = Cancellation::new(&config.cancel_file);
    let mut progress_writer = ProgressWriter::new(&config.progress);
    let mut progress = Progress::new(ProgressPhase::Latency, 0, "starting");
    progress_writer.update(&progress, true)?;

    let plan = BypassPlan::from_config(config)?;

    // `--pause-openclash` is opt-in; it runs before any bypass rules so the
    // direct-egress check sees the same service state as the probes.
    let mut oc_guard = if !config.dry_run && config.pause_openclash {
        match OpenClashGuard::install(runner) {
            Ok(guard) => Some(guard),
            Err(err) => {
                progress.state = ProgressState::Error;
                progress.message = format!("{err:#}");
                let _ = progress_writer.update(&progress, true);
                return Err(err).context("pause OpenClash for the run");
            }
        }
    } else {
        None
    };

    let mut guard = if !config.dry_run && plan.mode != BypassMode::None {
        match BypassGuard::install(plan.clone(), runner) {
            Ok(guard) => Some(guard),
            Err(err) => {
                if let Some(mut paused) = oc_guard.take() {
                    // Best effort; the setup error below is the one reported.
                    let _ = paused.cleanup();
                }
                progress.state = ProgressState::Error;
                progress.message = format!("{err:#}");
                let _ = progress_writer.update(&progress, true);
                return Err(err).context("install temporary OpenClash bypass rules");
            }
        }
    } else {
        None
    };

    let mut bypass = if config.dry_run {
        BypassInfo::dry_run()
    } else if plan.mode == BypassMode::None {
        BypassInfo::disabled()
    } else {
        plan.info(
            false,
            None,
            "temporary bypass rules installed; direct-egress verification pending",
        )
    };

    // Direct-egress check with the exact socket identity used by the probes.
    if !config.dry_run && plan.mode != BypassMode::None {
        let socket_options = plan.probe_socket_options();
        if cancel.is_cancelled() {
            bypass = plan.info(false, None, "cancelled before direct-egress verification");
        } else {
            bypass = match verifier.verify(&socket_options).await {
                Ok(check) => plan.info(
                    check.verified,
                    check.egress_ip.map(|ip| ip.to_string()),
                    check.note,
                ),
                Err(err) => plan.info(
                    false,
                    None,
                    format!("direct-egress verification failed: {err:#}"),
                ),
            };
        }
    }

    let core = if cancel.is_cancelled() {
        Ok(CoreOutcome {
            rows: Vec::new(),
            cancelled: true,
        })
    } else {
        execute_core(
            config,
            latency_probe,
            download_probe,
            ranges,
            &cancel,
            &mut progress,
            &mut progress_writer,
        )
        .await
    };

    let outcome = match core {
        Ok(outcome) => outcome,
        Err(err) => {
            progress.state = ProgressState::Error;
            progress.message = format!("{err:#}");
            let _ = progress_writer.update(&progress, true);
            if let Some(mut installed) = guard.take() {
                let _ = installed.cleanup();
            }
            if let Some(mut paused) = oc_guard.take() {
                let _ = paused.cleanup();
            }
            return Err(err);
        }
    };

    if let Some(mut installed) = guard.take()
        && let Err(err) = installed.cleanup()
    {
        bypass.note = format!("{}; cleanup failed: {err:#}", bypass.note);
    }
    if let Some(mut paused) = oc_guard.take() {
        match paused.cleanup() {
            Ok(()) => bypass.note = format!("{}; OpenClash paused for the run", bypass.note),
            Err(err) => bypass.note = format!("{}; OpenClash restart failed: {err:#}", bypass.note),
        }
    }

    let mut rows = outcome.rows;
    let has_results = !rows.is_empty();
    sort_results(&mut rows);
    let unverified = !config.dry_run && plan.mode != BypassMode::None && !bypass.verified;
    let report = ResultReport {
        run_id,
        started_at: format_rfc3339(started_at),
        finished_at: Some(format_rfc3339(now_utc())),
        params: RunParams::from_config(config),
        bypass,
        results: rows,
    };

    let write_result = (|| -> Result<std::path::PathBuf> {
        write_report(&config.output, &report).context("write result JSON")?;
        write_results_csv(&config.output, &report.results).context("write result CSV")
    })();
    let csv_path = match write_result {
        Ok(path) => path,
        Err(err) => {
            progress.state = ProgressState::Error;
            progress.message = format!("{err:#}");
            let _ = progress_writer.update(&progress, true);
            return Err(err);
        }
    };

    progress.state = if outcome.cancelled {
        ProgressState::Cancelled
    } else {
        ProgressState::Done
    };
    progress.message = if outcome.cancelled {
        "cancelled; partial results written".to_string()
    } else if unverified {
        format!(
            "results written to {}; bypass unverified (see bypass.note)",
            config.output.display()
        )
    } else {
        format!("results written to {}", config.output.display())
    };
    let final_state = progress.state;
    progress_writer.finish(progress, final_state)?;

    let exit_code = if outcome.cancelled {
        EXIT_CANCELLED
    } else if unverified && has_results {
        EXIT_BYPASS_UNVERIFIED
    } else {
        0
    };

    Ok(RunOutcome {
        report,
        csv_path,
        exit_code,
    })
}

struct CoreOutcome {
    rows: Vec<ResultRow>,
    cancelled: bool,
}

async fn execute_core<L, D, R>(
    config: &OptimizerConfig,
    latency_probe: L,
    download_probe: D,
    ranges: &R,
    cancel: &Cancellation,
    progress: &mut Progress,
    writer: &mut ProgressWriter,
) -> Result<CoreOutcome>
where
    L: LatencyProbe + 'static,
    D: DownloadProbe + 'static,
    R: RangeFetcher + ?Sized,
{
    let candidates = load_candidates(config, ranges)
        .await
        .context("load candidate IPs")?;

    // ---- latency phase -----------------------------------------------------
    progress.phase = ProgressPhase::Latency;
    progress.tested = 0;
    progress.total = candidates.len() as u64;
    progress.message = format!("probing TCP handshakes on port {}", config.port);
    writer.update(progress, true)?;

    let latency_probe = Arc::new(latency_probe);
    let semaphore = Arc::new(Semaphore::new(config.threads.max(1)));
    let mut joins = JoinSet::new();
    let mut cancelled = false;
    for ip in &candidates {
        if cancel.is_cancelled() {
            cancelled = true;
            break;
        }
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .context("acquire latency probe slot")?;
        if cancel.is_cancelled() {
            cancelled = true;
            break;
        }
        let probe = Arc::clone(&latency_probe);
        let ip = *ip;
        joins.spawn(async move {
            let _permit = permit;
            let samples = probe.probe(ip).await;
            (ip, summarize_latency(&samples))
        });
    }

    let mut latency_results: Vec<(std::net::IpAddr, LatencyStats)> = Vec::new();
    if !cancelled {
        loop {
            tokio::select! {
                joined = joins.join_next() => {
                    let Some(joined) = joined else { break };
                    let (ip, stats) = joined.context("latency probe task failed")?;
                    latency_results.push((ip, stats));
                    progress.tested += 1;
                    writer.update(progress, false)?;
                    if cancel.is_cancelled() {
                        cancelled = true;
                        break;
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(250)) => {
                    if cancel.is_cancelled() {
                        cancelled = true;
                        break;
                    }
                }
            }
        }
    }
    if cancelled {
        joins.abort_all();
    }
    latency_results.sort_by_key(|entry| entry.0);

    let mut rows: Vec<ResultRow> = latency_results
        .iter()
        .map(|(ip, stats)| ResultRow::new(*ip, config.family, *stats))
        .collect();

    if cancelled {
        return Ok(CoreOutcome {
            rows,
            cancelled: true,
        });
    }

    // ---- filter + download phase ------------------------------------------
    let mut download_candidates: Vec<(usize, std::net::IpAddr, LatencyStats)> = latency_results
        .iter()
        .enumerate()
        .filter(|(_, (_, stats))| stats.passes(config.max_latency_ms, config.max_loss_pct))
        .map(|(index, (ip, stats))| (index, *ip, *stats))
        .collect();
    download_candidates.sort_by(|a, b| {
        let left = a.2.avg_ms.unwrap_or(f64::INFINITY);
        let right = b.2.avg_ms.unwrap_or(f64::INFINITY);
        left.partial_cmp(&right)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    download_candidates.truncate(config.download_count);

    progress.phase = ProgressPhase::Download;
    progress.tested = 0;
    progress.total = download_candidates.len() as u64;
    progress.message = format!(
        "download testing {} candidate(s) with concurrency {}",
        download_candidates.len(),
        config.download_concurrency.max(1)
    );
    writer.update(progress, true)?;

    if !download_candidates.is_empty() {
        let download_probe = Arc::new(download_probe);
        let semaphore = Arc::new(Semaphore::new(config.download_concurrency.max(1)));
        let mut downloads = JoinSet::new();
        for (row_index, ip, _) in &download_candidates {
            if cancel.is_cancelled() {
                cancelled = true;
                break;
            }
            let permit = semaphore
                .clone()
                .acquire_owned()
                .await
                .context("acquire download probe slot")?;
            if cancel.is_cancelled() {
                cancelled = true;
                break;
            }
            let probe = Arc::clone(&download_probe);
            let ip = *ip;
            let row_index = *row_index;
            downloads.spawn(async move {
                let _permit = permit;
                let speed = probe.measure(ip).await.unwrap_or(0.0);
                (row_index, speed)
            });
        }

        if !cancelled {
            loop {
                tokio::select! {
                    joined = downloads.join_next() => {
                        let Some(joined) = joined else { break };
                        let (row_index, speed) = joined.context("download probe task failed")?;
                        if let Some(row) = rows.get_mut(row_index) {
                            row.speed_mbps = speed;
                        }
                        progress.tested += 1;
                        writer.update(progress, false)?;
                        if cancel.is_cancelled() {
                            cancelled = true;
                            break;
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {
                        if cancel.is_cancelled() {
                            cancelled = true;
                            break;
                        }
                    }
                }
            }
        }
        if cancelled {
            downloads.abort_all();
        }
    }

    // ---- score + sort ------------------------------------------------------
    for (index, row) in rows.iter_mut().enumerate() {
        let stats = latency_results[index].1;
        row.passed = !cancelled
            && stats.min_ms.is_some()
            && stats.passes(config.max_latency_ms, config.max_loss_pct)
            && row.speed_mbps >= config.min_speed_mbps;
    }

    progress.phase = ProgressPhase::Verify;
    progress.tested = progress.total;
    progress.message = "writing results".to_string();
    writer.update(progress, true)?;

    Ok(CoreOutcome { rows, cancelled })
}

/// Validate numeric/range inputs before any probe or firewall action.
pub fn validate_config(config: &OptimizerConfig) -> Result<()> {
    if config.port == 0 {
        bail!("--port must be between 1 and 65535");
    }
    if config.threads == 0 {
        bail!("--threads must be at least 1");
    }
    if config.ping == 0 {
        bail!("--ping must be at least 1");
    }
    if config.max_candidates == 0 {
        bail!("--max-candidates must be at least 1");
    }
    if config.download_concurrency == 0 {
        bail!("--download-concurrency must be at least 1");
    }
    if config.download_count > 0 {
        if config.download_time == 0 {
            bail!("--download-time must be at least 1 second");
        }
        if config.download_bytes == 0 {
            bail!("--download-bytes must be at least 1");
        }
        DownloadUrl::parse(&config.url).context("--url")?;
    }
    if !config.max_latency_ms.is_finite() || config.max_latency_ms < 0.0 {
        bail!("--max-latency must be a non-negative number");
    }
    if !config.max_loss_pct.is_finite() || !(0.0..=100.0).contains(&config.max_loss_pct) {
        bail!("--max-loss must be between 0 and 100");
    }
    if !config.min_speed_mbps.is_finite() || config.min_speed_mbps < 0.0 {
        bail!("--min-speed must be a non-negative number");
    }
    if config.timeout_ms == 0 {
        bail!("--timeout-ms must be at least 1");
    }
    if config.source == IpSourceKind::File && config.ip_file.is_none() {
        bail!("--source file requires --ip-file");
    }
    if config.family != Family::V4 && config.family != Family::V6 {
        bail!("--family must be 4 or 6");
    }
    // Also validates bypass-mode requirements (device, mark, table).
    BypassPlan::from_config(config)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bypass::EgressCheck;
    use crate::probe::{DownloadProbe, LatencyProbe};
    use crate::types::{Family, ProgressState};
    use std::net::IpAddr;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR: AtomicU64 = AtomicU64::new(0);

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rust-dns-cf-pipeline-{}-{}",
            std::process::id(),
            TEST_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Fake probe returning canned samples keyed by IP.
    struct FakeLatency {
        samples: Mutex<Vec<Option<Duration>>>,
    }

    impl LatencyProbe for FakeLatency {
        fn probe(
            &self,
            _ip: IpAddr,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Option<Duration>>> + Send + '_>>
        {
            let samples = self.samples.lock().unwrap().clone();
            Box::pin(async move { samples })
        }
    }

    struct FakeDownload {
        speed: f64,
    }

    impl DownloadProbe for FakeDownload {
        fn measure(
            &self,
            _ip: IpAddr,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<f64>> + Send + '_>> {
            let speed = self.speed;
            Box::pin(async move { Ok(speed) })
        }
    }

    fn dry_config(dir: &std::path::Path) -> OptimizerConfig {
        OptimizerConfig {
            dry_run: true,
            family: Family::V4,
            threads: 4,
            ping: 4,
            download_count: 3,
            download_concurrency: 2,
            max_latency_ms: 100.0,
            max_loss_pct: 10.0,
            min_speed_mbps: 10.0,
            output: dir.join("last.json"),
            progress: dir.join("progress.json"),
            cancel_file: dir.join("cancel"),
            ..OptimizerConfig::default()
        }
    }

    fn read_json(path: &std::path::Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn dry_run_writes_json_csv_and_progress() {
        let dir = tempdir();
        let config = dry_config(&dir);
        let outcome = run(&config).await.unwrap();
        assert_eq!(outcome.exit_code, 0);

        let json = read_json(&dir.join("last.json"));
        assert!(json["run_id"].as_str().unwrap().contains('-'));
        assert_eq!(json["params"]["family"], 4);
        assert_eq!(json["params"]["download_count"], 3);
        assert_eq!(json["bypass"]["mode"], "none");
        assert_eq!(json["bypass"]["verified"], true);
        let results = json["results"].as_array().unwrap();
        assert_eq!(results.len(), 10);
        assert_eq!(results[0]["passed"], true);
        assert!(results[0]["speed_mbps"].as_f64().unwrap() > 0.0);

        let csv = std::fs::read_to_string(&outcome.csv_path).unwrap();
        let header = csv.lines().next().unwrap();
        assert_eq!(
            header,
            "IP,family,latency_min_ms,latency_avg_ms,loss_pct,speed_mbps,passed"
        );
        assert_eq!(csv.lines().count(), 11);

        let progress = read_json(&dir.join("progress.json"));
        assert_eq!(progress["state"], "done");
        assert_eq!(progress["phase"], "verify");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn dry_run_results_are_deterministic_and_sorted() {
        let dir = tempdir();
        let config = dry_config(&dir);
        let first = run(&config).await.unwrap();
        let second = run(&config).await.unwrap();

        let keys = |outcome: &RunOutcome| -> Vec<(String, f64, f64, bool)> {
            outcome
                .report
                .results
                .iter()
                .map(|r| (r.ip.clone(), r.speed_mbps, r.latency_avg_ms, r.passed))
                .collect()
        };
        assert_eq!(keys(&first), keys(&second));
        let speeds: Vec<f64> = first.report.results.iter().map(|r| r.speed_mbps).collect();
        assert!(speeds.windows(2).all(|w| w[0] >= w[1]));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn thresholds_filter_candidates_and_mark_unpassed_rows() {
        let dir = tempdir();
        let mut config = dry_config(&dir);
        config.max_loss_pct = 0.0;
        config.min_speed_mbps = 1000.0;
        config.download_count = 10;
        let outcome = run(&config).await.unwrap();
        assert!(outcome.report.results.iter().all(|r| !r.passed));
        assert!(outcome.report.results.iter().any(|r| r.loss_pct > 0.0));
        assert!(outcome.report.results.iter().any(|r| r.speed_mbps == 0.0));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn preexisting_cancel_file_aborts_before_probing() {
        let dir = tempdir();
        let config = dry_config(&dir);
        std::fs::write(&config.cancel_file, b"").unwrap();
        let outcome = run(&config).await.unwrap();
        assert_eq!(outcome.exit_code, EXIT_CANCELLED);
        assert!(outcome.report.results.is_empty());
        let progress = read_json(&config.progress);
        assert_eq!(progress["state"], "cancelled");
        assert_eq!(progress["state"], ProgressState::Cancelled.as_str());
        let _ = std::fs::remove_dir_all(dir);
    }

    struct SlowLatency {
        delay: Duration,
    }

    impl LatencyProbe for SlowLatency {
        fn probe(
            &self,
            _ip: IpAddr,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Option<Duration>>> + Send + '_>>
        {
            let delay = self.delay;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                vec![Some(delay)]
            })
        }
    }

    #[tokio::test]
    async fn cancel_file_during_probes_stops_the_run() {
        let dir = tempdir();
        let mut config = dry_config(&dir);
        config.dry_run = true;
        config.threads = 1;
        let cancel_path = config.cancel_file.clone();
        let handle = tokio::spawn(async move {
            let latency = SlowLatency {
                delay: Duration::from_millis(300),
            };
            let download = FakeDownload { speed: 1.0 };
            run_with(&config, latency, download).await
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        std::fs::write(&cancel_path, b"").unwrap();

        let outcome = handle.await.unwrap().unwrap();
        assert_eq!(outcome.exit_code, EXIT_CANCELLED);
        let progress = read_json(&dir.join("progress.json"));
        assert_eq!(progress["state"], "cancelled");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn injected_probes_drive_latency_math_and_sorting() {
        let dir = tempdir();
        let mut config = dry_config(&dir);
        config.source = IpSourceKind::Fixture;
        config.dry_run = true; // avoids bypass while still using injected probes
        config.max_latency_ms = 1000.0;
        config.max_loss_pct = 100.0;
        config.min_speed_mbps = 0.0;
        config.download_count = 10;
        let latency = FakeLatency {
            samples: Mutex::new(vec![
                Some(Duration::from_millis(10)),
                Some(Duration::from_millis(30)),
                None,
                None,
            ]),
        };
        let download = FakeDownload { speed: 42.0 };
        let outcome = run_with(&config, latency, download).await.unwrap();
        assert_eq!(outcome.report.results.len(), 10);
        for row in &outcome.report.results {
            assert_eq!(row.latency_min_ms, 10.0);
            assert_eq!(row.latency_avg_ms, 20.0);
            assert_eq!(row.loss_pct, 50.0);
            assert_eq!(row.speed_mbps, 42.0);
        }
        assert!(outcome.report.results.iter().all(|r| r.passed));
        let _ = std::fs::remove_dir_all(dir);
    }

    struct FakeRanges {
        lines: Vec<String>,
    }

    impl RangeFetcher for FakeRanges {
        fn fetch<'a>(
            &'a self,
            _family: Family,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<String>>> + Send + 'a>>
        {
            let lines = self.lines.clone();
            Box::pin(async move { Ok(lines) })
        }
    }

    struct FakeVerifier {
        result: Result<EgressCheck, String>,
    }

    impl EgressVerifier for FakeVerifier {
        fn verify<'a>(
            &'a self,
            _options: &'a crate::probe::ProbeSocketOptions,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<EgressCheck>> + Send + 'a>>
        {
            let result = match &self.result {
                Ok(check) => Ok(check.clone()),
                Err(err) => Err(anyhow::anyhow!(err.clone())),
            };
            Box::pin(async move { result })
        }
    }

    fn mark_config(dir: &std::path::Path) -> OptimizerConfig {
        let mut config = dry_config(dir);
        config.dry_run = false;
        config.bypass_mode = BypassMode::Mark;
        config.device = Some("wan".to_string());
        config.source = IpSourceKind::Fixture;
        config.max_latency_ms = 1000.0;
        config.max_loss_pct = 100.0;
        config.min_speed_mbps = 0.0;
        config
    }

    fn push_mark_preflight(runner: &crate::bypass::MockCommandRunner) {
        // ip rule show (ok), nft list (must report the table is absent), ip link.
        runner.push_response(crate::bypass::CommandOutput::success());
        runner.push_failure(1, "No such file or directory");
        runner.push_response(crate::bypass::CommandOutput::success());
    }

    #[tokio::test]
    async fn cloudflare_source_uses_injected_ranges() {
        let dir = tempdir();
        let mut config = dry_config(&dir);
        config.dry_run = false;
        config.source = IpSourceKind::Cloudflare;
        config.max_candidates = 4;
        config.download_count = 4;
        let ranges = FakeRanges {
            lines: vec!["104.16.0.0/30".to_string()],
        };
        let verifier = FakeVerifier {
            result: Ok(EgressCheck {
                egress_ip: None,
                verified: false,
                note: "unused".into(),
            }),
        };
        let runner = crate::bypass::MockCommandRunner::new();
        let outcome = run_with_all(
            &config,
            FakeLatency {
                samples: Mutex::new(vec![Some(Duration::from_millis(5))]),
            },
            FakeDownload { speed: 10.0 },
            &ranges,
            &verifier,
            &runner,
        )
        .await
        .unwrap();
        assert_eq!(outcome.report.results.len(), 4);
        assert!(
            outcome
                .report
                .results
                .iter()
                .all(|row| row.speed_mbps == 10.0)
        );
        // No bypass mode means no host commands at all.
        assert!(runner.calls().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn failed_verification_exits_3_but_keeps_results() {
        let dir = tempdir();
        let config = mark_config(&dir);
        let runner = crate::bypass::MockCommandRunner::new();
        push_mark_preflight(&runner);
        let verifier = FakeVerifier {
            result: Err("still behind the proxy".to_string()),
        };
        let outcome = run_with_all(
            &config,
            FakeLatency {
                samples: Mutex::new(vec![Some(Duration::from_millis(5))]),
            },
            FakeDownload { speed: 42.0 },
            &FakeRanges { lines: Vec::new() },
            &verifier,
            &runner,
        )
        .await
        .unwrap();

        assert_eq!(outcome.exit_code, EXIT_BYPASS_UNVERIFIED);
        assert!(!outcome.report.bypass.verified);
        assert!(outcome.report.bypass.note.contains("verification failed"));
        assert!(!outcome.report.results.is_empty());

        let calls: Vec<String> = runner.calls().iter().map(ToString::to_string).collect();
        assert!(calls.contains(&"nft add table inet rust_dns_cf".to_string()));
        assert!(calls.contains(&"nft delete table inet rust_dns_cf".to_string()));
        assert!(calls.contains(&"ip rule del fwmark 0x400000 lookup 1688".to_string()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn successful_verification_records_egress_and_exits_0() {
        let dir = tempdir();
        let config = mark_config(&dir);
        let runner = crate::bypass::MockCommandRunner::new();
        push_mark_preflight(&runner);
        let verifier = FakeVerifier {
            result: Ok(EgressCheck {
                egress_ip: Some("203.0.113.10".parse().unwrap()),
                verified: true,
                note: "direct egress confirmed".to_string(),
            }),
        };
        let outcome = run_with_all(
            &config,
            FakeLatency {
                samples: Mutex::new(vec![Some(Duration::from_millis(5))]),
            },
            FakeDownload { speed: 42.0 },
            &FakeRanges { lines: Vec::new() },
            &verifier,
            &runner,
        )
        .await
        .unwrap();
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.report.bypass.verified);
        assert_eq!(
            outcome.report.bypass.egress_ip.as_deref(),
            Some("203.0.113.10")
        );
        assert_eq!(outcome.report.bypass.mark.as_deref(), Some("0x400000"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn pause_openclash_stops_and_restarts_only_when_requested() {
        let dir = tempdir();
        let mut config = dry_config(&dir);
        config.dry_run = false;
        config.pause_openclash = true;
        let runner = crate::bypass::MockCommandRunner::new();
        let verifier = FakeVerifier {
            result: Ok(EgressCheck {
                egress_ip: None,
                verified: false,
                note: "unused".into(),
            }),
        };
        let outcome = run_with_all(
            &config,
            FakeLatency {
                samples: Mutex::new(vec![Some(Duration::from_millis(5))]),
            },
            FakeDownload { speed: 10.0 },
            &FakeRanges { lines: Vec::new() },
            &verifier,
            &runner,
        )
        .await
        .unwrap();
        let calls: Vec<String> = runner.calls().iter().map(ToString::to_string).collect();
        assert_eq!(
            calls,
            vec![
                "/etc/init.d/openclash stop".to_string(),
                "/etc/init.d/openclash start".to_string(),
            ]
        );
        assert!(outcome.report.bypass.note.contains("OpenClash paused"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn invalid_config_is_rejected_before_io() {
        let dir = tempdir();
        let mut config = dry_config(&dir);
        config.threads = 0;
        let err = run(&config).await.unwrap_err();
        assert!(err.to_string().contains("--threads"));
        config.threads = 1;
        config.url = "ftp://example.com/x".to_string();
        let err = run(&config).await.unwrap_err();
        assert!(format!("{err:#}").contains("--url"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
