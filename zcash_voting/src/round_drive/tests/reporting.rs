//! What the run aggregate keeps from a step that failed.

use super::fixtures::*;
use crate::round_drive::{run_ledger::Run, RoundQuiescence};
use crate::session::NextStep;
use crate::{ChainSubmissionPending, ChainSubmissionResult};

/// A submission the step observed as still tracking under a candidate hash.
fn tracking_outcome() -> ChainSubmissionResult {
    ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking {
        candidate_transaction_hash: crate::CandidateTransactionHash::from_bytes([0x11; 32]),
    })
}

#[test]
fn a_chain_outcome_survives_a_failure_that_followed_it() {
    // A step can reach the chain and then fail on the helper work that
    // follows. What it saw on the chain is a durable effect the run observed,
    // so it belongs in the aggregate just as a successful step's does; keeping
    // it only inside the failure left `chain_outcomes` and its wire projection
    // describing less than the run actually saw.
    let step = NextStep::AdvanceVote {
        bundle_index: 0,
        proposal_id: 1,
    };
    let mut failure = step_failure("helper delivery failed after confirmation");
    failure.chain_outcome = Some(tracking_outcome());

    let mut run = Run::default();
    run.record_failure(Some(step.clone()), Some(0), failure);
    let report = run.finish(RoundQuiescence::Failures);

    assert_eq!(report.chain_outcomes.len(), 1);
    assert_eq!(report.chain_outcomes[0].0, step);
    assert_eq!(report.chain_outcomes[0].1, tracking_outcome());
    assert_eq!(report.failures.len(), 1, "the failure is still reported");
}

#[test]
fn a_failure_with_no_chain_outcome_adds_nothing() {
    let mut run = Run::default();
    run.record_failure(
        Some(NextStep::Delegate { bundle_index: 0 }),
        Some(0),
        step_failure("the proof failed"),
    );
    let report = run.finish(RoundQuiescence::Failures);

    assert!(report.chain_outcomes.is_empty());
}

#[test]
fn a_plan_failure_names_no_step_and_no_chain_outcome() {
    // A plan that could not be read belongs to no step, so there is nothing to
    // attribute a chain outcome to even if one were somehow attached.
    let mut run = Run::default();
    let mut failure = step_failure("the plan could not be read");
    failure.chain_outcome = Some(tracking_outcome());
    run.record_failure(None, None, failure);
    let report = run.finish(RoundQuiescence::Failures);

    assert!(report.chain_outcomes.is_empty());
    assert!(report.failures[0].bundle_index.is_none());
}

#[test]
fn a_signed_delegation_survives_the_failure_that_followed_it() {
    // A `Delegate` step can prove and sign and then lose the chain. The bundle
    // is durable and the run produced it, so keeping it only inside the
    // failure left `delegations` and its wire projection describing less than
    // the run had done — the same contract `share_deliveries` already meets.
    let step = NextStep::Delegate { bundle_index: 0 };
    let mut failure = step_failure("the chain refused the signed bundle");
    failure.delegation = Some(signed_delegation());

    let mut run = Run::default();
    run.record_failure(Some(step), Some(0), failure);
    let report = run.finish(RoundQuiescence::Failures);

    assert_eq!(report.delegations.len(), 1);
    assert_eq!(report.delegations[0].bundle_index, 0);
    assert_eq!(report.failures.len(), 1, "the failure is still reported");
}
