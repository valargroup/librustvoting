use super::*;

#[tokio::test]
async fn delegation_setup_is_bundle_scoped_but_excludes_destructive_access() {
    let first = identity(ChainSubmissionTarget::Delegation, 3);
    let second = identity(ChainSubmissionTarget::Delegation, 4);
    let coordination = SubmissionCoordination::default();

    let first_lease = coordination
        .try_acquire_delegation_setup(&first)
        .unwrap_or_else(|_| panic!("the first bundle should be idle"));
    let _second_lease = coordination
        .try_acquire_delegation_setup(&second)
        .unwrap_or_else(|_| panic!("an unrelated bundle should remain independent"));

    assert!(matches!(
        coordination.try_acquire_delegation_setup(&first),
        Err(ExclusiveRoundAcquireError::Busy)
    ));
    assert!(matches!(
        coordination.try_acquire_round_exclusive(&first),
        Err(ExclusiveRoundAcquireError::Busy)
    ));
    assert!(matches!(
        coordination.try_acquire_account_exclusive(first.wallet_id()),
        Err(ExclusiveRoundAcquireError::Busy)
    ));

    drop(first_lease);
}

#[tokio::test]
async fn delegation_setup_excludes_only_its_bundle_lifecycle() {
    let setup_identity = identity(ChainSubmissionTarget::Delegation, 3);
    let other_identity = identity(ChainSubmissionTarget::Delegation, 4);
    let coordination = SubmissionCoordination::default();
    let setup_lease = coordination
        .try_acquire_delegation_setup(&setup_identity)
        .unwrap_or_else(|_| panic!("the setup bundle should be idle"));

    let same_bundle = CapturedSubmissionOperation::new(setup_identity.clone(), 4);
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(10),
            coordination.acquire(&same_bundle, std::slice::from_ref(&setup_identity),),
        )
        .await
        .is_err(),
        "the same bundle lifecycle must wait for setup"
    );

    let other_bundle = CapturedSubmissionOperation::new(other_identity.clone(), 4);
    coordination
        .acquire(&other_bundle, std::slice::from_ref(&other_identity))
        .await
        .expect("another bundle lifecycle should remain independent");

    drop(setup_lease);
}
