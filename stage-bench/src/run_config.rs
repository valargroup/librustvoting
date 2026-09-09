//! What one benchmark child run needs to know, as a value rather than an
//! argument list.
//!
//! The parent writes this to a file inside the run directory and passes the
//! child a single path — the same choice `recovery-conformance` makes, for the
//! same two reasons: a dozen positional pairs is a transposition waiting to
//! happen, and argv is world readable through `ps`. Nothing here is secret;
//! the credentials the child needs reach it only through the environment it
//! inherits, so they are never written to disk or exposed in a process listing.
//!
//! The file is also the run's own record of what was measured. `analyze` reads
//! it back out of a finished run directory, so a manifest and its metrics can
//! never describe a different workload than the one that produced them.

use std::path::{Path, PathBuf};

use recovery_conformance::helper_fleet::HelperFleetPlan;
use recovery_conformance::run_config::Endpoints;
use serde::{Deserialize, Serialize};

use crate::ballot::Ballot;

/// Everything one child run needs.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BenchRunConfig {
    /// The voting sidecar this run builds and drives.
    pub sidecar: PathBuf,
    /// The scanned voter wallet note selection reads.
    pub wallet_db: PathBuf,
    /// A previous sidecar whose cached PIR proofs are copied in first.
    ///
    /// Absent means every padded slot and note is fetched from the live PIR
    /// fleet, which staging serves from one synchronous endpoint. Present, the
    /// run measures the phases a warm host actually spends time in.
    pub warm_pir_from: Option<PathBuf>,
    pub round_id: String,
    pub account_uuid: String,
    pub endpoints: Endpoints,
    /// The ballot this round was provisioned with, and votes.
    pub ballot: Ballot,
    /// The synthetic helper fleet, if any. Empty means the real staging primary.
    #[serde(default)]
    pub fleet: HelperFleetPlan,
    /// Unix vote-end the round was provisioned with.
    ///
    /// Share timing derives its retry, overdue, and last-moment windows from
    /// the distance to this time, so a run's window is part of what it measured
    /// rather than an incidental setting.
    pub vote_end_time_seconds: u64,
    /// Bundles the driver advances at once.
    ///
    /// The SDK ships three. Lowering it to one is what a cold-PIR run needs,
    /// because staging serves PIR from a single synchronous endpoint.
    pub bundle_concurrency: usize,
    /// Vote-commitment proofs built at once within a bundle.
    ///
    /// The SDK's `DEFAULT_BATCH_PROOF_CONCURRENCY` is three, capped at fifteen.
    /// A 37-proposal bundle builds 37 of these, so this is the second of the
    /// two serializations a wide ballot pays for.
    pub proof_concurrency: usize,
    /// Upper bound on driver dispatches, so a plan that never shrinks ends the
    /// run instead of hanging the benchmark.
    pub max_dispatches: usize,
    /// Wall-clock ceiling on the confirmation phase, in seconds.
    ///
    /// The benchmark's bound, not the round's: a healthy host confirms across
    /// the whole voting window. When it expires the run reports the tail as
    /// explicitly incomplete rather than pretending the round settled.
    pub tracking_budget_seconds: u64,
    /// Focused confirmations driven at once, or 1 for the shipped tracker.
    ///
    /// Above one this replaces `ShareTrackingDriver` with concurrent
    /// `confirm_pending_share` calls — an experiment measuring what the serial
    /// walk costs, not a measurement of shipped behaviour. See
    /// [`crate::confirm`].
    pub confirm_concurrency: usize,
    /// Detailed records retained per reported invocation.
    ///
    /// A run whose records are capped cannot support a peak-concurrency claim,
    /// so the derived metrics say so rather than reporting a smaller peak.
    pub max_records: usize,
    /// Where the child writes its snapshots, events, and outcome.
    pub run_dir: PathBuf,
}

impl BenchRunConfig {
    /// Path of the run configuration inside a run directory.
    pub fn path_in(run_dir: &Path) -> PathBuf {
        run_dir.join("run-config.json")
    }

    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        std::fs::write(path, serde_json::to_vec_pretty(self)?)
    }

    pub fn read(path: &Path) -> std::io::Result<Self> {
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
    }
}

/// One failed obligation, flattened so the parent can report it without
/// linking the driver's error types.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FailureRecord {
    pub step: Option<String>,
    pub bundle_index: Option<u32>,
    pub kind: String,
    /// Redacted by construction: the SDK bounds and escapes diagnostics before
    /// they reach here, and no payload or key material is copied in.
    pub message: String,
}

/// What one background share-tracking invocation did.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TrackingSummary {
    /// Debug rendering of `ShareTrackingQuiescence`.
    pub quiescence: String,
    pub passes: u32,
    pub confirmed: usize,
    pub resubmitted: usize,
    pub ambiguous: usize,
    pub unrecoverable: usize,
}

/// What one benchmark run ended up doing.
///
/// The authoritative domain result, kept separate from the timing snapshots.
/// A run that measured beautifully and delivered nothing is a failed run, and
/// this is what says so.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BenchOutcome {
    /// Debug rendering of `RoundQuiescence`.
    pub quiescence: String,
    /// Just the variant name, so the parent can match without parsing.
    pub quiescence_kind: String,
    pub failures: Vec<FailureRecord>,
    /// Notes the wallet selected, and the bundles they packed into.
    ///
    /// Observed, not asserted: the benchmark reports the layout it got, because
    /// a wallet rebalance changes the workload rather than invalidating it.
    pub notes: usize,
    pub bundles: u32,
    pub proposals: usize,
    /// Proposals the driver reported complete, out of the ballot.
    pub completed_proposals: usize,
    pub tracking: Vec<TrackingSummary>,
    /// Wall-clock seconds the child spent inside the round driver.
    pub round_drive_seconds: f64,
    /// Wall-clock seconds the child spent in background share tracking.
    pub tracking_seconds: f64,
}

impl BenchOutcome {
    /// Path of the outcome inside a run directory.
    pub fn path_in(run_dir: &Path) -> PathBuf {
        run_dir.join("outcome.json")
    }

    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        std::fs::write(path, serde_json::to_vec_pretty(self)?)
    }

    pub fn read(path: &Path) -> std::io::Result<Self> {
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
    }

    /// Whether the round finished everything the foreground owns.
    ///
    /// `BackgroundShareWorkOnly` counts: a share already accepted by a helper
    /// but not yet visible as confirmed is the host's timer to finish, not a
    /// failure of the drive.
    pub fn is_complete(&self) -> bool {
        self.failures.is_empty()
            && matches!(
                self.quiescence_kind.as_str(),
                "NoWorkLeft" | "BackgroundShareWorkOnly"
            )
    }
}
