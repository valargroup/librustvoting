//! A delegation driver must match the executor's wallet, database, network, and hotkey.

use super::fixtures::*;

#[tokio::test]
async fn a_driver_scoped_to_another_wallet_is_refused_before_proving() {
    let executor = executor();
    decided_ballot(&executor);
    let control = ChainSubmissionControl::new(1);
    let step = NextStep::Delegate { bundle_index: 0 };

    let failure = executor
        .advance_step(
            step.clone(),
            &host_with_delegation(&control, "other-wallet", &executor.database()),
            &control,
            &NoopRoundStepProgressReporter {},
        )
        .await
        .expect_err("a driver for another wallet must not run under this wallet's lock");

    assert_eq!(failure.kind, RoundStepFailureKind::InvalidInput);
    assert!(
        failure.message.contains("other-wallet"),
        "{}",
        failure.message
    );
    assert!(
        !control.is_cancelled(),
        "the driver must not have been invoked"
    );
}

#[tokio::test]
async fn a_driver_over_another_database_is_refused_before_proving() {
    let executor = executor();
    decided_ballot(&executor);
    let control = ChainSubmissionControl::new(1);
    let foreign = host_database_for("wallet");

    let failure = executor
        .advance_step(
            NextStep::Delegate { bundle_index: 0 },
            &host_with_delegation(&control, "wallet", &foreign),
            &control,
            &NoopRoundStepProgressReporter {},
        )
        .await
        .expect_err("a driver over another sidecar must not run");

    assert_eq!(failure.kind, RoundStepFailureKind::InvalidInput);
    assert!(
        failure.message.contains("different voting database"),
        "{}",
        failure.message
    );
    assert!(!control.is_cancelled());
}

#[tokio::test]
async fn a_driver_for_another_network_is_refused_before_proving() {
    let executor = executor();
    decided_ballot(&executor);
    let control = ChainSubmissionControl::new(1);

    let failure = executor
        .advance_step(
            NextStep::Delegate { bundle_index: 0 },
            &host_with_driver(
                &control,
                Interrupt::Cancel,
                Network::Mainnet,
                "wallet",
                &executor.database(),
            ),
            &control,
            &NoopRoundStepProgressReporter {},
        )
        .await
        .expect_err("a Mainnet driver must not prove for a Testnet binding");

    assert_eq!(failure.kind, RoundStepFailureKind::InvalidInput);
    assert!(failure.message.contains("Mainnet"), "{}", failure.message);
    assert!(
        !control.is_cancelled(),
        "the driver must not have been invoked"
    );
}

#[tokio::test]
async fn a_driver_for_another_hotkey_than_the_binding_is_refused_before_proving() {
    // The binding votes with hotkey 0x21; the driver would delegate to 0x22.
    let (executor, _) = bound_executor(
        host_database(),
        Some(zeroize::Zeroizing::new(vec![0x21; 64])),
    );
    decided_ballot(&executor);
    let control = ChainSubmissionControl::new(1);

    let failure = executor
        .advance_step(
            NextStep::Delegate { bundle_index: 0 },
            &host_with_driver_target(
                &control,
                Interrupt::Cancel,
                Network::Testnet,
                Some(hotkey_target(0x22)),
                "wallet",
                &executor.database(),
            ),
            &control,
            &NoopRoundStepProgressReporter {},
        )
        .await
        .expect_err("a delegation for another hotkey must not be proved");
    assert_eq!(failure.kind, RoundStepFailureKind::InvalidInput);
    assert!(
        failure.message.contains("voting hotkey"),
        "{}",
        failure.message
    );
    assert!(
        !control.is_cancelled(),
        "the driver must not have been invoked"
    );

    // The matching hotkey proceeds to the driver.
    let control = ChainSubmissionControl::new(1);
    let outcome = executor
        .advance_step(
            NextStep::Delegate { bundle_index: 0 },
            &host_with_driver_target(
                &control,
                Interrupt::Cancel,
                Network::Testnet,
                Some(hotkey_target(0x21)),
                "wallet",
                &executor.database(),
            ),
            &control,
            &NoopRoundStepProgressReporter {},
        )
        .await
        .unwrap();
    assert_eq!(outcome.disposition, RoundStepDisposition::Cancelled);
}
