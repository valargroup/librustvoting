//! The live fleet matrix: ten helpers, going up and down around a real round.
//!
//! One test, run serially, for the same reasons the crash matrix is.
//!
//! Skipped unless the environment is present, so `make recovery-conformance`
//! remains runnable on a machine with no staging access; the hermetic tests in
//! the sibling files always run.

#[path = "staging/helper_fleet_matrix.rs"]
mod helper_fleet_matrix;

/// Set to skip the matrix instead of failing when staging is unreachable.
const OPT_OUT: &str = "RECOVERY_CONFORMANCE_WITHOUT_STAGING";

#[test]
fn a_changing_helper_fleet_still_places_every_share_exactly_once() {
    match helper_fleet_matrix::run() {
        helper_fleet_matrix::Run::Skipped(reason) => {
            if std::env::var(OPT_OUT).is_ok() {
                eprintln!("helper fleet conformance skipped: {reason}");
                return;
            }
            panic!(
                "the fleet matrix could not run: {reason}. Set {OPT_OUT} to skip it \
                 deliberately; a matrix that skips itself because its environment is \
                 missing reports a green run having tested nothing"
            );
        }
        helper_fleet_matrix::Run::Completed(report) => {
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
