//! `CastVote`: prerequisites, vote-tree failover and reset, and hotkey checks.

use super::fixtures::*;

#[tokio::test]
async fn empty_plan_and_stale_steps_return_no_work_without_network_io() {
    let executor = executor();
    let control = ChainSubmissionControl::new(1);
    assert!(
        executor.plan().unwrap().next_steps.is_empty(),
        "an undecided round owes nothing"
    );
    assert!(
        advance_plan_head(
            &executor,
            &host(),
            &control,
            &NoopRoundStepProgressReporter {},
        )
        .await
        .is_none(),
        "an empty plan has no head to run"
    );

    let stale = NextStep::AdvanceVote {
        bundle_index: 0,
        proposal_id: 1,
    };
    let outcome = executor
        .advance_step_in_epoch(
            stale.clone(),
            &host(),
            &control,
            control.operation_epoch(),
            &NoopRoundStepProgressReporter {},
        )
        .await
        .unwrap();
    assert_eq!(outcome.disposition, RoundStepDisposition::NoWork);
    assert_eq!(outcome.step, Some(stale));
}

#[tokio::test]
async fn a_cast_vote_selected_ahead_of_its_delegation_is_rejected_before_any_work() {
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
    let cast = NextStep::CastVote {
        bundle_index: 0,
        proposal_id: 1,
        choice: 0,
    };
    let plan = executor.plan().unwrap();
    assert_eq!(
        plan.next_steps,
        vec![NextStep::Delegate { bundle_index: 0 }, cast.clone()]
    );

    // The only node URL is unreachable, so reaching tree sync would fail
    // with a transport error rather than InvalidInput.
    let control = ChainSubmissionControl::new(1);
    let failure = executor
        .advance_step_in_epoch(
            cast.clone(),
            &host(),
            &control,
            control.operation_epoch(),
            &NoopRoundStepProgressReporter {},
        )
        .await
        .expect_err("a step with an unresolved delegation prerequisite must not run");

    assert_eq!(failure.kind, RoundStepFailureKind::InvalidInput);
    assert_eq!(failure.step, Some(cast));
    assert!(failure.message.contains("Delegate"), "{}", failure.message);
}

#[tokio::test]
async fn a_failed_sync_on_the_only_node_clears_the_cached_round_tree() {
    let requests = cast_against_unreachable_nodes(
        "wallet-single-node",
        vec!["http://node-a.invalid".to_string()],
    )
    .await;
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn a_failed_sync_on_the_last_node_clears_the_cached_round_tree() {
    let requests = cast_against_unreachable_nodes(
        "wallet-two-nodes",
        vec![
            "http://node-a.invalid".to_string(),
            "http://node-b.invalid".to_string(),
        ],
    )
    .await;
    assert_eq!(requests, 2, "both nodes are tried in order");
}

#[tokio::test]
async fn a_bound_hotkey_that_is_not_the_delegation_target_is_refused_before_tree_io() {
    // The bundle's confirmed delegation targets hotkey 0x21; the executor
    // is bound to a different valid hotkey.
    let (executor, transport) =
        executor_ready_to_cast_with_hotkey_and_control("wallet-wrong-hotkey", 0x22, None);
    let cast = NextStep::CastVote {
        bundle_index: 0,
        proposal_id: 1,
        choice: 0,
    };
    let control = ChainSubmissionControl::new(1);

    let failure = executor
        .advance_step_in_epoch(
            cast,
            &host(),
            &control,
            control.operation_epoch(),
            &NoopRoundStepProgressReporter {},
        )
        .await
        .expect_err("hotkey 0x22 cannot spend a delegation made for 0x21");

    assert_eq!(failure.kind, RoundStepFailureKind::InvalidInput);
    assert!(
        failure
            .message
            .contains("does not reproduce from the bound voting hotkey"),
        "{}",
        failure.message
    );
    assert_eq!(
        transport.requests.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no tree request may be made for a hotkey that cannot vote"
    );
}

#[tokio::test]
async fn a_cast_after_the_authenticated_vote_end_is_refused_before_tree_io() {
    let (executor, transport) = executor_ready_to_cast("wallet-vote-ended");
    let cast = NextStep::CastVote {
        bundle_index: 0,
        proposal_id: 1,
        choice: 0,
    };
    let control = ChainSubmissionControl::new(1);
    let mut after_end = host();
    after_end.now_seconds = after_end.vote_end_time_seconds.unwrap();

    let failure = executor
        .advance_step_in_epoch(
            cast,
            &after_end,
            &control,
            control.operation_epoch(),
            &NoopRoundStepProgressReporter {},
        )
        .await
        .expect_err("the vote has ended");

    assert_eq!(failure.kind, RoundStepFailureKind::VoteEnded);
    assert!(
        failure.message.contains("vote ended"),
        "{}",
        failure.message
    );
    assert_eq!(
        transport.requests.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no tree request may be made for a vote that can no longer be cast"
    );
}
