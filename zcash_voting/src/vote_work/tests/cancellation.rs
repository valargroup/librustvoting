//! Cancellation and operation-epoch changes end a step at every boundary.

use super::fixtures::*;

#[tokio::test]
async fn a_cancelled_control_does_not_invent_work_for_an_idle_round() {
    // Planning is a read of durable state and answers the same whether or not
    // the host has cancelled: an undecided round owes nothing yet. There is
    // therefore no step for cancellation to short-circuit.
    let executor = executor();
    let control = ChainSubmissionControl::new(1);
    control.cancel();

    assert!(
        executor.plan().unwrap().next_steps.is_empty(),
        "an undecided round lists no step, cancelled or not"
    );
    assert!(advance_plan_head(
        &executor,
        &host(),
        &control,
        &NoopRoundStepProgressReporter {}
    )
    .await
    .is_none());
}

#[tokio::test]
async fn a_delegate_step_cancelled_after_preparation_keeps_the_proof_without_signing() {
    let executor = executor();
    executor
        .set_ballot_intents(&[
            BallotIntent {
                proposal_id: 1,
                decision: Decision::Choice(0),
            },
            BallotIntent {
                proposal_id: 2,
                decision: Decision::Skipped,
            },
        ])
        .unwrap();
    let control = ChainSubmissionControl::new(1);
    let step = NextStep::Delegate { bundle_index: 0 };
    assert!(executor.plan().unwrap().next_steps.contains(&step));

    let outcome = executor
        .advance_step_in_epoch(
            step.clone(),
            &host_with_delegation(&control, "wallet", &executor.database()),
            &control,
            control.operation_epoch(),
            &NoopRoundStepProgressReporter {},
        )
        .await
        .unwrap();

    assert_eq!(outcome.disposition, RoundStepDisposition::Cancelled);
    assert_eq!(outcome.step, Some(step));
    assert!(outcome.delegation.is_none());
    assert_eq!(
        executor.database().delegation_phase(ROUND_ID, 0).unwrap(),
        crate::phases::DelegationPhase::Proved
    );
}

#[tokio::test]
async fn a_delegate_step_stops_when_the_host_moves_to_a_new_operation_epoch() {
    let executor = executor();
    decided_ballot(&executor);
    let control = ChainSubmissionControl::new(7);
    let step = NextStep::Delegate { bundle_index: 0 };

    let outcome = executor
        .advance_step_in_epoch(
            step.clone(),
            &host_with_interrupting_delegation(
                &control,
                Interrupt::NewOperationEpoch,
                "wallet",
                &executor.database(),
            ),
            &control,
            control.operation_epoch(),
            &NoopRoundStepProgressReporter {},
        )
        .await
        .unwrap();

    // Not cancelled, but the epoch the step started under is gone: the
    // step must not dispatch to the chain on behalf of epoch 7.
    assert!(!control.is_cancelled());
    assert_eq!(control.operation_epoch(), 8);
    assert_eq!(outcome.disposition, RoundStepDisposition::Cancelled);
    assert!(outcome.delegation.is_none());
}

#[tokio::test]
async fn a_sync_that_fails_after_cancellation_reports_cancelled_not_a_transport_error() {
    let control = ChainSubmissionControl::new(1);
    let (executor, transport) =
        executor_ready_to_cast_with("wallet-cancel-in-flight", Some(control.clone()));
    let cast = NextStep::CastVote {
        bundle_index: 0,
        proposal_id: 1,
        choice: 0,
    };
    let host = RoundHostContext {
        vote_tree_node_urls: vec![
            "http://node-a.invalid".to_string(),
            "http://node-b.invalid".to_string(),
        ],
        ..host()
    };

    let outcome = executor
        .advance_step_in_epoch(
            cast.clone(),
            &host,
            &control,
            control.operation_epoch(),
            &NoopRoundStepProgressReporter {},
        )
        .await
        .expect("a cancelled step is an outcome, not a failure");

    let cached = crate::precompute::cached_vote_tree_rounds(&executor.database())
        .contains(&ROUND_ID.to_string());
    crate::precompute::reset_vote_tree(&executor.database(), "").unwrap();
    assert_eq!(outcome.disposition, RoundStepDisposition::Cancelled);
    assert_eq!(outcome.step, Some(cast));
    assert_eq!(
        transport.requests.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the second node must not be tried after cancellation"
    );
    assert!(!cached, "the poisoned tree is still reset before returning");
}

#[tokio::test]
async fn an_epoch_change_during_resigning_stops_delegation_advancement() {
    let executor = executor();
    // A submitted delegation with no confirmed position plans as
    // AdvanceDelegation.
    executor
        .database()
        .store_delegation_tx_hash(ROUND_ID, 0, "dtx")
        .unwrap();
    let step = NextStep::AdvanceDelegation { bundle_index: 0 };
    assert!(executor.plan().unwrap().next_steps.contains(&step));
    let control = ChainSubmissionControl::new(7);

    let outcome = executor
        .advance_step_in_epoch(
            step.clone(),
            &host_with_interrupting_delegation(
                &control,
                Interrupt::NewOperationEpoch,
                "wallet",
                &executor.database(),
            ),
            &control,
            control.operation_epoch(),
            &NoopRoundStepProgressReporter {},
        )
        .await
        .expect("an interrupted step is an outcome, not a chain failure");

    // The chain endpoint is unreachable, so reaching it would have failed
    // with a transport error; the epoch check must come first.
    assert_eq!(control.operation_epoch(), 8);
    assert_eq!(outcome.disposition, RoundStepDisposition::Cancelled);
    assert_eq!(outcome.step, Some(step));
}
