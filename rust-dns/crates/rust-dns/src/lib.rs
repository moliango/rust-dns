// SPDX-License-Identifier: GPL-3.0-only
//! Library surface for the `rust-dns` binary.
//!
//! The DNS pipeline wiring lives in [`dns`] so the daemon and the end-to-end
//! tests build exactly the same handler.

pub mod dns;
