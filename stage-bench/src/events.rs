//! A timestamped log of the boundaries the SDK's own observations do not label.
//!
//! `OperationObservability` measures stages the SDK names. It does not say when
//! the driver *selected* a step, when it skipped a bundle, or how long the host
//! waited between two invocations — and those gaps are exactly where a host's
//! own pacing shows up. This log records those boundaries against the same
//! clock, so a phase table can name an unexplained interval instead of hiding
//! it inside a total.
//!
//! Append-only JSONL, flushed and fsynced per line for the same reason the
//! conformance suite's log is: the interesting runs are the ones that end
//! badly, and a buffered log loses precisely those.

use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// One boundary the round or tracking driver crossed.
///
/// `phase` is this crate's own vocabulary, deliberately prefixed to keep it
/// distinguishable from the SDK stage names in an observability snapshot: the
/// two are recorded against the same clock but are not the same taxonomy.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PhaseEvent {
    /// Microseconds since the Unix epoch, the same anchor
    /// `OperationObservability::started_at_unix_us` uses.
    pub at_unix_us: u64,
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_index: Option<u32>,
    /// A short rendering of whatever the boundary carried. Never a payload,
    /// never a URL, never key material.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl PhaseEvent {
    /// A bare boundary, stamped now.
    pub fn phase(phase: &str) -> Self {
        Self {
            at_unix_us: now_unix_us(),
            phase: phase.to_string(),
            step: None,
            bundle_index: None,
            proposal_id: None,
            share_index: None,
            detail: None,
        }
    }
}

/// The run's append-only phase log.
///
/// Written from several concurrent bundle tasks, so the file handle is behind
/// a mutex. Recording is best effort: a benchmark must not fail because its
/// diagnostic could not be written.
pub struct EventLog {
    file: Mutex<std::fs::File>,
}

impl EventLog {
    /// Name of the log inside a run directory.
    pub const FILE_NAME: &'static str = "events.jsonl";

    pub fn create(run_dir: &Path) -> std::io::Result<Self> {
        let file = std::fs::File::create(run_dir.join(Self::FILE_NAME))?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    pub fn record(&self, event: PhaseEvent) {
        let Ok(mut encoded) = serde_json::to_vec(&event) else {
            return;
        };
        encoded.push(b'\n');
        let Ok(mut file) = self.file.lock() else {
            return;
        };
        let _ = file.write_all(&encoded);
        let _ = file.flush();
        let _ = file.sync_all();
    }

    /// Reads a finished log back, skipping lines a killed process left partial.
    pub fn read(run_dir: &Path) -> std::io::Result<Vec<PhaseEvent>> {
        let raw = std::fs::read_to_string(run_dir.join(Self::FILE_NAME))?;
        Ok(raw
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect())
    }
}

fn now_unix_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}
