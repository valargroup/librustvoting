//! The parent half: run a child to a crash point, or to quiescence, and prove
//! which of those happened.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::crash::{CrashLog, Observation, EXIT_INFRASTRUCTURE_FAILURE, EXIT_STAGE_NEVER_REACHED};
use crate::run_config::{RoundRunConfig, RunOutcome};
use crate::stages::CrashStage;

/// How many times an environment failure is retried.
///
/// Staging publishes a single PIR endpoint whose query path intermittently
/// stalls until the client deadline expires. Every retry preserves and
/// re-enters the same sidecar: not reaching the requested crash stage does not
/// prove that other bundles made no durable or on-chain progress.
const INFRASTRUCTURE_ATTEMPTS: usize = 6;

/// What one crashed child left behind.
pub struct CrashRun {
    pub sidecar: PathBuf,
    pub observations: Vec<Observation>,
}

impl CrashRun {
    /// The response body staging returned for a POST the wallet never
    /// classified, when the stage captured one.
    ///
    /// Only the `after-*-read` stages record this. It is what lets a test ask
    /// the chain about a specific transaction rather than inferring identity
    /// from counts.
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

    /// The plan the child last read before dying.
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
    /// Killed by `SIGABRT` at the armed stage.
    Aborted,
    /// The run finished without the stage firing.
    StageNeverReached,
    /// The environment ended the run before any stage could fire.
    InfrastructureFailure,
    /// Exited normally; an unarmed run's ordinary ending.
    Completed,
    Failed {
        code: Option<i32>,
    },
}

/// Drives a child to quiescence without arming a crash.
///
/// Used both to resume a crashed sidecar and to build the uncrashed control.
pub fn run_to_quiescence(worker: &Path, config: &RoundRunConfig) -> Result<RunOutcome> {
    anyhow::ensure!(
        config.armed_stage().is_none(),
        "run_to_quiescence was given an armed configuration"
    );

    let mut last = String::new();
    for attempt in 1..=INFRASTRUCTURE_ATTEMPTS {
        // The sidecar is never deleted between attempts, and never needs to be:
        // resuming is what a host does after any interruption, and the
        // lifecycle is built to be re-entered. Discarding it would be the
        // unsafe move, because after a POST may have been dispatched the row is
        // the only record that a transaction might exist.
        let status = spawn(worker, config)?;
        match classify(&status) {
            CrashOutcome::Completed => {
                let outcome = RunOutcome::read(&config.outcome)
                    .with_context(|| format!("reading {}", config.outcome.display()))?;
                // A run can exit cleanly having stopped on an environment
                // failure; that is still worth another pass.
                // A stalled chain recovery is not a verdict. The specification
                // distinguishes it from `ChainTerminal` precisely because
                // "running again later may still resolve it": the transaction
                // may not be mined yet, or the tree may not have advanced far
                // enough for the exact-tree scan to find it. Waiting and
                // re-driving is what a host does, so the matrix does it too
                // rather than reporting the SDK's own retry advice as a
                // failure.
                if outcome.quiescence_kind == "ChainRecoveryStalled"
                    || (outcome.quiescence_kind == "TargetRecoveryPending"
                        && outcome.failures.is_empty())
                {
                    last = outcome.quiescence.clone();
                    eprintln!(
                        "  resume attempt {attempt}/{INFRASTRUCTURE_ATTEMPTS}: chain recovery \
                         stalled; waiting for the chain to advance"
                    );
                    std::thread::sleep(CHAIN_ADVANCE_WAIT);
                    continue;
                }
                // The SDK discards the cached vote tree when it finds it stale
                // against a confirmed delegation, so the very next pass
                // re-syncs and proceeds. Re-driving is the designed recovery,
                // not a workaround.
                if outcome.is_self_healing() && !outcome.is_terminal_success() {
                    last = describe_environment_failure(config);
                    eprintln!(
                        "  resume attempt {attempt}/{INFRASTRUCTURE_ATTEMPTS}: stale vote-tree \
                         cache discarded by the SDK; re-driving"
                    );
                    continue;
                }
                if outcome.is_environmental() && !outcome.is_terminal_success() {
                    last = describe_environment_failure(config);
                    eprintln!("  resume attempt {attempt}/{INFRASTRUCTURE_ATTEMPTS}: {last}");
                    continue;
                }
                if outcome.needs_background_recovery() {
                    last = "background share tracking exhausted the suite time budget".into();
                    eprintln!(
                        "  resume attempt {attempt}/{INFRASTRUCTURE_ATTEMPTS}: {last}; \
                         reopening the same sidecar for remaining confirmations"
                    );
                    continue;
                }
                if outcome.needs_helper_recovery() {
                    // A confirmed combined unit can finish one member's helper
                    // delivery incompletely and leave later members untouched.
                    // Background tracking settles existing rows; reopening the
                    // driver is what executes those remaining obligations.
                    last = format!("incomplete helper delivery: {:?}", outcome.failures);
                    eprintln!(
                        "  resume attempt {attempt}/{INFRASTRUCTURE_ATTEMPTS}: helper delivery \
                         incomplete; reopening the same sidecar for remaining obligations"
                    );
                    continue;
                }
                return Ok(outcome);
            }
            CrashOutcome::InfrastructureFailure => {
                last = describe_environment_failure(config);
                eprintln!("  resume attempt {attempt}/{INFRASTRUCTURE_ATTEMPTS}: {last}");
            }
            other => anyhow::bail!("unarmed run ended unexpectedly: {other:?}"),
        }
    }
    // Exhausting every attempt on the *same* non-transport error is a finding,
    // not an environment problem. Saying so in the message matters: the caller
    // reports a failed resume as a skip, and a real defect filed under "the
    // environment was flaky" is a defect nobody looks at.
    anyhow::bail!(
        "resume did not converge after {INFRASTRUCTURE_ATTEMPTS} attempts, all ending the \
         same way. This is a conformance failure rather than an environment one unless the \
         message below is a transport error. Last: {last}"
    )
}

