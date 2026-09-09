//! Running the worker, bounded.
//!
//! The drive happens in a child process because the provers run on OS threads
//! that are not cancellable and hold the round lock through a cloned `Arc`: a
//! drive that ends early in the parent can leave a thread writing to the
//! sidecar the metrics are about to be read from. In a child, that thread dies
//! with the process.
//!
//! The bound is the benchmark's, not the round's. A wedged run must end with
//! its partial diagnostics on disk rather than occupying a terminal until
//! someone notices, and everything the child wrote before the deadline is
//! already fsynced.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

/// How often a bounded wait checks whether the child has exited.
const POLL: Duration = Duration::from_millis(250);

/// How the child ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildOutcome {
    /// Exited zero. The outcome file is authoritative.
    Completed,
    /// Exited non-zero. Whatever it wrote before that still stands.
    Failed { code: Option<i32> },
    /// Did not exit inside the budget and was killed.
    ///
    /// Distinct from a failure on purpose: a benchmark that ran out of time is
    /// a statement about the budget or the environment, not about the round.
    TimedOut,
}

/// Runs `worker` against `config_path` and waits up to `budget`.
///
/// The child inherits this process's environment, which is how the credentials
/// reach it: nothing secret is written to the configuration file or passed on
/// the command line, where `ps` would expose it.
pub fn run(worker: &Path, config_path: &Path, budget: Duration) -> Result<ChildOutcome> {
    let mut child = Command::new(worker)
        .arg(config_path)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;

    let started = Instant::now();
    loop {
        match child.try_wait()? {
            Some(status) if status.success() => return Ok(ChildOutcome::Completed),
            Some(status) => {
                return Ok(ChildOutcome::Failed {
                    code: status.code(),
                })
            }
            None => {}
        }
        if started.elapsed() >= budget {
            // Killed rather than waited on: the run already exceeded the bound
            // the caller chose, and its durable evidence is on disk.
            let _ = child.kill();
            let _ = child.wait();
            return Ok(ChildOutcome::TimedOut);
        }
        std::thread::sleep(POLL);
    }
}

/// Fails unless the child completed.
///
/// Separated from [`run`] so a caller can read the run directory first: the
/// diagnostics a failed run left are the reason it is worth reading.
pub fn require_completion(outcome: ChildOutcome, budget: Duration) -> Result<()> {
    match outcome {
        ChildOutcome::Completed => Ok(()),
        ChildOutcome::Failed { code: Some(code) } => bail!("the worker exited with status {code}"),
        ChildOutcome::Failed { code: None } => bail!("the worker was terminated by a signal"),
        ChildOutcome::TimedOut => bail!(
            "the worker did not finish inside its {budget:?} budget; raise --budget, or read \
             the run directory to see where it stopped"
        ),
    }
}
