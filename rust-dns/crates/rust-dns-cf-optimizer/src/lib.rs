// SPDX-License-Identifier: GPL-3.0-only
//! Cloudflare IP optimizer: TCP latency probes, HTTPS download probes, result
//! JSON/CSV, progress/cancellation files, and OpenClash bypass plumbing.
//!
//! The public JSON/CSV/progress contract lives in
//! `.trellis/tasks/11-23-cloudflare-ip-optimizer/design.md` §2. This crate is
//! independent of the DNS core and of LuCI; the `rust-dns cf-optimize`
//! subcommand and the rpcd layer are expected to call [`pipeline::run`].
//!
//! Phases currently covered:
//! - candidate fixtures / IP-or-CIDR files, and the official Cloudflare
//!   `ips-v4`/`ips-v6` lists fetched over HTTPS and cached for 24 hours with a
//!   bundled fallback,
//! - concurrent TCP handshake latency (min/avg/loss) with mark/device/source
//!   socket options,
//! - concurrent HTTPS download probes with byte/time/concurrency limits,
//! - threshold filtering, deterministic dry-run, atomic JSON/CSV/progress writes,
//! - cancellation by flag file, temporary `ip rule`/`nft` bypass guard, a
//!   direct-egress (Cloudflare trace) verification, and an explicit
//!   `--pause-openclash` guard,
//! - managed-block writes for `/etc/rust-dns/rule/hosts.txt` with `.bak`,
//!   atomic replacement, and an optional service reload.
//!
//! Safety deviation from design §4.1: the design names `mark` as the default
//! bypass mode, but this crate defaults to `none` so an unconfigured run never
//! changes the host firewall or routing. `mark`, `device`, and
//! `--pause-openclash` are only used when requested explicitly.

pub mod bypass;
pub mod cli;
pub mod hosts;
pub mod ip_source;
pub mod output;
pub mod pipeline;
pub mod probe;
pub mod types;

pub use bypass::{EgressCheck, EgressVerifier, OpenClashGuard};
pub use cli::{Args, exit_code_for_parse_error};
pub use hosts::{ApplyOutcome, apply_hosts, parse_entry};
pub use ip_source::{HttpsRangeFetcher, RangeFetcher};
pub use output::Cancellation;
pub use pipeline::{run, run_with, run_with_all, run_with_sources, validate_config};
pub use types::{
    BypassInfo, Family, OptimizerConfig, Progress, ProgressPhase, ProgressState, ResultReport,
    ResultRow, RunOutcome, RunParams,
};
