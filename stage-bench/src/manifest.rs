//! What a run was, recorded beside what it measured.
//!
//! A timing table is meaningless without the workload that produced it, and a
//! benchmark whose conditions live only in a shell history cannot be compared
//! to itself a week later. Every field here is something that changes the
//! numbers: the ballot, the fleet, the concurrency, the vote window the SDK
//! derives its share schedule from, and the build the SDK was compiled into.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ballot::ProposalSource;
use crate::run_config::{BenchOutcome, BenchRunConfig};

/// The conditions of one run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub round_id: String,
    pub run_dir: PathBuf,
    /// UTC, seconds since the epoch, for the parent's own span.
    pub started_at_unix: u64,
    pub finished_at_unix: u64,
    /// Git revision of the workspace the binaries were built from.
    pub commit: String,
    /// `debug` or `release`. A debug build's proving times measure the compiler.
    pub profile: String,
    /// The crypto backend feature the SDK was built with.
    pub backend: String,

    pub proposals: usize,
    pub bundles: u32,
    pub notes: usize,
    pub ballot: Vec<ProposalSource>,
    /// Helpers named to the SDK, including any that are synthetic.
    pub configured_helpers: usize,
    /// Whether the fleet was synthetic names routed onto one real backend.
    pub synthetic_fleet: bool,
    pub bundle_concurrency: usize,
    /// Vote-commitment proofs built at once within a bundle.
    #[serde(default)]
    pub proof_concurrency: usize,
    /// Milliseconds between chain-submission polls.
    #[serde(default)]
    pub chain_repoll_milliseconds: u64,
    /// Seconds the confirmation phase was allowed.
    ///
    /// Defaulted on read so a directory archived before this field existed still
    /// analyses. A run directory outlives the code that wrote it, and refusing
    /// to read an older one would throw away the measurement it holds.
    #[serde(default)]
    pub tracking_budget_seconds: u64,
    /// Focused confirmations driven at once. Above 1 the confirmation numbers
    /// describe an experiment, not the shipped tracker.
    ///
    /// Defaulted on read, and defaulting to zero rather than one: an older
    /// manifest cannot say which mode ran, and zero is visibly not a mode.
    #[serde(default)]
    pub confirm_concurrency: usize,
    pub max_dispatches: usize,
    pub max_records: usize,
    /// Seconds between provisioning and the round's vote end.
    ///
    /// Not cosmetic: the SDK's last-moment window is a fraction of the round,
    /// so two runs with different windows ran different share schedules and
    /// their delivery numbers are not comparable.
    pub vote_window_seconds: u64,
    pub warm_pir: bool,

    pub vote_servers: Vec<String>,
    pub pir_urls: Vec<String>,
    pub lightwalletd: String,

    pub quiescence: String,
    pub quiescence_kind: String,
    pub completed_proposals: usize,
    pub failures: usize,
    pub round_drive_seconds: f64,
    pub tracking_seconds: f64,
}

impl Manifest {
    /// Name of the manifest inside a run directory.
    pub const FILE_NAME: &'static str = "manifest.json";

    /// Builds the manifest from the run's own configuration and result.
    pub fn build(
        config: &BenchRunConfig,
        outcome: &BenchOutcome,
        started_at_unix: u64,
        vote_window_seconds: u64,
    ) -> Self {
        Self {
            round_id: config.round_id.clone(),
            run_dir: config.run_dir.clone(),
            started_at_unix,
            finished_at_unix: now_unix(),
            commit: commit(),
            profile: profile().to_string(),
            backend: backend().to_string(),
            proposals: outcome.proposals,
            bundles: outcome.bundles,
            notes: outcome.notes,
            ballot: config.ballot.sources().to_vec(),
            configured_helpers: config.endpoints.helper_urls.len(),
            synthetic_fleet: !config.fleet.configured_urls().is_empty(),
            bundle_concurrency: config.bundle_concurrency,
            proof_concurrency: config.proof_concurrency,
            chain_repoll_milliseconds: config.chain_repoll_milliseconds,
            tracking_budget_seconds: config.tracking_budget_seconds,
            confirm_concurrency: config.confirm_concurrency,
            max_dispatches: config.max_dispatches,
            max_records: config.max_records,
            vote_window_seconds,
            warm_pir: config.warm_pir_from.is_some(),
            vote_servers: config.endpoints.vote_servers.clone(),
            pir_urls: config.endpoints.pir_urls.clone(),
            lightwalletd: config.endpoints.lightwalletd.clone(),
            quiescence: outcome.quiescence.clone(),
            quiescence_kind: outcome.quiescence_kind.clone(),
            completed_proposals: outcome.completed_proposals,
            failures: outcome.failures.len(),
            round_drive_seconds: outcome.round_drive_seconds,
            tracking_seconds: outcome.tracking_seconds,
        }
    }

    pub fn write(&self, run_dir: &Path) -> std::io::Result<()> {
        std::fs::write(
            run_dir.join(Self::FILE_NAME),
            serde_json::to_vec_pretty(self)?,
        )
    }

    pub fn read(run_dir: &Path) -> std::io::Result<Self> {
        Ok(serde_json::from_slice(&std::fs::read(
            run_dir.join(Self::FILE_NAME),
        )?)?)
    }
}

/// The workspace revision, or `unknown` outside a git checkout.
///
/// Read at run time rather than baked in at compile time: the benchmark is
/// rebuilt far less often than it is run, and a stale constant would attribute
/// today's numbers to last week's commit.
fn commit() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|revision| revision.trim().to_string())
        .filter(|revision| !revision.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Which build profile this binary was compiled into.
///
/// Recorded because it dominates every proving measurement: a debug ZKP2 proof
/// takes minutes where a release proof takes seconds, so a debug run's phase
/// table describes the compiler rather than the SDK.
fn profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

fn backend() -> &'static str {
    if cfg!(feature = "lrz") {
        "lrz"
    } else {
        "zakura"
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}
