//! Multi-proposal staging benchmark for `zcash_voting`.
//!
//! One command provisions a round on the staging vote chain with a ballot the
//! caller chooses, drives one clean vote to completion, and reports where the
//! time went — per phase, per proposal, and with the concurrency each phase
//! actually reached.
//!
//! # Why it exists
//!
//! `docs/helper_delivery_benchmark.md` describes a capture procedure: run a
//! wallet against staging, save the observability snapshots from three
//! boundaries, then compute occupancy and percentiles by hand. That procedure
//! produced the numbers in the 32-slot admission work, and it is not repeatable
//! at any useful cost — so no phase below helper delivery has ever been
//! measured beside it, and a regression in proving, PIR, or chain advance would
//! be invisible until someone repeated the whole thing manually.
//!
//! This crate is that procedure as a command. It measures; it asserts almost
//! nothing.
//!
//! # What it is not
//!
//! Not a conformance suite. It injects no crash, no stall, and no helper
//! outage — `recovery-conformance` owns all three, and this crate borrows its
//! staging plumbing rather than duplicating it. A benchmark's only correctness
//! claim is that the round it timed actually completed, because a run that
//! delivered nothing quickly is not a fast run.
//!
//! Deliberately outside the workspace default members: it needs the network and
//! it provisions real rounds, so it must never run as part of `make test`.

pub mod ballot;
pub mod confirm;
pub mod drive;
pub mod events;
pub mod manifest;
pub mod metrics;
pub mod preflight;
pub mod provision;
pub mod run_config;
pub mod supervise;

pub use ballot::Ballot;
pub use manifest::Manifest;
pub use metrics::Metrics;
pub use run_config::{BenchOutcome, BenchRunConfig};

use std::path::Path;

use anyhow::{Context, Result};
use zcash_voting::OperationObservability;

/// One snapshot and the file it came from.
///
/// The SDK names both the round driver's and the share tracker's reported
/// invocation `run`, so the operation alone cannot tell two apart in a report.
/// The file name can, and it is also what a reader would go and open.
#[derive(Clone, Debug)]
pub struct CapturedSnapshot {
    pub source: String,
    pub snapshot: OperationObservability,
}

/// Reads every observability snapshot a run directory holds, in capture order.
///
/// The round drive first, then each background tracking invocation. A missing
/// file is not an error: a run that failed before tracking legitimately has
/// none, and the metrics describe what was captured.
pub fn read_snapshots(run_dir: &Path) -> Result<Vec<CapturedSnapshot>> {
    let mut snapshots = Vec::new();
    let mut names = vec!["round.observability.json".to_string()];
    for index in 0..MAX_TRACKING_SNAPSHOTS {
        names.push(format!("tracking.{index}.observability.json"));
    }
    // The concurrent confirmation mode writes one array of per-share snapshots
    // rather than a file each; every other capture is a single snapshot.
    let sweeps = run_dir.join(CONFIRM_SNAPSHOTS);
    if sweeps.exists() {
        let raw = std::fs::read(&sweeps).context("reading the confirmation snapshots")?;
        let decoded: Vec<OperationObservability> =
            serde_json::from_slice(&raw).context("decoding the confirmation snapshots")?;
        for (index, snapshot) in decoded.into_iter().enumerate() {
            snapshots.push(CapturedSnapshot {
                source: format!("{CONFIRM_SNAPSHOTS}#{index}"),
                snapshot,
            });
        }
    }

    for name in names {
        let path = run_dir.join(&name);
        if !path.exists() {
            continue;
        }
        let raw = std::fs::read(&path).with_context(|| format!("reading {name}"))?;
        snapshots.push(CapturedSnapshot {
            snapshot: serde_json::from_slice(&raw).with_context(|| format!("decoding {name}"))?,
            source: name,
        });
    }
    Ok(snapshots)
}

/// Tracking invocations a run directory is scanned for.
///
/// A fixed ceiling rather than a directory listing, so the read order is the
/// capture order: metrics place records on one timeline from each snapshot's
/// own anchor, and a shuffled read would still be correct but would make two
/// runs of the same shape print their invocations differently.
const MAX_TRACKING_SNAPSHOTS: usize = 16;

/// The concurrent confirmation mode's per-share snapshots, as one array.
pub const CONFIRM_SNAPSHOTS: &str = "confirm.observability.json";
