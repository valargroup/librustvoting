//! Bounded overlap for independently bundle-locked delegation work.

use super::fixtures::*;

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Condvar,
};

#[derive(Default)]
struct ConcurrencyProbe {
    active: AtomicUsize,
    maximum: AtomicUsize,
    release: Mutex<bool>,
    released_bundles: Mutex<std::collections::BTreeSet<u32>>,
    released: Condvar,
    entered: Mutex<Option<tokio::sync::mpsc::UnboundedSender<u32>>>,
}

impl ConcurrencyProbe {
    fn enter(&self, bundle_index: u32) {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        if let Some(entered) = self.entered.lock().unwrap().as_ref() {
            entered.send(bundle_index).unwrap();
        }
        let mut release = self.release.lock().unwrap();
        while !*release
            && !self
                .released_bundles
                .lock()
                .unwrap()
                .contains(&bundle_index)
        {
            release = self.released.wait(release).unwrap();
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
    }

    fn release_bundle(&self, bundle: u32) {
        let _release = self.release.lock().unwrap();
        self.released_bundles.lock().unwrap().insert(bundle);
        self.released.notify_all();
    }

    fn release(&self) {
        *self.release.lock().unwrap() = true;
        self.released.notify_all();
    }
}

struct GatedSigningDriver {
    database: Arc<crate::round::VotingDb>,
    probe: Arc<ConcurrencyProbe>,
}

impl crate::DelegationDriver for GatedSigningDriver {
    fn round_id(&self) -> &str {
        ROUND_ID
    }

    fn network(&self) -> Network {
        Network::Testnet
    }

    fn delegation_target(&self) -> Option<crate::VotingHotkeyTarget> {
        Some(hotkey_target())
    }

    fn wallet_id(&self) -> &str {
        WALLET_ID
    }

    fn shares_database_with(&self, database: &crate::round::VotingDb) -> bool {
        self.database.shares_connection_with(database)
    }

    fn prepare_blocking(
        &self,
        bundle_index: u32,
        pir: &crate::PirFleet,
        progress: &dyn crate::types::DelegationProgressReporter,
    ) -> Result<crate::delegate::DelegationProofStatus, crate::VotingError> {
        self.probe.enter(bundle_index);
        crate::DelegationDriver::prepare_blocking(
            &SigningDriver {
                database: Arc::clone(&self.database),
            },
            bundle_index,
            pir,
            progress,
        )
    }

    fn prove_and_sign_blocking(
        &self,
        bundle_index: u32,
        signer: &crate::DelegationSigner,
        pir: &crate::PirFleet,
        progress: &dyn crate::types::DelegationProgressReporter,
    ) -> Result<crate::delegate::SignedDelegationBundle, crate::VotingError> {
        self.probe.enter(bundle_index);
        crate::DelegationDriver::prove_and_sign_blocking(
            &SigningDriver {
                database: Arc::clone(&self.database),
            },
            bundle_index,
            signer,
            pir,
            progress,
        )
    }

