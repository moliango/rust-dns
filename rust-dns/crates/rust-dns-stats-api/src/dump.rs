// SPDX-License-Identifier: GPL-3.0-only
//! Versioned JSON dump/load for the statistics collector.
//!
//! Format (design §5):
//!
//! ```json
//! {"magic":"RDNSS","version":1,"saved_at":1763900000,"capacity":2000,
//!  "counters":{...},"records":[...],"history":[...],"top":{...}}
//! ```

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::types::{HistoryPoint, QueryRecord};

/// Magic header expected in every dump file.
pub const DUMP_MAGIC: &str = "RDNSS";
/// Current dump format version.
pub const DUMP_VERSION: u32 = 1;

/// Counter block of a dump file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpCounters {
    /// Total recorded queries.
    pub total_queries: u64,
    /// Blocked query count.
    pub blocked_queries: u64,
    /// Cached query count.
    pub cached_queries: u64,
    /// Sum of query durations in microseconds.
    pub latency_us_sum: u64,
}

/// Top tables of a dump file. `BTreeMap` keeps the on-disk JSON deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpTop {
    /// Domain -> count.
    #[serde(default)]
    pub domains: BTreeMap<String, u64>,
    /// Blocked domain -> count.
    #[serde(default)]
    pub blocked: BTreeMap<String, u64>,
    /// Client IP -> count.
    #[serde(default)]
    pub clients: BTreeMap<String, u64>,
}

/// Full dump envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpData {
    /// Always [`DUMP_MAGIC`].
    pub magic: String,
    /// Format version, currently [`DUMP_VERSION`].
    pub version: u32,
    /// Wall-clock save time as Unix seconds.
    pub saved_at: i64,
    /// Ring buffer capacity at save time.
    pub capacity: usize,
    /// Counters.
    pub counters: DumpCounters,
    /// Ring buffer contents, oldest first.
    #[serde(default)]
    pub records: Vec<QueryRecord>,
    /// History buckets, chronologically ascending.
    #[serde(default)]
    pub history: Vec<HistoryPoint>,
    /// Top aggregates.
    #[serde(default)]
    pub top: DumpTop,
}

impl DumpData {
    /// Build a valid, empty envelope.
    pub fn empty(saved_at: i64, capacity: usize) -> Self {
        Self {
            magic: DUMP_MAGIC.to_string(),
            version: DUMP_VERSION,
            saved_at,
            capacity,
            counters: DumpCounters::default(),
            records: Vec::new(),
            history: Vec::new(),
            top: DumpTop::default(),
        }
    }

    /// Whether the envelope carries the expected magic and version.
    pub fn is_valid(&self) -> bool {
        self.magic == DUMP_MAGIC && self.version == DUMP_VERSION && self.capacity > 0
    }
}

/// Result of reading a dump file.
#[derive(Debug, Clone)]
pub enum LoadOutcome {
    /// No dump file present; start empty.
    Missing,
    /// A valid envelope was read.
    Loaded(Box<DumpData>),
    /// The file existed but was unreadable, malformed or from another version.
    Corrupt,
}

/// Write `data` to `path` atomically: `<path>.tmp`, fsync, rename.
pub fn write_dump(path: &Path, data: &DumpData) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_vec_pretty(data).map_err(io::Error::other)?;
    let tmp = temp_path(path);

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o644);
    }

    let mut file = options.open(&tmp)?;
    file.write_all(&json)?;
    file.sync_all()?;
    drop(file);

    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Read and validate a dump file.
pub fn read_dump(path: &Path) -> LoadOutcome {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return LoadOutcome::Missing,
        Err(_) => return LoadOutcome::Corrupt,
    };

    match serde_json::from_slice::<DumpData>(&bytes) {
        Ok(data) if data.is_valid() => LoadOutcome::Loaded(Box::new(data)),
        _ => LoadOutcome::Corrupt,
    }
}

/// `<path>.corrupt.<unix>` used to quarantine an unreadable dump.
pub fn corrupt_path(path: &Path, unix_secs: i64) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".corrupt.{unix_secs}"));
    std::path::PathBuf::from(name)
}

fn temp_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    std::path::PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_magic_and_version() {
        let mut data = DumpData::empty(1, 2000);
        assert!(data.is_valid());
        data.magic = "NOPE".to_string();
        assert!(!data.is_valid());

        let mut data = DumpData::empty(1, 2000);
        data.version = 99;
        assert!(!data.is_valid());
    }
}
