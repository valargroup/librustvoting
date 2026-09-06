//! The parent half: run a child to its crash point and prove it got there.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use super::crash::{CrashLog, Observation, EXIT_STAGE_NEVER_REACHED};
use crate::stages::CrashStage;

/// What one crashed child left behind.
pub struct CrashRun {
    /// The sidecar the child was driving, exactly as its last committed
    /// transaction left it.
    pub sidecar: PathBuf,
    /// Everything the child recorded before dying.
    pub observations: Vec<Observation>,
}

impl CrashRun {
    /// The transaction hash staging returned for a POST the wallet never
    /// classified, when the stage recorded one.
    ///
    /// Only `after-broadcast-read` captures this. It is what lets a test ask
    /// the chain about a specific transaction instead of inferring from counts.
    pub fn dispatched_response_body(&self) -> Option<&str> {
        self.observations
            .iter()
            .find_map(|observation| match observation {
                Observation::PostResponse { body, .. } => Some(body.as_str()),
                _ => None,
            })
    }

    /// Whether the child got a POST onto the wire at all.
    pub fn dispatched_a_post(&self) -> bool {
        self.observations
            .iter()
            .any(|observation| matches!(observation, Observation::PostDispatched { .. }))
    }

    /// The plan the child last read, as debug-formatted steps.
    pub fn plan_before_crash(&self) -> Option<&[String]> {
        self.observations
            .iter()
            .rev()
            .find_map(|observation| match observation {
                Observation::PlanBeforeCrash { next_steps } => Some(next_steps.as_slice()),
                _ => None,
            })
    }
}

/// How a child ended.
#[derive(Debug, PartialEq, Eq)]
pub enum CrashOutcome {
    /// Killed by `SIGABRT` at the armed stage. The only outcome a crash test
    /// may proceed from.
    Aborted,
    /// The round finished without ever reaching the stage.
    StageNeverReached,
    /// Anything else: a real failure in the child.
    Failed { code: Option<i32> },
}

/// Drives one round in a child process until it crashes at `stage`.
///
/// Returns an error unless the child died of `SIGABRT` at that stage. A child
/// that completed the round is a harness failure, not a passing test: every
/// assertion about "the state a crash left" would still hold trivially against
/// a round that simply finished, so a silently-missed stage would turn the
/// test into a no-op that never fails again.
pub fn run_until_crash(
    worker: &Path,
    sidecar: &Path,
    round_id: &str,
    stage: CrashStage,
    bundle_index: u32,
    proposal_id: u32,
) -> Result<CrashRun> {
    let log_path = sidecar.with_extension("crashlog.jsonl");
    // Create it up front so a child that dies before its first record still
    // leaves a readable, empty log rather than a missing file.
    CrashLog::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;

    let status = Command::new(worker)
        .arg("--sidecar")
        .arg(sidecar)
        .arg("--round")
        .arg(round_id)
        .arg("--stage")
        .arg(stage.name())
        .arg("--bundle")
        .arg(bundle_index.to_string())
        .arg("--proposal")
        .arg(proposal_id.to_string())
        .arg("--crash-log")
        .arg(&log_path)
        .status()
        .with_context(|| format!("spawning {}", worker.display()))?;

    let observations = CrashLog::read(&log_path).unwrap_or_default();

    match classify(&status) {
        CrashOutcome::Aborted => {}
        CrashOutcome::StageNeverReached => {
            bail!("the round finished without reaching stage {stage}; the test would have asserted against a completed round rather than a crashed one")
        }
        CrashOutcome::Failed { code } => {
            bail!("worker failed before reaching stage {stage} (exit {code:?})")
        }
    }

    if !observations.iter().any(|observation| {
        matches!(observation, Observation::StageReached { stage: reached } if reached == stage.name())
    }) {
        bail!("worker aborted without recording stage {stage}");
    }

    Ok(CrashRun {
        sidecar: sidecar.to_path_buf(),
        observations,
    })
}

#[cfg(unix)]
fn classify(status: &std::process::ExitStatus) -> CrashOutcome {
    use std::os::unix::process::ExitStatusExt;
    if status.signal() == Some(libc::SIGABRT) {
        return CrashOutcome::Aborted;
    }
    match status.code() {
        Some(EXIT_STAGE_NEVER_REACHED) => CrashOutcome::StageNeverReached,
        code => CrashOutcome::Failed { code },
    }
}
