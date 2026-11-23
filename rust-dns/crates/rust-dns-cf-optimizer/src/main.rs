// SPDX-License-Identifier: GPL-3.0-only
//! Standalone entry point mirroring the `rust-dns cf-optimize` subcommand.
//!
//! Exit codes follow design §2.1: 0 success, 1 parameter/run error, 2 cancelled,
//! 3 bypass verification failed. Clap parse failures are mapped to 1 so they do
//! not collide with the cancelled code.

use clap::Parser;
use rust_dns_cf_optimizer::{Args, Cancellation, exit_code_for_parse_error, pipeline};

#[tokio::main]
async fn main() {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(err) => {
            let _ = err.print();
            std::process::exit(exit_code_for_parse_error(&err));
        }
    };
    let config = args.into_config();

    // A cancel flag left by a previous run must not abort this one.
    if let Err(err) = Cancellation::new(&config.cancel_file).clear() {
        eprintln!(
            "cf-optimize: warning: cannot clear stale cancel file {}: {err:#}",
            config.cancel_file.display()
        );
    }

    match pipeline::run(&config).await {
        Ok(outcome) => {
            let passed = outcome
                .report
                .results
                .iter()
                .filter(|row| row.passed)
                .count();
            eprintln!(
                "cf-optimize: {} result(s), {} passed; JSON {} CSV {}; exit {}",
                outcome.report.results.len(),
                passed,
                config.output.display(),
                outcome.csv_path.display(),
                outcome.exit_code
            );
            std::process::exit(outcome.exit_code);
        }
        Err(err) => {
            eprintln!("cf-optimize: {err:#}");
            std::process::exit(1);
        }
    }
}