    fn resign_blocking(
        &self,
        bundle_index: u32,
        signer: &crate::DelegationSigner,
    ) -> Result<[u8; 64], crate::VotingError> {
        crate::DelegationDriver::resign_blocking(
            &SigningDriver {
                database: Arc::clone(&self.database),
            },
            bundle_index,
            signer,
        )
    }
}

struct GatedSigningHost {
    database: Arc<crate::round::VotingDb>,
    probe: Arc<ConcurrencyProbe>,
}

impl RoundHostSource for GatedSigningHost {
    fn host_context(&self) -> RoundHostContext {
        let mut context = SigningHost {
            database: Arc::clone(&self.database),
        }
        .host_context();
        context.delegation.as_mut().unwrap().driver = Arc::new(GatedSigningDriver {
            database: Arc::clone(&self.database),
            probe: Arc::clone(&self.probe),
        });
        context
    }
}

async fn observe_wave(
    bundle_count: usize,
    max_bundle_concurrency: usize,
    max_dispatches: usize,
) -> (RoundRunReport, usize, Vec<u32>) {
    let database = database_with_bundles(bundle_count);
    let executor = executor_over_unreachable_chain(Arc::clone(&database));
    decide_ballot(&executor);
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let probe = Arc::new(ConcurrencyProbe::default());
    *probe.entered.lock().unwrap() = Some(entered_tx);
    let run_probe = Arc::clone(&probe);
    let task = tokio::spawn(async move {
        let control = ChainSubmissionControl::new(1);
        RoundDriver::new(&executor)
            .with_policy(RoundDrivePolicy {
                max_bundle_concurrency: std::num::NonZeroUsize::new(max_bundle_concurrency)
                    .unwrap(),
                max_dispatches,
                ..RoundDrivePolicy::default()
            })
            .run(
                &GatedSigningHost {
                    database,
                    probe: run_probe,
                },
                &control,
                &RecordingReporter::default(),
            )
            .await
    });

    let expected = max_bundle_concurrency.min(max_dispatches).min(bundle_count);
    let mut entered = Vec::new();
    for _ in 0..expected {
        entered.push(
            tokio::time::timeout(Duration::from_secs(2), entered_rx.recv())
                .await
                .expect("an admitted bundle enters promptly")
                .expect("the entry sender remains open"),
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), entered_rx.recv())
            .await
            .is_err(),
        "no bundle beyond the configured wave enters before release"
    );
    probe.release();
    let report = task.await.unwrap();
    (report, probe.maximum.load(Ordering::SeqCst), entered)
}

#[tokio::test]
async fn bundle_steps_run_up_to_the_configured_limit() {
    let (report, maximum, mut entered) = observe_wave(3, 2, 2).await;
    entered.sort_unstable();
    assert_eq!(entered, vec![0, 1]);
    assert_eq!(maximum, 2);
    assert!(report.failures.is_empty());
}

#[tokio::test]
async fn one_bundle_slot_keeps_bundle_steps_serial() {
    let (report, maximum, entered) = observe_wave(2, 1, 1).await;
    assert_eq!(entered, vec![0]);
    assert_eq!(maximum, 1);
    assert!(report.failures.is_empty());
}

#[tokio::test]
async fn dispatch_budget_is_not_overshot_by_concurrent_launches() {
    let (report, _, entered) = observe_wave(3, 3, 2).await;
    assert_eq!(entered.len(), 2);
    assert!(report.failures.is_empty());
}

#[tokio::test]
async fn a_finished_pipeline_refills_while_an_original_bundle_remains_blocked() {
    let database = database_with_bundles(7);
    let executor = executor_over_unreachable_chain(Arc::clone(&database));
    executor
        .set_ballot_intents(&[BallotIntent {
            proposal_id: 1,
            decision: Decision::Choice(0),
        }])
        .unwrap();
    // An open ballot leaves delegation preparation as the only executable work.
    let (sender, mut entered) = tokio::sync::mpsc::unbounded_channel();
    let probe = Arc::new(ConcurrencyProbe::default());
    *probe.entered.lock().unwrap() = Some(sender);
    let running = probe.clone();
    let task = tokio::spawn(async move {
        let events = RecordingReporter::default();
        let report = RoundDriver::new(&executor)
            .with_policy(RoundDrivePolicy {
                max_dispatches: 7,
                ..Default::default()
            })
            .run(
                &GatedSigningHost {
                    database,
                    probe: running,
                },
                &ChainSubmissionControl::new(1),
                &events,
            )
            .await;
        (report, events)
    });
    let mut first = Vec::new();
    for _ in 0..5 {
        first.push(
            tokio::time::timeout(Duration::from_secs(5), entered.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    first.sort_unstable();
    assert_eq!(first, vec![0, 1, 2, 3, 4]);
    probe.release_bundle(3);
    let replacement = tokio::time::timeout(Duration::from_secs(5), entered.recv()).await;
    // Always release workers before asserting, including a scheduler regression.
    probe.release();
    assert_eq!(replacement.unwrap().unwrap(), 5);
    let (report, events) = task.await.unwrap();
    assert!(report.failures.is_empty());
    assert_eq!(probe.maximum.load(Ordering::SeqCst), 5);
    let finished = events
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            RoundDriveEvent::StepFinished { step, .. } => Some(step.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        finished.first(),
        Some(&NextStep::Delegate { bundle_index: 3 })
    );
}
