//! Delegation-signature handoff before a step is dispatched.

use super::fixtures::*;

#[tokio::test]
async fn a_missing_stored_keystone_signature_stops_for_the_host() {
    let database = database();
    let executor = executor_over(Arc::clone(&database));
    decide_ballot(&executor);
    let control = ChainSubmissionControl::new(1);
    let events = RecordingReporter::default();
    let report = RoundDriver::new(&executor)
        .run(
            &StoredSigningHost {
                database: Arc::clone(&database),
            },
            &control,
            &events,
        )
        .await;

    let RoundQuiescence::NeedsDelegationSignatures { bundles } = report.quiescence else {
        panic!(
            "missing storage is a signer handoff: {:?}",
            report.quiescence
        );
    };
    assert_eq!(bundles, vec![0]);
    assert!(report.failures.is_empty());
    assert!(report.skipped_bundles.is_empty());
    assert!(!events
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|event| matches!(event, RoundDriveEvent::StepSelected { .. })));
}

#[tokio::test]
async fn stored_keystone_handoff_names_only_unsigned_bundles() {
    let database = database_with_bundles(2);
    database
        .store_keystone_signature(ROUND_ID, 0, &[0x68; 64], &[0x69; 32], &[0x62; 32])
        .unwrap();
    let executor = executor_over(Arc::clone(&database));
    decide_ballot(&executor);
    let control = ChainSubmissionControl::new(1);
    let report = RoundDriver::new(&executor)
        .run(
            &StoredSigningHost {
                database: Arc::clone(&database),
            },
            &control,
            &RecordingReporter::default(),
        )
        .await;

    let RoundQuiescence::NeedsDelegationSignatures { bundles } = report.quiescence else {
        panic!(
            "only unsigned bundles are handed off: {:?}",
            report.quiescence
        );
    };
    assert_eq!(bundles, vec![1]);
    assert!(report.failures.is_empty());
}

#[tokio::test]
async fn a_present_stored_keystone_signature_is_dispatched() {
    let database = database();
    database
        .store_keystone_signature(ROUND_ID, 0, &[0x68; 64], &[0x69; 32], &[0x62; 32])
        .unwrap();
    let executor = executor_over(Arc::clone(&database));
    decide_ballot(&executor);
    let control = ChainSubmissionControl::new(1);
    let events = RecordingReporter::default();
    let report = RoundDriver::new(&executor)
        .run(
            &StoredSigningHost {
                database: Arc::clone(&database),
            },
            &control,
            &events,
        )
        .await;

    assert!(events
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|event| matches!(event, RoundDriveEvent::StepSelected { .. })));
    assert!(
        !matches!(
            report.quiescence,
            RoundQuiescence::NeedsDelegationSignatures { .. }
        ),
        "{:?}",
        report.quiescence
    );
}

#[tokio::test]
async fn every_unsigned_bundle_is_named_before_anything_is_dispatched() {
    // Four bundles against a concurrency limit of three: the unsigned bundle
    // falls outside the first wave. Checking only the wave would prove and
    // broadcast the three signed bundles and report the fourth one wave
    // later, so the voter would sign in two device rounds and the first three
    // delegations would already be on the wire before the first of them.
    let database = database_with_bundles(4);
    for bundle in 0..3u32 {
        database
            .store_keystone_signature(ROUND_ID, bundle, &[0x68; 64], &[0x69; 32], &[0x62; 32])
            .unwrap();
    }
    let executor = executor_over(Arc::clone(&database));
    decide_ballot(&executor);
    let control = ChainSubmissionControl::new(1);
    let events = RecordingReporter::default();
    let report = RoundDriver::new(&executor)
        .run(
            &StoredSigningHost {
                database: Arc::clone(&database),
            },
            &control,
            &events,
        )
        .await;

    let RoundQuiescence::NeedsDelegationSignatures { bundles } = report.quiescence else {
        panic!("an unsigned bundle stops the run: {:?}", report.quiescence);
    };
    assert_eq!(bundles, vec![3]);
    assert!(
        !events
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, RoundDriveEvent::StepSelected { .. })),
        "nothing runs before the voter has signed every bundle"
    );
}

#[tokio::test]
async fn the_handoff_names_every_unsigned_bundle_not_only_one_wave() {
    // With none signed, the handoff must still name all four, or the host
    // builds its signing request for three and comes back for the fourth.
    let database = database_with_bundles(4);
    let executor = executor_over(Arc::clone(&database));
    decide_ballot(&executor);
    let control = ChainSubmissionControl::new(1);
    let report = RoundDriver::new(&executor)
        .run(
            &StoredSigningHost {
                database: Arc::clone(&database),
            },
            &control,
            &RecordingReporter::default(),
        )
        .await;

    let RoundQuiescence::NeedsDelegationSignatures { bundles } = report.quiescence else {
        panic!("expected a signer handoff: {:?}", report.quiescence);
    };
    assert_eq!(bundles, vec![0, 1, 2, 3]);
}
