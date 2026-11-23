// SPDX-License-Identifier: GPL-3.0-only
//! Core error type.

use hickory_proto::ProtoError;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("config: {0}")]
    Config(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("dns: {0}")]
    Dns(#[from] ProtoError),
    #[error("upstream: {0}")]
    Upstream(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("internal: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;
