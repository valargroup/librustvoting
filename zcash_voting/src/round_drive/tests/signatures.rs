//! Delegation-signature handoff before a step is dispatched.

use super::fixtures::*;

fn store_signature(database: &crate::round::VotingDb, bundle_index: u32) {
    database
        .conn()
        .execute(
            "UPDATE bundles SET pczt_sighash = ?1, rk = ?2 WHERE bundle_index = ?3",
            rusqlite::params![vec![0x69u8; 32], vec![0x62u8; 32], bundle_index],
        )
        .unwrap();
    database
        .store_keystone_signature(
            ROUND_ID,
            bundle_index,
            &[0x68; 64],
            &[0x69; 32],
            &[0x62; 32],
        )
        .unwrap();
}

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
    store_signature(&database, 0);
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
    store_signature(&database, 0);
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
        store_signature(&database, bundle);
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

#[tokio::test]
async fn each_bundle_is_judged_by_its_own_signer_context() {
    // `RoundHostSource` is sampled once per dispatch, and nothing requires two
    // samples to agree. Reading only the first context let a wave whose first
    // step carried its own signature skip the stored-material gate entirely,
    // so a bundle whose own context needed a stored signature that did not
    // exist was dispatched anyway. Collapsing the modes the other way is just
    // as wrong: it would demand a durable row for the bundle that signs during
    // its own step, a handoff the host could never satisfy because there is
    // nothing for it to store.
    let database = database_with_bundles(2);
    let executor = executor_over(Arc::clone(&database));
    decide_ballot(&executor);
    let control = ChainSubmissionControl::new(1);
    let events = RecordingReporter::default();

    let report = RoundDriver::new(&executor)
        .with_policy(RoundDrivePolicy {
            max_bundle_concurrency: std::num::NonZeroUsize::new(2).unwrap(),
            ..RoundDrivePolicy::default()
        })
        .run(
            &DriftingSigningHost::new(Arc::clone(&database)),
            &control,
            &events,
        )
        .await;

    let RoundQuiescence::NeedsDelegationSignatures { bundles } = report.quiescence else {
        panic!(
            "the second context reads stored material that does not exist: {:?}",
            report.quiescence
        );
    };
    assert_eq!(
        bundles,
        vec![1],
        "bundle 0 carries its own signature and is owed nothing"
    );
    assert!(
        !events
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, RoundDriveEvent::StepSelected { .. })),
        "nothing is dispatched before the host is asked"
    );
}

/// A host whose first sample has no delegation inputs at all and whose later
/// samples read stored material.
pub(super) struct MixedSigningHost {
    database: Arc<crate::round::VotingDb>,
    samples: std::sync::atomic::AtomicUsize,
}

impl crate::RoundHostSource for MixedSigningHost {
    fn host_context(&self) -> RoundHostContext {
        if self
            .samples
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            return host();
        }
        stored_signing_context(&self.database)
    }
}

#[tokio::test]
async fn a_bundle_that_cannot_sign_does_not_condemn_one_that_already_has() {
    // A wave holding both a step with no signing inputs and a step that reads
    // stored material must answer for each on its own terms. Returning the
    // whole owed set as soon as one context cannot sign reported the
    // stored-signed bundle as missing too, sending the host to collect a
    // signature it had already stored.
    let database = database_with_bundles(2);
    store_signature(&database, 1);
    let executor = executor_over(Arc::clone(&database));
    decide_ballot(&executor);
    let control = ChainSubmissionControl::new(1);

    let report = RoundDriver::new(&executor)
        .with_policy(RoundDrivePolicy {
            max_bundle_concurrency: std::num::NonZeroUsize::new(2).unwrap(),
            ..RoundDrivePolicy::default()
        })
        .run(
            &MixedSigningHost {
                database: Arc::clone(&database),
                samples: std::sync::atomic::AtomicUsize::new(0),
            },
            &control,
            &RecordingReporter::default(),
        )
        .await;

    let RoundQuiescence::NeedsDelegationSignatures { bundles } = report.quiescence else {
        panic!("bundle 0 has no signing inputs: {:?}", report.quiescence);
    };
    assert_eq!(
        bundles,
        vec![0],
        "bundle 1's stored signature is already there"
    );
}
