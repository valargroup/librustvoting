//! The executor's wallet scope is frozen at construction.

use super::fixtures::*;

#[tokio::test]
async fn a_host_wallet_switch_does_not_retarget_a_bound_executor() {
    let (executor, host_handle) = executor_over(host_database());
    let bound_plan = executor.plan().unwrap();
    assert!(!bound_plan.delegation_statuses.is_empty());

    // The host moves its own handle to an account with no state in this round.
    host_handle.set_wallet_id("other-wallet");

    assert_eq!(executor.database().wallet_id(), "wallet");
    assert!(executor.database().shares_connection_with(&host_handle));
    let plan_after_switch = executor.plan().unwrap();
    assert_eq!(
        plan_after_switch.delegation_statuses,
        bound_plan.delegation_statuses
    );
    let control = ChainSubmissionControl::new(1);
    let outcome = executor
        .advance_next(&host(), &control, &NoopRoundStepProgressReporter {})
        .await
        .unwrap();
    assert_eq!(outcome.disposition, RoundStepDisposition::NoWork);
}

#[tokio::test]
async fn re_scoping_a_handle_from_database_does_not_reach_the_executor() {
    let executor = executor();
    let handle = executor.database();
    handle.set_wallet_id("other-wallet");

    // The executor never hands out its own handle, so the re-scope is
    // confined to the caller's copy.
    assert_eq!(executor.database().wallet_id(), "wallet");
    assert!(handle.shares_connection_with(&executor.database()));
    let plan = executor.plan().unwrap();
    assert!(!plan.delegation_statuses.is_empty());
    let control = ChainSubmissionControl::new(1);
    let outcome = executor
        .advance_next(&host(), &control, &NoopRoundStepProgressReporter {})
        .await
        .unwrap();
    assert_eq!(outcome.disposition, RoundStepDisposition::NoWork);
}
