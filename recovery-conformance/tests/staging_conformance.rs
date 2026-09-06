//! The live conformance matrix: every crash stage, against staging.
//!
//! One test, run serially, because the resources it uses cannot be shared
//! safely. Round creation serialises chain-wide, a delegation is consumed on
//! the vote chain and cannot be replayed, and several stages are alternative
//! boundaries around the same POST rather than points on one timeline. Running
//! them as independent parallel tests would either deadlock on the chain's
//! single pending ceremony or assert against each other's rounds.
//!
//! Skipped unless the environment is present, so `make recovery-conformance`
//! remains runnable on a machine with no staging access; the hermetic tests in
//! the sibling files always run.

#[path = "staging/matrix.rs"]
mod matrix;

#[test]
fn every_crash_stage_is_reached_and_recovers() {
    match matrix::run() {
        matrix::Run::Skipped(reason) => {
            eprintln!("skipping the staging matrix: {reason}");
        }
        matrix::Run::Completed(report) => {
            report.print();
            assert!(
                report.failed.is_empty(),
                "{} of {} stages failed conformance",
                report.failed.len(),
                report.attempted
            );
        }
    }
}
