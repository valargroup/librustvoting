//! The child process: drive one round, and die where told.
//!
//! Deliberately thin. Everything it does lives in
//! [`round_run`](recovery_conformance::round_run) so the armed run, the resumed
//! run, and the uncrashed control share one implementation — a control built by
//! separate code would not be a control.
//!
//! Every drive runs here rather than in the parent because the provers use
//! dedicated OS threads that are not cancellable and hold the round lock
//! through a cloned `Arc`. In the parent, one of those could outlive a drive
//! and keep writing to the sidecar a test was about to read.
//!
//! Exit codes are the parent's only signal for an armed run, since a run that
//! reaches its stage is killed before it can report anything.

use recovery_conformance::child::{EXIT_INFRASTRUCTURE_FAILURE, EXIT_STAGE_NEVER_REACHED};
use recovery_conformance::round_run::drive_round;
use recovery_conformance::run_config::RoundRunConfig;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: recovery-conformance-worker <run-config.json>");
        std::process::exit(2);
    };
    let config = match RoundRunConfig::read(std::path::Path::new(&path)) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("worker: unreadable run config {path}: {error}");
            std::process::exit(2);
        }
    };

    // The staging chain is asserted here as well as in the parent: this binary
    // can be run by hand, and the one thing it does on purpose is kill itself
    // mid-broadcast.
    recovery_conformance::environment::assert_targets_staging(
        recovery_conformance::environment::STAGING_CHAIN_ID,
    );

    match drive_round(&config).await {
        Ok(outcome) => {
            eprintln!("worker: ended at {}", outcome.quiescence);
            for failure in &outcome.failures {
                eprintln!(
                    "worker: FAILURE step={:?} bundle={:?} kind={} message={}",
                    failure.step, failure.bundle_index, failure.kind, failure.message
                );
            }
            if config.armed_stage().is_some() {
                // Returning from an armed run means the stage never fired. Its
                // cause decides who is to blame: a run the environment ended
                // never got far enough to find out whether the seam works, so
                // it is retried rather than reported as a broken seam.
                if outcome.is_environmental() {
                    std::process::exit(EXIT_INFRASTRUCTURE_FAILURE);
                }
                std::process::exit(EXIT_STAGE_NEVER_REACHED);
            }
            // An unarmed run reports through its outcome file; a non-terminal
            // quiescence is the parent's to judge, not this process's.
        }
        Err(error) => {
            eprintln!("worker: {error:#}");
            std::process::exit(EXIT_INFRASTRUCTURE_FAILURE);
        }
    }
}
