//! Round and bundle locks: scope, sidecar identity, and epoch-aware waiting.

use super::fixtures::*;

#[tokio::test]
async fn bundle_scoped_locks_do_not_serialize_distinct_bundles() {
    let control = ChainSubmissionControl::new(1);
    let first = super::super::round_lock::acquire(
        7,
        "w".to_string(),
        ROUND_ID,
        Some(0),
        &control,
        control.operation_epoch(),
    )
    .await
    .unwrap()
    .unwrap();
    let second = tokio::time::timeout(
        Duration::from_millis(200),
        super::super::round_lock::acquire(
            7,
            "w".to_string(),
            ROUND_ID,
            Some(1),
            &control,
            control.operation_epoch(),
        ),
    )
    .await
    .expect("a different bundle must not wait")
    .unwrap();
    assert!(second.is_some());
    let round_scope = tokio::time::timeout(
        Duration::from_millis(200),
        super::super::round_lock::acquire(
            7,
            "w".to_string(),
            ROUND_ID,
            None,
            &control,
            control.operation_epoch(),
        ),
    )
    .await
    .expect("the round scope is independent of bundle scopes")
    .unwrap();
    assert!(round_scope.is_some());
    let same_bundle = tokio::time::timeout(
        Duration::from_millis(100),
        super::super::round_lock::acquire(
            7,
            "w".to_string(),
            ROUND_ID,
            Some(0),
            &control,
            control.operation_epoch(),
        ),
    )
    .await;
    assert!(same_bundle.is_err(), "the same bundle must wait");
    drop(first);
    let _ = RoundStepFailureKind::Busy;
}

#[tokio::test]
async fn a_queued_lock_wait_stops_when_the_operation_epoch_changes() {
    let control = ChainSubmissionControl::new(1);
    let held = super::super::round_lock::acquire(
        7,
        "epoch-wait-wallet".to_string(),
        ROUND_ID,
        None,
        &control,
        control.operation_epoch(),
    )
    .await
    .unwrap()
    .unwrap();

    let waiter = super::super::round_lock::acquire(
        7,
        "epoch-wait-wallet".to_string(),
        ROUND_ID,
        None,
        &control,
        1,
    );
    let switch = async {
        tokio::time::sleep(Duration::from_millis(120)).await;
        control.set_operation_epoch(2);
    };
    let (outcome, ()) = tokio::join!(waiter, switch);

    assert!(
        outcome.unwrap().is_none(),
        "a stale caller must stop queuing"
    );
    assert!(!control.is_cancelled());
    drop(held);
}

#[tokio::test]
async fn locks_for_different_sidecars_with_one_wallet_id_do_not_serialize() {
    let control = ChainSubmissionControl::new(1);
    let first = super::super::round_lock::acquire(
        11,
        "shared-wallet-name".to_string(),
        ROUND_ID,
        None,
        &control,
        control.operation_epoch(),
    )
    .await
    .unwrap()
    .unwrap();
    let other_sidecar = tokio::time::timeout(
        Duration::from_millis(200),
        super::super::round_lock::acquire(
            12,
            "shared-wallet-name".to_string(),
            ROUND_ID,
            None,
            &control,
            control.operation_epoch(),
        ),
    )
    .await
    .expect("an unrelated sidecar must not wait")
    .unwrap();
    assert!(other_sidecar.is_some());
    drop(first);
}