/// Drives a child until it crashes at its armed stage, retrying runs the
/// environment ended before any stage could fire.
///
/// A child that completed the round is a failure, not a pass: every assertion
/// about "the state a crash left" would hold trivially against a finished
/// round, so a seam that stopped firing would turn the test into a no-op.
pub fn run_until_crash(worker: &Path, config: &RoundRunConfig) -> Result<CrashRun> {
    let stage = config
        .armed_stage()
        .context("run_until_crash was given an unarmed configuration")?;

    let mut last = None;
    for attempt in 1..=INFRASTRUCTURE_ATTEMPTS {
        match attempt_crash(worker, config, stage) {
            Ok(run) => return Ok(run),
            Err(AttemptFailure::Infrastructure(reason)) => {
                eprintln!("  attempt {attempt}/{INFRASTRUCTURE_ATTEMPTS} for {stage}: {reason}");
                last = Some(reason);
                // Continue from the durable state this attempt left. Another
                // bundle may have reserved or dispatched a submission before
                // the target bundle hit an environmental failure, and deleting
                // the sidecar would discard the only recovery evidence.
            }
            Err(AttemptFailure::Fatal(error)) => return Err(error),
        }
    }
    Err(anyhow::anyhow!(
        "staging ended {INFRASTRUCTURE_ATTEMPTS} runs before stage {stage} could fire; last: {}",
        last.unwrap_or_else(|| "unknown".to_string())
    ))
}

enum AttemptFailure {
    Infrastructure(String),
    Fatal(anyhow::Error),
}

