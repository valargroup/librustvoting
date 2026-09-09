//! The live stall matrix: every class of network request, hung against staging.
//!
//! One test, run serially, for the same reasons the crash matrix is: round
//! creation serialises chain-wide, a delegation is consumed on the vote chain
//! and cannot be replayed, and the exercises would otherwise assert against
//! each other's rounds.
//!
//! Skipped unless the environment is present, so `make recovery-conformance`
//! remains runnable on a machine with no staging access; the hermetic tests in
//! the sibling files always run.

#[path = "staging/stall_matrix.rs"]
mod stall_matrix;

/// Set to skip the matrix instead of failing when staging is unreachable.
const OPT_OUT: &str = "RECOVERY_CONFORMANCE_WITHOUT_STAGING";

#[test]
fn every_hung_request_is_bounded_and_the_round_recovers() {
    match stall_matrix::run() {
        stall_matrix::Run::Skipped(reason) => {
            if std::env::var(OPT_OUT).is_ok() {
                eprintln!("stall conformance skipped: {reason}");
                return;
            }
            panic!(
                "the stall matrix could not run: {reason}. Set {OPT_OUT} to skip it \
                 deliberately; a matrix that skips itself because its environment is \
                 missing reports a green run having tested nothing"
            );
        }
        stall_matrix::Run::Completed(report) => {
            report.print();
            assert!(
                report.attempted > 0,
                "the stall matrix attempted no targets at all"
            );
            assert!(
                !report.passed.is_empty(),
                "the stall matrix passed no target it attempted"
            );
            assert!(
                report.failed.is_empty(),
                "{} stall target(s) failed: {:?}",
                report.failed.len(),
                report.failed
            );
        }
    }
}
