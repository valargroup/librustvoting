//! What a run reports when nothing is left for it to dispatch.
//!
//! The decision is a pure function of the classified plan, the failures
//! recorded so far, and the bundles a failure isolated, so it is exercised
//! directly here: the interesting cases are combinations of durable state that
//! are laborious to forge end to end and easy to get wrong in exactly one of
//! them.

use super::fixtures::*;
use crate::round_drive::{quiescence::quiesce_before_dispatch, run_ledger::Run, RoundQuiescence};
use crate::round_planning::{ClassifiedPlan, Obligation};
use crate::session::NextStep;

/// A real classified plan for the single-proposal share round, so every field
/// except the ones a case varies holds a value the planner actually produces,
/// and the share obligations carry the acceptance the decision reads.
fn share_round_plan() -> ClassifiedPlan {
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
                local_capacity_exhausted: false,
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
    let classified = executor_over_share_round(std::sync::Arc::new(database))
        .plan_classified()
        .unwrap();
    // The fixture's premise: two shares a helper already accepted, and nothing
    // the foreground has to run.
    assert!(!classified.plan.blocking_share_work);
    assert!(!classified.plan.blocking_recovery);
    assert_eq!(classified.plan.next_steps.len(), 2);
    classified
}

/// Applies the stop decision to a classified plan a case has adjusted.
fn stop(classified: &ClassifiedPlan, run: &Run) -> Option<RoundQuiescence> {
    quiesce_before_dispatch(&classified.plan, &classified.obligations.obligations, run)
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
    let quiescence = stop(&plan, &Run::default());

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
    plan.plan.blocking_recovery = true;

    assert!(
        matches!(
            stop(&plan, &Run::default()),
            Some(RoundQuiescence::PersistedChainTerminal)
        ),
        "the persisted submission is what the host has to handle"
    );
}

#[test]
fn a_terminal_submission_is_reported_for_an_empty_plan_too() {
    let mut plan = share_round_plan();
    plan.plan.next_steps.clear();
    plan.plan.blocking_recovery = true;

    assert!(matches!(
        stop(&plan, &Run::default()),
        Some(RoundQuiescence::PersistedChainTerminal)
    ));
}

#[test]
fn a_recorded_failure_outranks_every_healthy_handoff() {
    let mut plan = share_round_plan();
    plan.plan.blocking_recovery = true;

    assert!(
        matches!(
            stop(&plan, &run_with(&[3])),
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
    plan.plan
        .next_steps
        .push(NextStep::Delegate { bundle_index: 3 });
    plan.plan.blocking_recovery = true;

    assert!(matches!(
        stop(&plan, &run_with(&[3])),
        Some(RoundQuiescence::Failures)
    ));
}

#[test]
fn an_unskipped_bundles_work_still_runs() {
    let mut plan = share_round_plan();
    plan.plan
        .next_steps
        .push(NextStep::Delegate { bundle_index: 4 });
    plan.plan.blocking_recovery = true;

    assert!(
        stop(&plan, &run_with(&[3])).is_none(),
        "bundle 4 is healthy and its delegation is still owed"
    );
}

/// Marks the obligation for one share as reached by no helper, so it is
/// delivered rather than polled.
fn make_undelivered(classified: &mut ClassifiedPlan, share_index: u32) {
    let mut found = false;
    for obligation in &mut classified.obligations.obligations {
        if let Obligation::Confirm {
            share_index: index,
            accepted,
            ..
        } = obligation
        {
            if *index == share_index {
                *accepted = false;
                found = true;
            }
        }
    }
    assert!(found, "the fixture owes share {share_index}");
}

#[test]
fn an_undelivered_share_is_foreground_work() {
    // A share row no helper has reached cannot be finished by polling, so the
    // run delivers it rather than handing the round to background tracking.
    let mut plan = share_round_plan();
    make_undelivered(&mut plan, 0);

    assert!(stop(&plan, &Run::default()).is_none());
}

#[test]
fn an_undelivered_share_on_a_skipped_bundle_does_not_hold_the_run_open() {
    // The round-wide `blocking_share_work` stayed true for a share this run
    // will never touch, so the healthy bundle's already-accepted shares were
    // polled in the foreground until the dispatch budget ran out — reporting
    // `PassBudgetExhausted` in place of the failure that skipped the bundle.
    let mut plan = share_round_plan();
    plan.plan.next_steps.push(NextStep::ConfirmShare {
        bundle_index: 3,
        proposal_id: 1,
        share_index: 9,
    });
    assert!(
        !plan.plan.blocking_share_work,
        "the skipped bundle's share is the only undelivered one"
    );

    assert!(
        matches!(
            stop(&plan, &run_with(&[3])),
            Some(RoundQuiescence::Failures)
        ),
        "the skipped bundle's own share is not this run's work"
    );
}

#[test]
fn an_open_ballot_outranks_the_share_handoff() {
    // Both are states the run cannot advance, but only one of them is
    // something the voter can still act on.
    let mut plan = share_round_plan();
    plan.plan.open_proposals = vec![2];

    let Some(RoundQuiescence::NeedsBallot { open_proposals, .. }) = stop(&plan, &Run::default())
    else {
        panic!("an undecided proposal is the host's to resolve");
    };
    assert_eq!(open_proposals, vec![2]);
}

#[test]
fn bundle_setup_outranks_the_ballot_it_blocks() {
    let mut plan = share_round_plan();
    plan.plan.needs_bundle_setup = true;
    plan.plan.open_proposals = vec![2];

    assert!(
        matches!(
            stop(&plan, &Run::default()),
            Some(RoundQuiescence::NeedsBundleSetup)
        ),
        "no vote work can be planned until the bundle rows exist"
    );
}

#[test]
fn an_empty_plan_with_nothing_owed_is_no_work_left() {
    let mut plan = share_round_plan();
    plan.plan.next_steps.clear();

    assert!(matches!(
        stop(&plan, &Run::default()),
        Some(RoundQuiescence::NoWorkLeft)
    ));
}