fn attempt_crash(
    worker: &Path,
    config: &RoundRunConfig,
    stage: CrashStage,
) -> Result<CrashRun, AttemptFailure> {
    // Created up front so a child that dies before its first record still
    // leaves a readable, empty log rather than a missing file.
    CrashLog::create(&config.crash_log)
        .with_context(|| format!("creating {}", config.crash_log.display()))
        .map_err(AttemptFailure::Fatal)?;

    let status = spawn(worker, config).map_err(AttemptFailure::Fatal)?;
    let observations = CrashLog::read(&config.crash_log).unwrap_or_default();

    match classify(&status) {
        CrashOutcome::Aborted => {}
        CrashOutcome::InfrastructureFailure => {
            return Err(AttemptFailure::Infrastructure(
                describe_environment_failure(config),
            ))
        }
        CrashOutcome::StageNeverReached | CrashOutcome::Completed => {
            return Err(AttemptFailure::Fatal(anyhow::anyhow!(
                "stage {stage} was never reached; the round finished without its crash; the test would have \
                 asserted against a completed round rather than a crashed one"
            )))
        }
        CrashOutcome::Failed { code } => {
            return Err(AttemptFailure::Fatal(anyhow::anyhow!(
                "worker failed before reaching stage {stage} (exit {code:?})"
            )))
        }
    }

    if !observations.iter().any(|observation| {
        matches!(observation, Observation::StageReached { stage: reached } if reached == stage.name())
    }) {
        return Err(AttemptFailure::Fatal(anyhow::anyhow!(
            "worker aborted without recording stage {stage}; the abort must follow an \
             fsynced observation or the crash point cannot be proven"
        )));
    }

    Ok(CrashRun {
        sidecar: config.sidecar.clone(),
        observations,
    })
}

/// What one run armed with a stall did.
pub struct StalledRun {
    pub observations: Vec<Observation>,
    /// How long the child ran before it ended or was killed.
    pub elapsed: Duration,
    /// Whether the child ended on its own rather than being killed at `budget`.
    ///
    /// The finding this axis exists for. A child killed at its budget is a
    /// child whose hung request had no effective deadline, and no restart
    /// repairs that, because nothing crashed.
    pub ended_itself: bool,
    /// The outcome the child wrote, when it lived long enough to write one.
    pub outcome: Option<RunOutcome>,
}

/// Drives a child whose armed request never answers, bounded by `budget`.
///
/// Unlike [`run_until_crash`] this does not retry: an environment failure and a
/// deadline that was never applied both end a run early, and repeating the run
/// would only make the two harder to tell apart. The caller judges from
/// `ended_itself` and the recorded stalls.
///
/// The budget must be well below [`CHILD_BUDGET`] or the finding is unreachable:
/// a wedged run would be killed by the outer bound and reported as one more
/// slow stage rather than as an unbounded request.
pub fn run_until_the_stall_resolves(
    worker: &Path,
    config: &RoundRunConfig,
    budget: Duration,
) -> Result<StalledRun> {
    // Created up front so a child that hangs before its first record still
    // leaves a readable log rather than a missing file.
    CrashLog::create(&config.crash_log)
        .with_context(|| format!("creating {}", config.crash_log.display()))?;

    let started = Instant::now();
    let status = spawn_bounded(worker, config, budget)?;
    let elapsed = started.elapsed();
    let observations = CrashLog::read(&config.crash_log).unwrap_or_default();
    // Read even when the child was killed: a run that hung on its last request
    // may still have written an outcome for the work it completed first.
    let outcome = RunOutcome::read(&config.outcome).ok();

    Ok(StalledRun {
        observations,
        elapsed,
        ended_itself: status.is_some(),
        outcome,
    })
}

/// A short, secret-free description of why the environment ended a run.
fn describe_environment_failure(config: &RoundRunConfig) -> String {
    let Ok(outcome) = RunOutcome::read(&config.outcome) else {
        return "the run ended before it could report a reason".to_string();
    };
    outcome
        .failures
        .first()
        .map(|failure| format!("{}: {}", failure.kind, failure.message))
        .unwrap_or_else(|| outcome.quiescence.clone())
}

