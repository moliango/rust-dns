// SPDX-License-Identifier: GPL-3.0-only
//! Expose the Cargo target triple to `rust-dns version` without a runtime probe.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=RUST_DNS_TARGET={target}");
}
