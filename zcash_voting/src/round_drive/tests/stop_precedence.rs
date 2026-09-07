//! What a run reports when nothing is left for it to dispatch.
//!
//! The decision is a pure function of the plan, the failures recorded so far,
//! and the bundles a failure isolated, so it is exercised directly here: the
//! interesting cases are combinations of durable state that are laborious to
//! forge end to end and easy to get wrong in exactly one of them.

use super::fixtures::*;
use crate::round_drive::{quiesce_before_dispatch, RoundQuiescence, Run};
use crate::session::{NextStep, RoundPlan};

/// A real plan for the single-proposal share round, so every field except the
/// ones a case varies holds a value the planner actually produces.
fn share_round_plan() -> RoundPlan {
    let helpers = vec!["http://helper.invalid".to_string()];
    let database = crate::share_tracking::tests::db_with_share(&helpers);
    crate::share::record_delivery(
        &database,
        &crate::share::ShareDeliveryRecordParams {
            round_id: ROUND_ID,
            bundle_index: 0,
            proposal_id: 1,
            share_index: 1,
            submission: &crate::share_tracking::ShareSubmissionReport {
                accepted_urls: helpers.clone(),
                ambiguous_urls: Vec::new(),
                target_count: helpers.len(),
            },
            submit_at: 1_700_000_000,
        },
    )
    .unwrap();
    database
        .conn()
        .execute(
            "UPDATE votes SET tx_hash = 'aa' WHERE round_id = :round_id
               AND wallet_id = :wallet_id AND bundle_index = 0 AND proposal_id = 1",
            rusqlite::named_params! {
                ":round_id": ROUND_ID,
                ":wallet_id": database.wallet_id(),
            },
        )
        .unwrap();
    let plan = executor_over_share_round(std::sync::Arc::new(database))
        .plan()
        .unwrap();
    // The fixture's premise: two shares a helper already accepted, and nothing
    // the foreground has to run.
    assert!(!plan.blocking_share_work);
    assert!(!plan.blocking_recovery);
    assert_eq!(plan.next_steps.len(), 2);
    plan
}

fn run_with(failed_bundles: &[u32]) -> Run {
    let mut run = Run::default();
    for bundle_index in failed_bundles {
        run.record_failure(
            Some(NextStep::Delegate {
                bundle_index: *bundle_index,
            }),
            Some(*bundle_index),
            step_failure("the bundle failed"),
        );
        run.skipped.push(*bundle_index);
    }
    run
}

#[test]
fn shares_a_helper_holds_are_handed_to_background_tracking() {
    let plan = share_round_plan();
    let quiescence = quiesce_before_dispatch(&plan, &Run::default());

    let Some(RoundQuiescence::BackgroundShareWorkOnly { shares }) = quiescence else {
        panic!("a plan of accepted shares is a background handoff: {quiescence:?}");
    };
    assert_eq!(shares.len(), 2);
}

#[test]
fn a_terminal_submission_outranks_shares_the_timer_would_finish() {
    // The regression: `blocking_recovery` is a property of the whole round, so
    // a rejected or hashless submission on one bundle keeps it true while the
    // only steps left are shares another bundle's confirmed vote owes. Reading
    // that flag as "foreground work remains" made the run poll those shares
    // for its entire dispatch budget and then report `PassBudgetExhausted` —
    // an invariant-level event — instead of the rejection the host must act
    // on.
    let mut plan = share_round_plan();
    plan.blocking_recovery = true;

    assert!(
        matches!(
            quiesce_before_dispatch(&plan, &Run::default()),
            Some(RoundQuiescence::PersistedChainTerminal)
        ),
        "the persisted submission is what the host has to handle"
    );
}

#[test]
fn a_terminal_submission_is_reported_for_an_empty_plan_too() {
    let mut plan = share_round_plan();
    plan.next_steps.clear();
    plan.blocking_recovery = true;

    assert!(matches!(
        quiesce_before_dispatch(&plan, &Run::default()),
        Some(RoundQuiescence::PersistedChainTerminal)
    ));
}

#[test]
fn a_recorded_failure_outranks_every_healthy_handoff() {
    let mut plan = share_round_plan();
    plan.blocking_recovery = true;

    assert!(
        matches!(
            quiesce_before_dispatch(&plan, &run_with(&[3])),
            Some(RoundQuiescence::Failures)
        ),
        "reporting the submission would read as 'the round is fine'"
    );
}

#[test]
fn a_skipped_bundles_own_work_does_not_keep_the_run_dispatching() {
    // Selection will never admit a skipped bundle's step, so counting it as
    // foreground work would leave the run polling the healthy bundles'
    // background shares instead of reporting the failure that skipped it.
    let mut plan = share_round_plan();
    plan.next_steps.push(NextStep::Delegate { bundle_index: 3 });
    plan.blocking_recovery = true;

    assert!(matches!(
        quiesce_before_dispatch(&plan, &run_with(&[3])),
        Some(RoundQuiescence::Failures)
    ));
}

#[test]
fn an_unskipped_bundles_work_still_runs() {
    let mut plan = share_round_plan();
    plan.next_steps.push(NextStep::Delegate { bundle_index: 4 });
    plan.blocking_recovery = true;

    assert!(
        quiesce_before_dispatch(&plan, &run_with(&[3])).is_none(),
        "bundle 4 is healthy and its delegation is still owed"
    );
}

#[test]
fn an_undelivered_share_is_foreground_work_even_on_a_skipped_bundle() {
    // `blocking_share_work` is round-wide: a share row no helper has reached
    // is delivered rather than polled, and background tracking cannot finish
    // it, so the round is not ready to be handed over.
    let mut plan = share_round_plan();
    plan.blocking_share_work = true;

    assert!(quiesce_before_dispatch(&plan, &Run::default()).is_none());
}

#[test]
fn an_open_ballot_outranks_the_share_handoff() {
    // Both are states the run cannot advance, but only one of them is
    // something the voter can still act on.
    let mut plan = share_round_plan();
    plan.open_proposals = vec![2];

    let Some(RoundQuiescence::NeedsBallot { open_proposals, .. }) =
        quiesce_before_dispatch(&plan, &Run::default())
    else {
        panic!("an undecided proposal is the host's to resolve");
    };
    assert_eq!(open_proposals, vec![2]);
}

#[test]
fn bundle_setup_outranks_the_ballot_it_blocks() {
    let mut plan = share_round_plan();
    plan.needs_bundle_setup = true;
    plan.open_proposals = vec![2];

    assert!(
        matches!(
            quiesce_before_dispatch(&plan, &Run::default()),
            Some(RoundQuiescence::NeedsBundleSetup)
        ),
        "no vote work can be planned until the bundle rows exist"
    );
}

#[test]
fn an_empty_plan_with_nothing_owed_is_no_work_left() {
    let mut plan = share_round_plan();
    plan.next_steps.clear();

    assert!(matches!(
        quiesce_before_dispatch(&plan, &Run::default()),
        Some(RoundQuiescence::NoWorkLeft)
    ));
}