/// Longest one child may run before it is killed.
///
/// A drive to quiescence proves three delegations and up to nine votes, and a
/// vote proof takes minutes, so this is generous. It exists because
/// `Command::status` waits forever: without it a wedged child — a stalled
/// endpoint holding a connection open, a prover that never finishes — hangs the
/// whole matrix rather than failing one stage, and no outer budget can help
/// because the parent is blocked inside the wait.
const CHILD_BUDGET: Duration = Duration::from_secs(20 * 60);

/// How often the parent checks whether a child has finished.
const CHILD_POLL: Duration = Duration::from_millis(250);

/// How long to wait before re-driving a submission whose recovery stalled.
///
/// Long enough for a block or two: what a stalled exact-tree scan is usually
/// waiting for is the transaction being mined and the commitment tree
/// advancing past it.
const CHAIN_ADVANCE_WAIT: Duration = Duration::from_secs(45);

fn spawn(worker: &Path, config: &RoundRunConfig) -> Result<std::process::ExitStatus> {
    spawn_bounded(worker, config, CHILD_BUDGET)?.ok_or_else(|| {
        anyhow::anyhow!(
            "the worker exceeded its {CHILD_BUDGET:?} budget and was killed; stage={:?} \
             bundle={} proposal={}",
            config.armed_stage(),
            config.target.bundle_index,
            config.target.proposal_id
        )
    })
}

/// Spawns a child and waits at most `budget` for it.
///
/// `None` means the budget expired and the child was killed. That is an error
/// for every ordinary run — hence [`spawn`] above — but it is the *finding* a
/// stall exercise is looking for, so the two are separated rather than the
/// budget being a constant one caller has to work around.
fn spawn_bounded(
    worker: &Path,
    config: &RoundRunConfig,
    budget: Duration,
) -> Result<Option<std::process::ExitStatus>> {
    // Preserve prior attempts instead of losing the evidence when the child
    // creates its next log and outcome. Only public diagnostics are copied.
    static ATTEMPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let attempt = ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    for artifact in [&config.crash_log, &config.outcome] {
        if artifact.exists() {
            let archive =
                artifact.with_extension(format!("attempt-{}-{attempt}", std::process::id()));
            std::fs::copy(artifact, archive).context("preserving previous worker evidence")?;
        }
    }
    let config_path = config.sidecar.with_extension("run.json");
    config
        .write(&config_path)
        .with_context(|| format!("writing {}", config_path.display()))?;
    // The child inherits this process's environment, which is how the voter
    // seed reaches it. It is never written into the config file above nor
    // passed as an argument.
    let mut command = Command::new(worker);
    command.arg(&config_path);
    if config.mode == crate::run_config::RunMode::RecoverCombined {
        command.env_remove(crate::environment::VOTER_SEED_VAR);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("spawning {}", worker.display()))?;

    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait().context("waiting for the worker")? {
            Some(status) => return Ok(Some(status)),
            None if Instant::now() >= deadline => {
                // Killed rather than left running: a detached worker keeps
                // holding the sidecar and its round lock, and the next attempt
                // would race it. The sidecar remains the authority for the
                // next retry even when this worker reached no crash stage.
                let _ = child.kill();
                let _ = child.wait();
                return Ok(None);
            }
            None => std::thread::sleep(CHILD_POLL),
        }
    }
}

#[cfg(unix)]
fn classify(status: &std::process::ExitStatus) -> CrashOutcome {
    use std::os::unix::process::ExitStatusExt;
    if status.signal() == Some(libc::SIGABRT) {
        return CrashOutcome::Aborted;
    }
    match status.code() {
        Some(0) => CrashOutcome::Completed,
        Some(EXIT_STAGE_NEVER_REACHED) => CrashOutcome::StageNeverReached,
        Some(EXIT_INFRASTRUCTURE_FAILURE) => CrashOutcome::InfrastructureFailure,
        code => CrashOutcome::Failed { code },
    }
}
