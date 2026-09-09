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

/// Set to skip the matrix instead of failing when staging is unreachable.
///
/// Absence is the default and it is strict: a run that cannot reach staging
/// fails. Anything softer makes "nothing ran" indistinguishable from "nothing
/// was wrong", which is the failure this whole file exists to prevent.
const OPT_OUT: &str = "RECOVERY_CONFORMANCE_WITHOUT_STAGING";

#[test]
fn every_crash_stage_is_reached_and_recovers() {
    match matrix::run() {
        // Setup could not complete. This used to log and pass, which meant
        // removing credentials turned the whole matrix into a silent no-op that
        // still reported success. Skipping is now something a caller asks for
        // explicitly, never something a missing environment decides.
        matrix::Run::Skipped(reason) => {
            assert!(
                std::env::var_os(OPT_OUT).is_some(),
                "the staging matrix could not run ({reason}). Set {OPT_OUT}=1 to                  skip it deliberately; without that a matrix that runs nothing                  is a failure, not a pass"
            );
            eprintln!("skipping the staging matrix deliberately: {reason}");
        }
        matrix::Run::Completed(report) => {
            report.print();
            recovery_conformance::matrix_coverage::MatrixCoverage {
                attempted: report.attempted,
                passed: report.passed.len(),
                failed: report.failed.len(),
                skipped: report.skipped.len(),
                excluded: 0,
            }
            .validate()
            .expect("incomplete live conformance coverage");
        }
    }
}
