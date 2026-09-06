//! What one child run needs to know, as a value rather than an argument list.
//!
//! The parent writes this to a file and passes the child a single path. Two
//! reasons it is not argv: the list had grown past a dozen positional pairs
//! where a transposed pair silently retargets the run, and argv is world
//! readable through `ps`. Nothing here is secret — paths, URLs, and a round id
//! — and the credentials the child needs reach it only through the environment
//! it inherits, so they are never written to disk or exposed in a process
//! listing.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::stages::CrashStage;

/// Services one run talks to.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Endpoints {
    /// Tendermint RPC, for reading the round the suite provisioned.
    pub chain_rpc: String,
    /// Wallet-facing vote servers, in published order: the submission
    /// lifecycle cycles them by reservation ordinal, so order is behaviour.
    pub vote_servers: Vec<String>,
    pub pir_urls: Vec<String>,
    pub helper_urls: Vec<String>,
    pub lightwalletd: String,
}

/// Which bundle and proposal a run is scoped to.
///
/// A crash stage names one bundle. The driver is also pinned to one bundle at a
/// time for an armed run, because `CrashTransport` sees an HTTP request rather
/// than a bundle index and cannot tell which bundle is posting.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct Target {
    pub bundle_index: u32,
    pub proposal_id: u32,
}

/// What the child is being asked to do.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum RunMode {
    /// Drive until `stage` is reached, then abort the process.
    Armed { stage: CrashStage },
    /// Drive to quiescence without crashing.
    ///
    /// Used both to resume a crashed sidecar and to produce the uncrashed
    /// control a resumed round is compared against; the two differ only in the
    /// sidecar they start from, which is what makes them comparable.
    Unarmed,
}

/// Everything one child run needs.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RoundRunConfig {
    pub sidecar: PathBuf,
    pub wallet_db: PathBuf,
    /// A previous sidecar whose cached PIR proofs are copied in first.
    ///
    /// Padded-slot secrets are copied from the same template, so the synthetic
    /// nullifiers they generate are stable across runs and their proofs cache
    /// like a real note's. A complete template removes the run's live PIR
    /// entirely; an incomplete one leaves exactly the bundles it is missing to
    /// fetch live.
    pub warm_pir_from: Option<PathBuf>,
    pub round_id: String,
    pub account_uuid: String,
    pub endpoints: Endpoints,
    pub target: Target,
    pub mode: RunMode,
    pub crash_log: PathBuf,
    /// Where the child writes [`RunOutcome`] before exiting.
    ///
    /// An armed run that reaches its stage never writes one: it is killed. Its
    /// absence is therefore evidence, not an error.
    pub outcome: PathBuf,
    /// Upper bound on driver dispatches, so a plan that never shrinks ends the
    /// run instead of hanging a test.
    pub max_dispatches: usize,
    /// Unix vote-end the round was provisioned with.
    ///
    /// Share recovery derives its retry and cutoff windows from the distance to
    /// this time, so background tracking cannot classify a share as overdue
    /// without it.
    pub vote_end_time_seconds: u64,
}

impl RoundRunConfig {
    /// The stage this run is armed for, if any.
    pub fn armed_stage(&self) -> Option<CrashStage> {
        match &self.mode {
            RunMode::Armed { stage } => Some(*stage),
            RunMode::Unarmed => None,
        }
    }

    pub fn write(&self, path: &std::path::Path) -> std::io::Result<()> {
        std::fs::write(path, serde_json::to_vec_pretty(self)?)
    }

    pub fn read(path: &std::path::Path) -> std::io::Result<Self> {
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

impl FailureRecord {
    /// Whether this failure is the environment rather than the round.
    ///
    /// Transport failures are the ones staging produces on its own — a stalled
    /// PIR endpoint, an unreachable vote server — and they say nothing about
    /// recovery. Treating them as conformance failures would blame the suite
    /// for its surroundings.
    pub fn is_environmental(&self) -> bool {
        self.kind == "Transport"
    }

    /// Whether the SDK repairs this condition itself, so re-driving resolves it.
    ///
    /// A crash can leave the cached vote-commitment tree stale relative to a
    /// delegation that confirmed. The tree sync detects that, **discards the
    /// cached tree**, and fails the pass — so the next pass re-syncs from
    /// scratch and succeeds. Failing the stage on the first occurrence reports
    /// the SDK's own repair mechanism as a defect, which is exactly what an
    /// earlier version of this matrix did.
    ///
    /// Matched on the message because the SDK exposes no typed marker for it.
    /// Deliberately narrow: a broader rule would retry past real findings.
    pub fn is_self_healing(&self) -> bool {
        self.kind == "InvalidInput"
            && self
                .message
                .contains("does not match its synced vote-tree leaf")
    }
}

/// What an unarmed run ended up doing.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RunOutcome {
    /// Debug rendering of `RoundQuiescence`.
    pub quiescence: String,
    /// Just the variant name, so the parent can match without parsing.
    pub quiescence_kind: String,
    pub failures: Vec<FailureRecord>,
    pub dispatches: usize,
}

impl RunOutcome {
    pub fn write(&self, path: &std::path::Path) -> std::io::Result<()> {
        std::fs::write(path, serde_json::to_vec_pretty(self)?)
    }

    pub fn read(path: &std::path::Path) -> std::io::Result<Self> {
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
    }

    /// Whether the run ended somewhere a conformance test may accept.
    ///
    /// `NoWorkLeft` and `BackgroundShareWorkOnly` are the round finishing.
    /// Everything else either needs the host to act or names a fault.
    pub fn is_terminal_success(&self) -> bool {
        matches!(
            self.quiescence_kind.as_str(),
            "NoWorkLeft" | "BackgroundShareWorkOnly"
        )
    }

    /// Whether only the environment stopped this run.
    pub fn is_environmental(&self) -> bool {
        !self.failures.is_empty() && self.failures.iter().all(FailureRecord::is_environmental)
    }

    /// Whether every failure is one the SDK repairs on the next pass.
    pub fn is_self_healing(&self) -> bool {
        !self.failures.is_empty() && self.failures.iter().all(FailureRecord::is_self_healing)
    }
}
