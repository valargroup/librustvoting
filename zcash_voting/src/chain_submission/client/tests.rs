//! The inter-pass repoll wait observes host cancellation promptly.

use std::time::Duration;

use super::{
    interrupted_during, ChainSubmissionClient, ChainSubmissionClientConfig, ChainSubmissionControl,
};

#[tokio::test(start_paused = true)]
async fn a_cancellation_during_the_repoll_wait_returns_promptly() {
    let control = ChainSubmissionControl::new(1);
    let waiter = {
        let control = control.clone();
        tokio::spawn(
            async move { interrupted_during(Duration::from_secs(3_600), &control, 1).await },
        )
    };
    let started = tokio::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !waiter.is_finished(),
        "the wait must not end before cancellation"
    );

    control.cancel();
    let cancelled = waiter.await.unwrap();

    assert!(cancelled);
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "cancellation took {:?} to be observed",
        started.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_repoll_wait_ends_without_advancing_the_clock() {
    // The mechanism, not just the latency. A wait that re-read the control on
    // a tick would need the clock to reach that tick before it noticed, and
    // under paused time the runtime supplies exactly that by auto-advancing
    // whenever every task is idle — so the test above passes either way. Being
    // woken by the control costs no clock movement, which is what makes an
    // unbounded `pending_repoll` free to wait out.
    let control = ChainSubmissionControl::new(1);
    let waiter = {
        let control = control.clone();
        tokio::spawn(
            async move { interrupted_during(Duration::from_secs(86_400), &control, 1).await },
        )
    };
    // Deliberately not a multiple of any poll tick: landing on one would leave
    // a polling implementation already runnable at `at_cancel` and let it pass.
    tokio::time::sleep(Duration::from_millis(3)).await;
    assert!(!waiter.is_finished());

    let at_cancel = tokio::time::Instant::now();
    control.cancel();

    assert!(waiter.await.unwrap());
    assert_eq!(
        tokio::time::Instant::now(),
        at_cancel,
        "the wait is woken by the control, not by the clock reaching a poll tick",
    );
}

#[tokio::test(start_paused = true)]
async fn an_uncancelled_repoll_wait_lasts_the_full_delay() {
    let control = ChainSubmissionControl::new(1);
    let started = tokio::time::Instant::now();

    let cancelled = interrupted_during(Duration::from_secs(90), &control, 1).await;

    assert!(!cancelled);
    assert_eq!(started.elapsed(), Duration::from_secs(90));
}

#[tokio::test(start_paused = true)]
async fn an_already_cancelled_control_skips_the_wait() {
    let control = ChainSubmissionControl::new(1);
    control.cancel();
    let started = tokio::time::Instant::now();

    assert!(interrupted_during(Duration::from_secs(90), &control, 1).await);
    assert_eq!(started.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn an_unrepresentable_repoll_deadline_waits_for_cancellation_instead_of_panicking() {
    let control = ChainSubmissionControl::new(1);
    let waiter = {
        let control = control.clone();
        tokio::spawn(async move { interrupted_during(Duration::MAX, &control, 1).await })
    };
    tokio::time::sleep(Duration::from_secs(3_600)).await;
    assert!(!waiter.is_finished(), "an unbounded repoll keeps waiting");

    control.cancel();

    assert!(waiter.await.unwrap());
}

#[tokio::test(start_paused = true)]
async fn an_operation_epoch_change_during_the_repoll_wait_interrupts_it() {
    let control = ChainSubmissionControl::new(1);
    let waiter = {
        let control = control.clone();
        tokio::spawn(
            async move { interrupted_during(Duration::from_secs(3_600), &control, 1).await },
        )
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!waiter.is_finished());

    control.set_operation_epoch(2);
    let interrupted = waiter.await.unwrap();

    assert!(interrupted);
    assert!(
        !control.is_cancelled(),
        "an epoch change is not a cancellation"
    );
}

#[test]
fn the_client_keeps_the_wallet_it_was_constructed_for() {
    let host_handle = std::sync::Arc::new(crate::round::VotingDb::open_in_memory().unwrap());
    host_handle.set_wallet_id("wallet-a");
    let client = ChainSubmissionClient::new(
        std::sync::Arc::clone(&host_handle),
        ChainSubmissionClientConfig::for_network(
            crate::Network::Testnet,
            vec!["http://chain.invalid".to_string()],
        ),
    )
    .unwrap();

    // The host moves its own handle to another account mid-episode.
    host_handle.set_wallet_id("wallet-b");

    assert_eq!(client.wallet_id(), "wallet-a");
}

mod episode {
    //! The episode loop: escalation, terminal outcomes, the pass budget, and
    //! interruption, driven by scripted pass results.

    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use super::super::{
        run_episode, ChainAdvanceOutcome, ChainAdvancePolicy, ChainRecoveryMode,
        ChainSubmissionControl,
    };
    use crate::chain_submission::{
        result::ValidatedChainSubmissionConfirmation, CandidateTransactionHash,
        ChainSubmissionDiagnostic, ChainSubmissionDiagnosticKind, ChainSubmissionFailure,
        ChainSubmissionFailureKind, ChainSubmissionPending, ChainSubmissionResult,
    };

    /// Whether an outcome is the one a scripted terminal result must produce.
    type OutcomeCheck = fn(&ChainAdvanceOutcome) -> bool;

    fn tracking() -> ChainSubmissionResult {
        ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking {
            candidate_transaction_hash: CandidateTransactionHash::from_bytes([0x11; 32]),
        })
    }

    fn recovering() -> ChainSubmissionResult {
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            diagnostic: diagnostic(ChainSubmissionDiagnosticKind::AmbiguousDispatch),
        })
    }

    fn diagnostic(kind: ChainSubmissionDiagnosticKind) -> ChainSubmissionDiagnostic {
        ChainSubmissionDiagnostic::from_redacted_message(kind, "scripted")
    }

    fn confirmed() -> ChainSubmissionResult {
        ChainSubmissionResult::Confirmed(
            ValidatedChainSubmissionConfirmation::from_tree(7, vec![3])
                .unwrap()
                .into_public(),
        )
    }

    /// Runs the loop over `script`, one result per pass, and returns the
    /// outcome with the recovery mode each pass was asked for.
    async fn run(
        policy: &ChainAdvancePolicy,
        control: &ChainSubmissionControl,
        script: Vec<ChainSubmissionResult>,
    ) -> (ChainAdvanceOutcome, Vec<ChainRecoveryMode>) {
        let script = Arc::new(Mutex::new(VecDeque::from(script)));
        let modes = Arc::new(Mutex::new(Vec::new()));
        let outcome = run_episode(policy, control, 1, |mode| {
            modes.lock().unwrap().push(mode);
            let next = script
                .lock()
                .unwrap()
                .pop_front()
                .expect("the script covers every pass the loop runs");
            async move { Ok(next) }
        })
        .await
        .unwrap();
        let modes = modes.lock().unwrap().clone();
        (outcome, modes)
    }

    fn policy() -> ChainAdvancePolicy {
        ChainAdvancePolicy {
            initial_recovery_mode: ChainRecoveryMode::StatusOnly,
            pending_repoll: Duration::from_secs(2),
            escalate_to_exact_tree: true,
            max_passes: 45,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn episode_escalates_to_exact_tree_once() {
        let control = ChainSubmissionControl::new(1);
        let started = tokio::time::Instant::now();

        let (outcome, modes) = run(&policy(), &control, vec![recovering(), recovering()]).await;

        assert!(matches!(
            outcome,
            ChainAdvanceOutcome::StillPending(ChainSubmissionPending::Recovering { .. })
        ));
        assert_eq!(
            modes,
            vec![ChainRecoveryMode::StatusOnly, ChainRecoveryMode::ExactTree],
            "one Recovering pass escalates; the second ends the episode"
        );
        assert_eq!(
            started.elapsed(),
            Duration::ZERO,
            "escalation is a different pass, not a repoll"
        );

        let exact_from_the_start = ChainAdvancePolicy {
            initial_recovery_mode: ChainRecoveryMode::ExactTree,
            ..policy()
        };
        let (outcome, modes) = run(&exact_from_the_start, &control, vec![recovering()]).await;
        assert!(matches!(outcome, ChainAdvanceOutcome::StillPending(_)));
        assert_eq!(modes, vec![ChainRecoveryMode::ExactTree]);

        let no_escalation = ChainAdvancePolicy {
            escalate_to_exact_tree: false,
            ..policy()
        };
        let (outcome, modes) = run(&no_escalation, &control, vec![recovering()]).await;
        assert!(matches!(outcome, ChainAdvanceOutcome::StillPending(_)));
        assert_eq!(modes, vec![ChainRecoveryMode::StatusOnly]);
    }

    #[tokio::test(start_paused = true)]
    async fn episode_never_retries_terminal_outcomes() {
        let control = ChainSubmissionControl::new(1);
        let terminal: [(ChainSubmissionResult, &str, OutcomeCheck); 4] = [
            (confirmed(), "confirmed", |outcome: &ChainAdvanceOutcome| {
                matches!(outcome, ChainAdvanceOutcome::Confirmed(_))
            }),
            (
                ChainSubmissionResult::SubmittedWithoutHash(diagnostic(
                    ChainSubmissionDiagnosticKind::NullifierAlreadySpent,
                )),
                "submitted without hash",
                |outcome: &ChainAdvanceOutcome| {
                    matches!(outcome, ChainAdvanceOutcome::SubmittedWithoutHash(_))
                },
            ),
            (
                ChainSubmissionResult::Rejected(diagnostic(
                    ChainSubmissionDiagnosticKind::ChainRejected,
                )),
                "rejected",
                |outcome: &ChainAdvanceOutcome| matches!(outcome, ChainAdvanceOutcome::Rejected(_)),
            ),
            (
                ChainSubmissionResult::Cancelled,
                "cancelled",
                |outcome: &ChainAdvanceOutcome| matches!(outcome, ChainAdvanceOutcome::Cancelled),
            ),
        ];
        for (result, label, expected) in terminal {
            let started = tokio::time::Instant::now();
            // Two more passes are scripted; a retry would consume them.
            let (outcome, modes) =
                run(&policy(), &control, vec![result, tracking(), tracking()]).await;
            assert!(expected(&outcome), "{label}: {outcome:?}");
            assert_eq!(modes.len(), 1, "{label} ended the episode on its own pass");
            assert_eq!(
                started.elapsed(),
                Duration::ZERO,
                "{label} waited for nothing"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn episode_ends_after_its_pass_budget_and_paces_tracking_polls() {
        let control = ChainSubmissionControl::new(1);
        let bounded = ChainAdvancePolicy {
            max_passes: 3,
            ..policy()
        };
        let started = tokio::time::Instant::now();

        let (outcome, modes) = run(
            &bounded,
            &control,
            vec![tracking(), tracking(), tracking(), tracking()],
        )
        .await;

        assert!(matches!(
            outcome,
            ChainAdvanceOutcome::StillPending(ChainSubmissionPending::Tracking { .. })
        ));
        assert_eq!(modes.len(), 3, "the budget bounds the passes");
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(4),
            "each Tracking pass but the last waits pending_repoll"
        );

        let unbounded = ChainAdvancePolicy {
            max_passes: 0,
            ..policy()
        };
        let (outcome, modes) = run(
            &unbounded,
            &control,
            vec![
                tracking(),
                tracking(),
                tracking(),
                tracking(),
                ChainSubmissionResult::Rejected(diagnostic(
                    ChainSubmissionDiagnosticKind::ChainRejected,
                )),
            ],
        )
        .await;
        assert!(matches!(outcome, ChainAdvanceOutcome::Rejected(_)));
        assert_eq!(
            modes.len(),
            5,
            "a zero budget runs until the row leaves Tracking"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_epoch_change_or_cancellation_between_passes_ends_the_episode() {
        let control = ChainSubmissionControl::new(1);
        let bump = {
            let control = control.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                control.set_operation_epoch(2);
            })
        };
        let (outcome, modes) = run(&policy(), &control, vec![tracking(), tracking()]).await;
        bump.await.unwrap();
        assert!(matches!(outcome, ChainAdvanceOutcome::Cancelled));
        assert_eq!(
            modes.len(),
            1,
            "the epoch change during the repoll wait ends the episode before a second pass"
        );

        let stale = ChainSubmissionControl::new(1);
        stale.set_operation_epoch(2);
        let (outcome, modes) = run(&policy(), &stale, vec![tracking()]).await;
        assert!(matches!(outcome, ChainAdvanceOutcome::Cancelled));
        assert!(
            modes.is_empty(),
            "an episode whose entry epoch has already passed runs no pass at all"
        );

        let cancelled = ChainSubmissionControl::new(1);
        cancelled.cancel();
        let (outcome, modes) = run(&policy(), &cancelled, vec![tracking()]).await;
        assert!(matches!(outcome, ChainAdvanceOutcome::Cancelled));
        assert!(modes.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_pass_refused_for_a_stale_epoch_ends_the_episode_as_cancelled() {
        // The epoch changes after the boundary check admits the pass; the
        // coordinator refuses the stale epoch inside it. That is the host
        // moving on, not a failed step.
        let control = ChainSubmissionControl::new(1);
        let passes = Arc::new(Mutex::new(0usize));
        let outcome = run_episode(&policy(), &control, 1, |_mode| {
            *passes.lock().unwrap() += 1;
            control.set_operation_epoch(2);
            async {
                Err(ChainSubmissionFailure::without_state(
                    ChainSubmissionFailureKind::InvalidInput,
                    "host operation epoch changed before chain submission",
                ))
            }
        })
        .await
        .unwrap();
        assert!(matches!(outcome, ChainAdvanceOutcome::Cancelled));
        assert_eq!(*passes.lock().unwrap(), 1);

        // A pass that fails while the host is still on this epoch is a
        // failure the caller must see.
        let steady = ChainSubmissionControl::new(1);
        let error = run_episode(&policy(), &steady, 1, |_mode| async {
            Err(ChainSubmissionFailure::without_state(
                ChainSubmissionFailureKind::Transport,
                "chain unreachable",
            ))
        })
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ChainSubmissionFailureKind::Transport);
    }
}
