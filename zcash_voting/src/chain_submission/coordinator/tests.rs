//! Behavior-oriented conformance tests for one bounded lifecycle pass.

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use incrementalmerkletree::frontier::Frontier;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::Notify;
use vote_commitment_tree::{MerkleHashVote, TREE_DEPTH};

use super::*;
use crate::{
    chain_submission::{
        generation::{ChainSubmissionRequest, ExpectedTreeLayout},
        protocol::ChainProtocolClient,
        store::memory::InMemoryChainSubmissionStore,
        CandidateTransactionHash, ChainHttpRequest, ChainHttpResponse, ChainPostDispatch,
        ChainSubmissionGeneration, ChainSubmissionGenerationDigest, ChainSubmissionIdentity,
        ChainSubmissionPending, ChainSubmissionStateEvidence, ChainSubmissionTarget,
        ChainTransportError, ChainTransportFuture,
    },
    confirmation::{TxEvent, TxEventAttribute},
    types::Network,
    wire::{DelegationSubmissionWire, VoteCommitmentBatchWire, VoteCommitmentWire},
};

const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[derive(Clone)]
struct ManualClock(Arc<AtomicU64>);

impl ManualClock {
    fn new(now: u64) -> Self {
        Self(Arc::new(AtomicU64::new(now)))
    }

    fn set(&self, now: u64) {
        self.0.store(now, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn exact_recovery_confirms_without_a_hash_and_clamps_timestamp() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    seed_hashless_tree_recovery(&store, &StoreAdvancementRequest::vote(identity.clone()));
    let transport = Arc::new(ScriptedTransport::default());
    for response in tree_responses(&[[8; 32], [3; 32], [4; 32], [7; 32]]) {
        transport.queue(Ok(response));
    }
    let clock = ManualClock::new(100);
    transport.move_clock_on_next_get(clock.clone(), 90);
    let coordinator = coordinator(Arc::clone(&transport), Arc::clone(&store), clock, 10);

    let result = coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap();

    let ChainSubmissionResult::Confirmed(confirmation) = result else {
        panic!("exact layout must confirm")
    };
    assert_eq!(
        confirmation.source(),
        super::super::ChainSubmissionConfirmationSource::Tree
    );
    assert_eq!(confirmation.transaction_hash(), None);
    assert_eq!(confirmation.final_van_position(), 1);
    assert_eq!(confirmation.vote_commitment_positions(), &[2]);
    assert_eq!(transport.methods(), vec!["GET", "GET"]);
    let record = store.record(&identity).unwrap();
    assert_eq!(record.committed_post_reservations(), 1);
    assert_eq!(record.updated_at(), 92);
}

#[tokio::test]
async fn failed_tree_confirmation_reports_durable_recovery_and_rolls_back_projection() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    seed_hashless_tree_recovery(&store, &StoreAdvancementRequest::vote(identity.clone()));
    store.fail_next_confirmation();
    let transport = Arc::new(ScriptedTransport::default());
    for response in tree_responses(&[[8; 32], [3; 32], [4; 32], [7; 32]]) {
        transport.queue(Ok(response));
    }

    let failure = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance_with_recovery(
        StoreAdvancementRequest::vote(identity.clone()),
        ChainRecoveryMode::ExactTree,
        &ManualControl::default(),
    )
    .await
    .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Storage);
    assert_eq!(
        failure.strongest_state().unwrap().state(),
        ChainSubmissionState::Recovering
    );
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Recovering
    );
    assert!(store.projection(&identity).is_none());
}

#[tokio::test]
async fn failed_recovery_retry_reservation_reports_durable_recovery_without_redispatch() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    seed_hashless_tree_recovery(&store, &StoreAdvancementRequest::vote(identity.clone()));
    let transport = Arc::new(ScriptedTransport::default());
    let coordinator = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    );
    for response in tree_responses(&[[8; 32]]) {
        transport.queue(Ok(response));
    }
    transport.fail_recovery_reservation_after_tree_page(Arc::clone(&store));
    let failure = coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Storage);
    let strongest_state = failure.strongest_state().unwrap();
    assert_eq!(strongest_state.state(), ChainSubmissionState::Recovering);
    assert_eq!(
        strongest_state.evidence(),
        ChainSubmissionStateEvidence::Durable
    );
    let record = store.record(&identity).unwrap();
    assert_eq!(record.durable_state(), ChainSubmissionState::Recovering);
    assert_eq!(record.committed_post_reservations(), 1);
    assert_eq!(transport.methods(), vec!["GET", "GET"]);
}

#[tokio::test]
async fn fresh_ambiguous_post_exhaustion_stays_recovering_without_scanning() {
    // A fresh POST whose only attempt is ambiguous exhausts the invocation's
    // budget. That is local uncertainty, not chain evidence: the row stays
    // hashless `Recovering`, and this invocation neither scans nor ends it.
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Err(ChainTransportError::possibly_dispatched("timeout")));
    let coordinator = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    );

    let result = coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            ref diagnostic,
        }) if diagnostic.kind() == ChainSubmissionDiagnosticKind::AmbiguousDispatch
    ));
    assert_eq!(transport.methods(), vec!["POST"]);
    let record = store.record(&identity).unwrap();
    assert_eq!(record.durable_state(), ChainSubmissionState::Recovering);
    assert_eq!(record.committed_post_reservations(), 1);
}

#[tokio::test]
async fn ambiguous_exhaustion_stays_recovering_and_a_later_exact_tree_pass_confirms() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Err(ChainTransportError::possibly_dispatched("timeout")));
    let coordinator = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    );
    coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    for response in tree_responses(&[[8; 32], [3; 32], [4; 32], [7; 32]]) {
        transport.queue(Ok(response));
    }
    let result = coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(matches!(result, ChainSubmissionResult::Confirmed(_)));
    assert_eq!(transport.methods(), vec!["POST", "GET", "GET"]);
    let record = store.record(&identity).unwrap();
    assert_eq!(record.durable_state(), ChainSubmissionState::Confirmed);
    assert_eq!(record.committed_post_reservations(), 1);
}

#[tokio::test]
async fn ambiguous_dispatch_row_scans_before_redispatch_under_exact_tree() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    seed_dispatch_ambiguity(&store, &StoreAdvancementRequest::vote(identity.clone()));
    let transport = Arc::new(ScriptedTransport::default());
    for response in tree_responses(&[[8; 32]]) {
        transport.queue(Ok(response));
    }
    transport.queue(Ok(accepted()));

    let result = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance_with_recovery(
        StoreAdvancementRequest::vote(identity.clone()),
        ChainRecoveryMode::ExactTree,
        &ManualControl::default(),
    )
    .await
    .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking { .. })
    ));
    // The no-match pass authorizes the retry; no backoff precedes the first
    // POST of the invocation.
    assert_eq!(transport.methods(), vec!["GET", "GET", "POST"]);
    assert_eq!(
        store
            .record(&identity)
            .unwrap()
            .committed_post_reservations(),
        2
    );
}

#[tokio::test]
async fn ambiguous_dispatch_row_confirms_from_tree_without_redispatch_under_exact_tree() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    seed_dispatch_ambiguity(&store, &StoreAdvancementRequest::vote(identity.clone()));
    let transport = Arc::new(ScriptedTransport::default());
    for response in tree_responses(&[[8; 32], [3; 32], [4; 32], [7; 32]]) {
        transport.queue(Ok(response));
    }

    let result = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance_with_recovery(
        StoreAdvancementRequest::vote(identity.clone()),
        ChainRecoveryMode::ExactTree,
        &ManualControl::default(),
    )
    .await
    .unwrap();

    let ChainSubmissionResult::Confirmed(confirmation) = result else {
        panic!("exact layout must confirm")
    };
    assert_eq!(confirmation.final_van_position(), 1);
    assert_eq!(confirmation.vote_commitment_positions(), &[2]);
    assert_eq!(transport.methods(), vec!["GET", "GET"]);
    assert_eq!(
        store
            .record(&identity)
            .unwrap()
            .committed_post_reservations(),
        1
    );
}

#[tokio::test]
async fn preexisting_tree_recovery_reserves_without_invocation_backoff() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    seed_hashless_tree_recovery(&store, &StoreAdvancementRequest::vote(identity.clone()));
    let transport = Arc::new(ScriptedTransport::default());
    for response in tree_responses(&[[8; 32]]) {
        transport.queue(Ok(response));
    }
    transport.queue(Ok(accepted()));
    let clock = ManualClock::new(100);
    transport.move_clock_on_next_get(clock.clone(), 90);
    transport.require_recovery_reservation_timestamp(Arc::clone(&store), identity.clone(), 92);
    let protocol = ChainProtocolClient::new(
        Arc::clone(&transport),
        Network::Testnet,
        &[
            "https://one.example".to_string(),
            "https://two.example".to_string(),
        ],
    )
    .unwrap();
    let coordinator = Arc::new(
        ChainSubmissionCoordinator::new(
            protocol,
            Arc::clone(&store),
            clock,
            CoordinatorPolicy::new(Duration::from_secs(10), 2, vec![Duration::from_secs(5)])
                .unwrap(),
        )
        .unwrap(),
    );
    let task = {
        let coordinator = Arc::clone(&coordinator);
        let identity = identity.clone();
        tokio::spawn(async move {
            coordinator
                .advance_with_recovery(
                    StoreAdvancementRequest::vote(identity),
                    ChainRecoveryMode::ExactTree,
                    &ManualControl::default(),
                )
                .await
        })
    };

    let result = task.await.unwrap().unwrap();
    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking { .. })
    ));
    assert_eq!(transport.methods(), vec!["GET", "GET", "POST"]);
    assert_eq!(
        store
            .record(&identity)
            .unwrap()
            .committed_post_reservations(),
        2
    );
}

#[tokio::test]
async fn other_recovery_retry_rejection_is_an_error_without_candidate_evidence() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    seed_hashless_tree_recovery(&store, &StoreAdvancementRequest::vote(identity.clone()));
    let transport = Arc::new(ScriptedTransport::default());
    let coordinator = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    );
    for response in tree_responses(&[[8; 32]]) {
        transport.queue(Ok(response));
    }
    transport.queue(Ok(rejected_with_hash()));
    let failure = coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Protocol);
    assert_eq!(
        failure.strongest_state().unwrap().state(),
        ChainSubmissionState::Recovering
    );
    let record = store.record(&identity).unwrap();
    assert_eq!(record.committed_post_reservations(), 2);
    assert!(matches!(
        record.state(),
        SubmissionRecordState::Recovering {
            candidate_transaction_hash: None,
            ..
        }
    ));
    assert!(store.projection(&identity).is_none());
    assert_eq!(transport.methods(), vec!["GET", "GET", "POST"]);
}

#[tokio::test]
async fn nullifier_spent_after_definite_committed_failure_stays_recoverable() {
    // The seed is a candidate that committed unsuccessfully: a definite
    // outcome that spent nothing. A later code 2 is therefore not evidence
    // that this generation was submitted and must not end it hashless.
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    seed_hashless_tree_recovery(&store, &StoreAdvancementRequest::vote(identity.clone()));
    let transport = Arc::new(ScriptedTransport::default());
    for response in tree_responses(&[[8; 32]]) {
        transport.queue(Ok(response));
    }
    transport.queue(Ok(ChainHttpResponse::json(
        422,
        br#"{"code":2,"log":"must not be retained"}"#.to_vec(),
    )));
    let coordinator = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    );

    let failure = coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Protocol);
    // The chain's own words ride along so a rejection can be acted on; the
    // recovery state below is what the test is really pinning.
    assert!(failure.message().contains("must not be retained"));
    assert_eq!(
        failure.strongest_state().unwrap().state(),
        ChainSubmissionState::Recovering
    );
    let record = store.record(&identity).unwrap();
    assert_eq!(record.committed_post_reservations(), 2);
    assert!(matches!(
        record.state(),
        SubmissionRecordState::Recovering {
            candidate_transaction_hash: None,
            ambiguity_diagnostic,
        } if ambiguity_diagnostic.kind() == ChainSubmissionDiagnosticKind::ChainRejected
    ));
    assert_eq!(transport.methods(), vec!["GET", "GET", "POST"]);

    // The row stays recoverable: a later pass scans again and may confirm.
    for response in tree_responses(&[[8; 32]]) {
        transport.queue(Ok(response));
    }
    transport.queue(Ok(accepted()));
    let result = coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking { .. })
    ));
    assert_eq!(
        transport.methods(),
        vec!["GET", "GET", "POST", "GET", "GET", "POST"]
    );
}

#[tokio::test]
async fn nullifier_spent_after_definite_rejection_stays_recoverable() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let request = StoreAdvancementRequest::vote(identity.clone());
    let StoreAdmission::Ready { derived, .. } = store.admit(&request, true, 1, 90).unwrap() else {
        panic!("fresh reservation")
    };
    store
        .classify_post(
            derived.generation(),
            SubmissionObservation::DefiniteRejection(
                ChainSubmissionDiagnostic::from_redacted_message(
                    ChainSubmissionDiagnosticKind::ChainRejected,
                    "vote chain rejected the first attempt with code 7",
                ),
            ),
            91,
        )
        .unwrap();
    let transport = Arc::new(ScriptedTransport::default());
    for response in tree_responses(&[[8; 32]]) {
        transport.queue(Ok(response));
    }
    transport.queue(Ok(ChainHttpResponse::json(
        422,
        br#"{"code":2,"log":"nullifier already spent"}"#.to_vec(),
    )));

    let failure = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance_with_recovery(
        request,
        ChainRecoveryMode::ExactTree,
        &ManualControl::default(),
    )
    .await
    .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Protocol);
    assert_eq!(
        failure.strongest_state().unwrap().state(),
        ChainSubmissionState::Recovering
    );
    let record = store.record(&identity).unwrap();
    assert_eq!(record.durable_state(), ChainSubmissionState::Recovering);
    assert_eq!(record.committed_post_reservations(), 2);
    assert_eq!(
        record.diagnostic().unwrap().kind(),
        ChainSubmissionDiagnosticKind::ChainRejected
    );
    assert_eq!(transport.methods(), vec!["GET", "GET", "POST"]);
}

#[tokio::test]
async fn nullifier_spent_recovery_retry_after_unresolved_dispatch_is_terminal() {
    // An accepted hash whose tracking window expired is unresolved dispatch:
    // the transaction definitely left the wallet and never resolved. Code 2
    // on the authorized retry is then evidence the earlier dispatch landed.
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(accepted()));
    transport.queue(Ok(pending()));
    let clock = ManualClock::new(100);
    let coordinator = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        clock.clone(),
        10,
    );
    coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    clock.set(111);
    transport.queue(Ok(pending()));
    for response in tree_responses(&[[8; 32]]) {
        transport.queue(Ok(response));
    }
    transport.queue(Ok(ChainHttpResponse::json(
        422,
        br#"{"code":2,"log":"must not be retained"}"#.to_vec(),
    )));
    // Code 2 is checked against the tree once more before the generation
    // ends hashless; the pass finds no layout.
    for response in tree_responses(&[[8; 32]]) {
        transport.queue(Ok(response));
    }
    let result = coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::SubmittedWithoutHash(ref diagnostic)
            if diagnostic.kind() == ChainSubmissionDiagnosticKind::NullifierAlreadySpent
                && !diagnostic.message().contains("must not be retained")
    ));
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::SubmittedWithoutHash
    );
    assert!(store.projection(&identity).is_none());
    assert_eq!(
        transport.methods(),
        vec!["POST", "GET", "GET", "GET", "GET", "POST", "GET", "GET"]
    );

    // Terminal: a later pass performs no network work.
    let before = transport.methods();
    coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap();
    assert_eq!(transport.methods(), before);
}

#[tokio::test]
async fn nullifier_spent_recovery_retry_after_unresolved_dispatch_confirms_from_tree() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(accepted()));
    transport.queue(Ok(pending()));
    let clock = ManualClock::new(100);
    let coordinator = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        clock.clone(),
        10,
    );
    coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    clock.set(111);
    transport.queue(Ok(pending()));
    for response in tree_responses(&[[8; 32]]) {
        transport.queue(Ok(response));
    }
    transport.queue(Ok(ChainHttpResponse::json(
        422,
        br#"{"code":2,"log":"nullifier already spent"}"#.to_vec(),
    )));
    // The tree indexed the earlier dispatch between the two passes.
    for response in tree_responses(&[[8; 32], [3; 32], [4; 32], [7; 32]]) {
        transport.queue(Ok(response));
    }
    let result = coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap();

    let ChainSubmissionResult::Confirmed(confirmation) = result else {
        panic!("code 2 with a matching layout must confirm")
    };
    assert_eq!(confirmation.transaction_hash(), None);
    assert_eq!(confirmation.final_van_position(), 1);
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Confirmed
    );
    assert_eq!(
        transport.methods(),
        vec!["POST", "GET", "GET", "GET", "GET", "POST", "GET", "GET"]
    );
}

#[tokio::test]
async fn final_ambiguous_recovery_retry_stays_recovering() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    seed_hashless_tree_recovery(&store, &StoreAdvancementRequest::vote(identity.clone()));
    let transport = Arc::new(ScriptedTransport::default());
    for response in tree_responses(&[[8; 32]]) {
        transport.queue(Ok(response));
    }
    transport.queue(Err(ChainTransportError::possibly_dispatched("timeout")));

    let result = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance_with_recovery(
        StoreAdvancementRequest::vote(identity.clone()),
        ChainRecoveryMode::ExactTree,
        &ManualControl::default(),
    )
    .await
    .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            ref diagnostic,
        }) if diagnostic.kind() == ChainSubmissionDiagnosticKind::AmbiguousDispatch
    ));
    let record = store.record(&identity).unwrap();
    assert_eq!(record.durable_state(), ChainSubmissionState::Recovering);
    assert!(record.state().permits_ambiguous_retry());
    assert_eq!(transport.methods(), vec!["GET", "GET", "POST"]);
}

#[tokio::test]
async fn final_recovery_hash_collision_stays_recovering_until_the_next_scan() {
    let owner_identity = identity(1, 0);
    let recovering_identity = identity(2, 1);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(owner_identity.clone(), 1));
    store.seed_derivation(derived(recovering_identity.clone(), 2));

    let owner_transport = Arc::new(ScriptedTransport::default());
    owner_transport.queue(Ok(accepted()));
    owner_transport.queue(Ok(pending()));
    coordinator(
        owner_transport,
        Arc::clone(&store),
        ManualClock::new(90),
        10,
    )
    .advance(
        StoreAdvancementRequest::vote(owner_identity),
        &ManualControl::default(),
    )
    .await
    .unwrap();

    let request = StoreAdvancementRequest::vote(recovering_identity.clone());
    seed_hashless_tree_recovery(&store, &request);
    let recovery_transport = Arc::new(ScriptedTransport::default());
    for response in tree_responses(&[[8; 32]]) {
        recovery_transport.queue(Ok(response));
    }
    recovery_transport.queue(Ok(accepted()));
    let coordinator = coordinator(
        Arc::clone(&recovery_transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    );

    let result = coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(recovering_identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            ref diagnostic,
        }) if diagnostic.kind() == ChainSubmissionDiagnosticKind::InvalidProtocolResponse
    ));
    assert_eq!(recovery_transport.methods(), vec!["GET", "GET", "POST"]);
    let record = store.record(&recovering_identity).unwrap();
    assert_eq!(record.durable_state(), ChainSubmissionState::Recovering);
    assert_eq!(record.committed_post_reservations(), 2);

    // The next exact-tree pass scans first and only then retries.
    for response in tree_responses(&[[8; 32]]) {
        recovery_transport.queue(Ok(response));
    }
    recovery_transport.queue(Ok(accepted_with_hash(&"12".repeat(32))));
    recovery_transport.queue(Ok(pending()));
    let result = coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(recovering_identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking { .. })
    ));
    assert_eq!(
        recovery_transport.methods(),
        vec!["GET", "GET", "POST", "GET", "GET", "POST"]
    );
}

#[tokio::test]
async fn ambiguous_retry_reservation_failure_reports_durable_recovery() {
    let identity = identity(1, 0);
    let request = StoreAdvancementRequest::vote(identity.clone());
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let StoreAdmission::Ready { derived, .. } = store.admit(&request, true, 1, 90).unwrap() else {
        panic!("fresh reservation")
    };
    store
        .classify_post(
            derived.generation(),
            SubmissionObservation::PossiblyDispatched(
                ChainSubmissionDiagnostic::from_redacted_message(
                    ChainSubmissionDiagnosticKind::AmbiguousDispatch,
                    "response unavailable",
                ),
            ),
            91,
        )
        .unwrap();
    store.fail_next_ambiguous_retry();
    let transport = Arc::new(ScriptedTransport::default());

    let failure = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance(request, &ManualControl::default())
    .await
    .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Storage);
    let strongest_state = failure.strongest_state().unwrap();
    assert_eq!(strongest_state.state(), ChainSubmissionState::Recovering);
    assert_eq!(
        strongest_state.evidence(),
        ChainSubmissionStateEvidence::Durable
    );
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Recovering
    );
    assert!(transport.methods().is_empty());
}

#[tokio::test]
async fn ambiguous_retry_clock_failure_reports_the_durable_recovery_state() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Err(ChainTransportError::possibly_dispatched("timeout")));
    let protocol = ChainProtocolClient::new(
        Arc::clone(&transport),
        Network::Testnet,
        &["https://chain.example".to_string()],
    )
    .unwrap();
    // Reads: admission, ambiguity classification, then the retry reservation
    // that must fail after the ambiguity is already durable.
    let coordinator = ChainSubmissionCoordinator::new(
        protocol,
        Arc::clone(&store),
        FailingClock::failing_after(100, 2),
        CoordinatorPolicy::new(Duration::from_secs(10), 2, vec![Duration::from_millis(1)]).unwrap(),
    )
    .unwrap();

    let failure = coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Storage);
    let strongest_state = failure.strongest_state().unwrap();
    assert_eq!(strongest_state.state(), ChainSubmissionState::Recovering);
    assert_eq!(
        strongest_state.evidence(),
        ChainSubmissionStateEvidence::Durable
    );
    assert_eq!(transport.methods(), vec!["POST"]);
    let record = store.record(&identity).unwrap();
    assert_eq!(record.durable_state(), ChainSubmissionState::Recovering);
    assert_eq!(record.committed_post_reservations(), 1);
}

#[tokio::test]
async fn atomic_vote_batch_uses_shared_lifecycle_and_confirms_ordered_positions() {
    let identity = batch_identity(0);
    let proposals = vec![1, 2, 5];
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived_batch(identity.clone(), proposals.clone()));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(accepted_batch(&identity)));
    transport.queue(Ok(batch_confirmed(&identity, &proposals)));

    let result = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance(
        StoreAdvancementRequest::vote_batch(identity.clone(), proposals).unwrap(),
        &ManualControl::default(),
    )
    .await
    .unwrap();

    let ChainSubmissionResult::Confirmed(confirmation) = result else {
        panic!("batch must confirm atomically")
    };
    assert_eq!(confirmation.final_van_position(), 7);
    assert_eq!(confirmation.vote_commitment_positions(), &[8, 9, 10]);
    assert_eq!(transport.methods(), vec!["POST", "GET"]);
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Confirmed
    );
}

#[tokio::test]
async fn atomic_vote_batches_from_one_through_protocol_maximum_confirm() {
    for size in 1..=crate::vote::MAX_VOTE_BATCH_ACTIONS {
        let identity = batch_identity(size as u32);
        let proposals = (1..=size as u32).collect::<Vec<_>>();
        let store = Arc::new(InMemoryChainSubmissionStore::default());
        store.seed_derivation(derived_batch(identity.clone(), proposals.clone()));
        let transport = Arc::new(ScriptedTransport::default());
        transport.queue(Ok(accepted_batch(&identity)));
        transport.queue(Ok(batch_confirmed(&identity, &proposals)));

        let result = coordinator(transport, store, ManualClock::new(100), 10)
            .advance(
                StoreAdvancementRequest::vote_batch(identity, proposals).unwrap(),
                &ManualControl::default(),
            )
            .await
            .unwrap();
        assert!(matches!(result, ChainSubmissionResult::Confirmed(_)));
    }
}

fn combined_identity(bundle_index: u32) -> ChainSubmissionIdentity {
    ChainSubmissionIdentity::new(
        "wallet",
        Network::Testnet,
        [1; 32],
        bundle_index,
        ChainSubmissionTarget::DelegateAndVoteBatch {
            ordered_batch_digest: [0xcd; 32],
        },
    )
    .unwrap()
}

/// A combined envelope over the same cast effects as `derived_batch`, so the
/// tree layout it expects is identical: the delegation's initial VAN is never
/// a leaf, only the final successor VAN and the vote commitments are.
fn derived_combined_batch(
    identity: ChainSubmissionIdentity,
    ordered_proposal_ids: Vec<u32>,
) -> DerivedChainSubmission {
    let ChainSubmissionRequest::VoteBatch(batch) =
        derived_batch(identity.clone(), ordered_proposal_ids.clone())
            .request()
            .clone()
    else {
        unreachable!("batch derivation carries a batch request")
    };
    let delegation = crate::wire::DelegationSubmissionWire {
        rk: BASE64_STANDARD.encode([0x31; 32]),
        spend_auth_sig: BASE64_STANDARD.encode([0x32; 64]),
        tx1_effects: BASE64_STANDARD.encode([0x33; 32]),
        nf_signed: BASE64_STANDARD.encode([0x34; 32]),
        cmx_new: BASE64_STANDARD.encode([0x35; 32]),
        gov_comm: BASE64_STANDARD.encode([0x09; 32]),
        gov_nullifiers: vec![BASE64_STANDARD.encode([0x36; 32])],
        proof: BASE64_STANDARD.encode([0x37; 96]),
        vote_round_id: BASE64_STANDARD.encode(identity.vote_round_id()),
    };
    DerivedChainSubmission::new(
        ChainSubmissionGeneration::new(
            identity,
            ChainSubmissionGenerationDigest::from_bytes([0xce; 32]),
        ),
        ChainSubmissionRequest::DelegateAndVoteBatch(crate::wire::DelegateAndVoteBatchWire {
            delegation,
            batch,
        }),
        ExpectedTreeLayout::VoteBatch {
            final_successor_van: [3; 32],
            vote_commitments: vec![[4; 32]; ordered_proposal_ids.len()],
        },
        ordered_proposal_ids,
    )
}

#[tokio::test]
async fn exact_recovery_confirms_a_combined_batch_from_the_tree() {
    // A combined generation that lost its hash confirms from the tree with
    // the same final-VAN-plus-vote-leaves layout as an ordinary batch. No
    // initial-VAN leaf is looked for, and no transaction hash is invented.
    let identity = combined_identity(0);
    let proposals = vec![1, 2, 5];
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived_combined_batch(identity.clone(), proposals.clone()));
    let request = StoreAdvancementRequest::vote_batch(identity.clone(), proposals.clone()).unwrap();
    seed_hashless_tree_recovery(&store, &request);
    let transport = Arc::new(ScriptedTransport::default());
    for response in tree_responses(&[[8; 32], [3; 32], [4; 32], [4; 32], [4; 32], [7; 32]]) {
        transport.queue(Ok(response));
    }

    let result = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance_with_recovery(
        request,
        ChainRecoveryMode::ExactTree,
        &ManualControl::default(),
    )
    .await
    .unwrap();

    let ChainSubmissionResult::Confirmed(confirmation) = result else {
        panic!("complete combined layout must confirm: {result:?}")
    };
    assert_eq!(
        confirmation.source(),
        super::super::ChainSubmissionConfirmationSource::Tree
    );
    assert!(confirmation.transaction_hash().is_none());
    assert_eq!(confirmation.final_van_position(), 1);
    assert_eq!(confirmation.vote_commitment_positions(), &[2, 3, 4]);
    assert_eq!(transport.methods(), vec!["GET", "GET"]);
    assert!(matches!(
        store.record(&identity).unwrap().state(),
        SubmissionRecordState::Confirmed(_)
    ));
}

#[tokio::test]
async fn exact_recovery_confirms_complete_ordered_batch_layout() {
    let identity = batch_identity(0);
    let proposals = vec![1, 2, 5];
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived_batch(identity.clone(), proposals.clone()));
    let request = StoreAdvancementRequest::vote_batch(identity.clone(), proposals.clone()).unwrap();
    seed_hashless_tree_recovery(&store, &request);
    let transport = Arc::new(ScriptedTransport::default());
    for response in tree_responses(&[[8; 32], [3; 32], [4; 32], [4; 32], [4; 32], [7; 32]]) {
        transport.queue(Ok(response));
    }

    let result = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance_with_recovery(
        request,
        ChainRecoveryMode::ExactTree,
        &ManualControl::default(),
    )
    .await
    .unwrap();

    let ChainSubmissionResult::Confirmed(confirmation) = result else {
        panic!("complete ordered batch layout must confirm")
    };
    assert_eq!(
        confirmation.source(),
        super::super::ChainSubmissionConfirmationSource::Tree
    );
    assert_eq!(confirmation.final_van_position(), 1);
    assert_eq!(confirmation.vote_commitment_positions(), &[2, 3, 4]);
    assert_eq!(transport.methods(), vec!["GET", "GET"]);
}

#[tokio::test]
async fn reordered_batch_confirmation_leaves_tracking_authoritative() {
    let identity = batch_identity(0);
    let proposals = vec![1, 2, 5];
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived_batch(identity.clone(), proposals.clone()));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(accepted_batch(&identity)));
    transport.queue(Ok(batch_confirmed(&identity, &[1, 5, 2])));

    let failure = coordinator(transport, Arc::clone(&store), ManualClock::new(100), 10)
        .advance(
            StoreAdvancementRequest::vote_batch(identity.clone(), proposals).unwrap(),
            &ManualControl::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Protocol);
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Tracking
    );
    assert!(store.projection(&identity).is_none());
}

#[tokio::test(start_paused = true)]
async fn partial_nonadjacent_batch_tree_members_authorize_retry_without_confirmation() {
    let identity = batch_identity(0);
    let proposals = vec![1, 2, 5];
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived_batch(identity.clone(), proposals.clone()));
    let request = StoreAdvancementRequest::vote_batch(identity.clone(), proposals.clone()).unwrap();
    seed_hashless_tree_recovery(&store, &request);
    let transport = Arc::new(ScriptedTransport::default());
    for response in tree_responses(&[[3; 32], [4; 32], [8; 32], [4; 32], [4; 32]]) {
        transport.queue(Ok(response));
    }
    transport.queue(Ok(accepted_batch(&identity)));

    let protocol = ChainProtocolClient::new(
        Arc::clone(&transport),
        Network::Testnet,
        &[
            "https://one.example".to_string(),
            "https://two.example".to_string(),
        ],
    )
    .unwrap();
    let coordinator = ChainSubmissionCoordinator::new(
        protocol,
        Arc::clone(&store),
        ManualClock::new(100),
        CoordinatorPolicy::new(Duration::from_secs(10), 2, vec![Duration::from_secs(1)]).unwrap(),
    )
    .unwrap();

    let result = coordinator
        .advance_with_recovery(
            request,
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking { .. })
    ));
    assert_eq!(
        store
            .record(&identity)
            .unwrap()
            .committed_post_reservations(),
        2
    );
    assert!(store.projection(&identity).is_none());
}

#[tokio::test]
async fn recovering_candidate_is_polled_before_no_match_retry_reservation() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(accepted()));
    transport.queue(Ok(pending()));
    let clock = ManualClock::new(100);
    let coordinator = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        clock.clone(),
        10,
    );
    coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    clock.set(110);
    transport.queue(Ok(pending()));
    for response in tree_responses(&[[8; 32]]) {
        transport.queue(Ok(response));
    }
    transport.queue(Ok(accepted()));
    let result = coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking { .. })
    ));
    assert_eq!(
        transport.methods(),
        vec!["POST", "GET", "GET", "GET", "GET", "POST"]
    );
    let record = store.record(&identity).unwrap();
    assert_eq!(record.durable_state(), ChainSubmissionState::Tracking);
    assert_eq!(record.committed_post_reservations(), 2);
}

#[tokio::test]
async fn definitely_unsent_recovery_retry_keeps_reservation_and_requires_a_new_scan() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    seed_hashless_tree_recovery(&store, &StoreAdvancementRequest::vote(identity.clone()));
    let transport = Arc::new(ScriptedTransport::default());
    for response in tree_responses(&[[8; 32]]) {
        transport.queue(Ok(response));
    }
    transport.queue(Err(ChainTransportError::definitely_unsent("refused")));
    let coordinator = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(101),
        10,
    );
    let failure = coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Transport);
    let record = store.record(&identity).unwrap();
    assert_eq!(record.committed_post_reservations(), 2);
    assert!(matches!(
        record.state(),
        SubmissionRecordState::Recovering {
            candidate_transaction_hash: None,
            ..
        }
    ));

    let before = transport.methods();
    coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();
    assert_eq!(transport.methods(), before);
    assert_eq!(
        store
            .record(&identity)
            .unwrap()
            .committed_post_reservations(),
        2
    );
}

#[tokio::test]
async fn malformed_tree_after_candidate_first_poll_retains_candidate_and_never_retries() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(accepted()));
    transport.queue(Ok(pending()));
    let clock = ManualClock::new(100);
    let coordinator = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        clock.clone(),
        10,
    );
    coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    clock.set(110);
    transport.queue(Ok(pending()));
    transport.queue(Ok(ChainHttpResponse::json(
        200,
        br#"{"tree":{"next_index":1,"root":"not-base64","height":1}}"#.to_vec(),
    )));
    let failure = coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Protocol);
    assert_eq!(transport.methods(), vec!["POST", "GET", "GET", "GET"]);
    let record = store.record(&identity).unwrap();
    assert_eq!(record.committed_post_reservations(), 1);
    assert!(matches!(
        record.state(),
        SubmissionRecordState::Recovering {
            candidate_transaction_hash: Some(_),
            ..
        }
    ));
}

impl ChainSubmissionClock for ManualClock {
    fn now_seconds(&self) -> Result<u64, ChainSubmissionFailure> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

/// Manual clock whose read fails once a configured number of reads succeeded.
struct FailingClock {
    inner: ManualClock,
    successful_reads_remaining: AtomicUsize,
}

impl FailingClock {
    fn failing_after(now: u64, successful_reads: usize) -> Self {
        Self {
            inner: ManualClock::new(now),
            successful_reads_remaining: AtomicUsize::new(successful_reads),
        }
    }
}

impl ChainSubmissionClock for FailingClock {
    fn now_seconds(&self) -> Result<u64, ChainSubmissionFailure> {
        let remaining = self.successful_reads_remaining.load(Ordering::SeqCst);
        if remaining == 0 {
            return Err(ChainSubmissionFailure::without_state(
                ChainSubmissionFailureKind::Storage,
                "system clock is before the Unix epoch",
            ));
        }
        self.successful_reads_remaining
            .store(remaining - 1, Ordering::SeqCst);
        self.inner.now_seconds()
    }
}

#[derive(Default)]
struct ManualControl {
    cancelled: AtomicBool,
    epoch: AtomicU64,
}

impl SubmissionControl for ManualControl {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn operation_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
}

struct CancelOnCheck {
    checks: AtomicUsize,
    cancel_at: usize,
}

impl CancelOnCheck {
    fn new(cancel_at: usize) -> Self {
        Self {
            checks: AtomicUsize::new(0),
            cancel_at,
        }
    }
}

impl SubmissionControl for CancelOnCheck {
    fn is_cancelled(&self) -> bool {
        self.checks.fetch_add(1, Ordering::SeqCst) + 1 >= self.cancel_at
    }

    fn operation_epoch(&self) -> u64 {
        0
    }
}

type TransportReply = Result<ChainHttpResponse, ChainTransportError>;
type RecoveryReservationTimestampProbe = (
    Arc<InMemoryChainSubmissionStore>,
    ChainSubmissionIdentity,
    u64,
);

#[derive(Clone)]
struct PostGate {
    armed: Arc<AtomicBool>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[derive(Default)]
struct ScriptedTransport {
    replies: Mutex<VecDeque<TransportReply>>,
    methods: Mutex<Vec<&'static str>>,
    post_urls: Mutex<Vec<String>>,
    reservation_probe: Mutex<Option<(Arc<InMemoryChainSubmissionStore>, ChainSubmissionIdentity)>>,
    recovery_reservation_timestamp_probe: Mutex<Option<RecoveryReservationTimestampProbe>>,
    next_get_clock_update: Mutex<Option<(ManualClock, u64)>>,
    post_gate: Mutex<Option<PostGate>>,
    fail_classification_store: Mutex<Option<Arc<InMemoryChainSubmissionStore>>>,
    fail_reconciliation_store: Mutex<Option<Arc<InMemoryChainSubmissionStore>>>,
    fail_recovery_reservation_store: Mutex<Option<Arc<InMemoryChainSubmissionStore>>>,
}

impl ScriptedTransport {
    fn queue(&self, reply: TransportReply) {
        self.replies.lock().unwrap().push_back(reply);
    }

    fn methods(&self) -> Vec<&'static str> {
        self.methods.lock().unwrap().clone()
    }

    fn post_urls(&self) -> Vec<String> {
        self.post_urls.lock().unwrap().clone()
    }

    fn require_reservation_before_post(
        &self,
        store: Arc<InMemoryChainSubmissionStore>,
        identity: ChainSubmissionIdentity,
    ) {
        *self.reservation_probe.lock().unwrap() = Some((store, identity));
    }

    fn require_recovery_reservation_timestamp(
        &self,
        store: Arc<InMemoryChainSubmissionStore>,
        identity: ChainSubmissionIdentity,
        expected_updated_at: u64,
    ) {
        *self.recovery_reservation_timestamp_probe.lock().unwrap() =
            Some((store, identity, expected_updated_at));
    }

    fn move_clock_on_next_get(&self, clock: ManualClock, now: u64) {
        *self.next_get_clock_update.lock().unwrap() = Some((clock, now));
    }

    fn gate_first_post(&self) -> (Arc<Notify>, Arc<Notify>) {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        *self.post_gate.lock().unwrap() = Some(PostGate {
            armed: Arc::new(AtomicBool::new(true)),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        });
        (entered, release)
    }

    fn fail_classification_after_post(&self, store: Arc<InMemoryChainSubmissionStore>) {
        *self.fail_classification_store.lock().unwrap() = Some(store);
    }

    fn fail_commit_after_lookup(&self, store: Arc<InMemoryChainSubmissionStore>) {
        *self.fail_reconciliation_store.lock().unwrap() = Some(store);
    }

    fn fail_recovery_reservation_after_tree_page(&self, store: Arc<InMemoryChainSubmissionStore>) {
        *self.fail_recovery_reservation_store.lock().unwrap() = Some(store);
    }

    fn begin_request(&self, method: &'static str) {
        if method == "POST" {
            if let Some((store, identity)) = self.reservation_probe.lock().unwrap().as_ref() {
                assert!(matches!(
                    store.record(identity).unwrap().state(),
                    SubmissionRecordState::Submitting
                ));
            }
            if let Some((store, identity, expected_updated_at)) = self
                .recovery_reservation_timestamp_probe
                .lock()
                .unwrap()
                .as_ref()
            {
                let record = store.record(identity).unwrap();
                if record.committed_post_reservations() == 2 {
                    assert_eq!(record.updated_at(), *expected_updated_at);
                }
            }
        }
        self.methods.lock().unwrap().push(method);
    }

    fn take_reply(&self) -> TransportReply {
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted chain response")
    }
}

impl ChainTransport for ScriptedTransport {
    fn chain_get<'a>(&'a self, request: ChainHttpRequest) -> ChainTransportFuture<'a> {
        Box::pin(async move {
            self.begin_request("GET");
            if let Some((clock, now)) = self.next_get_clock_update.lock().unwrap().take() {
                clock.set(now);
            }
            if let Some(store) = self.fail_reconciliation_store.lock().unwrap().take() {
                store.fail_next_commit();
            }
            if request.url().contains("/leaves") {
                if let Some(store) = self.fail_recovery_reservation_store.lock().unwrap().take() {
                    store.fail_next_commit_without_state();
                }
            }
            self.take_reply()
        })
    }

    fn chain_post_json<'a>(
        &'a self,
        request: ChainHttpRequest,
        _json: Vec<u8>,
    ) -> ChainTransportFuture<'a> {
        Box::pin(async move {
            self.post_urls
                .lock()
                .unwrap()
                .push(request.url().to_string());
            self.begin_request("POST");
            let gate = self.post_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                if gate.armed.swap(false, Ordering::SeqCst) {
                    gate.entered.notify_one();
                    gate.release.notified().await;
                }
            }
            if let Some(store) = self.fail_classification_store.lock().unwrap().take() {
                store.fail_next_commit();
            }
            self.take_reply()
        })
    }
}

fn identity(proposal_id: u32, bundle_index: u32) -> ChainSubmissionIdentity {
    ChainSubmissionIdentity::new(
        "wallet",
        Network::Testnet,
        [1; 32],
        bundle_index,
        ChainSubmissionTarget::Vote { proposal_id },
    )
    .unwrap()
}

fn batch_identity(bundle_index: u32) -> ChainSubmissionIdentity {
    ChainSubmissionIdentity::new(
        "wallet",
        Network::Testnet,
        [1; 32],
        bundle_index,
        ChainSubmissionTarget::VoteBatch {
            ordered_batch_digest: [9; 32],
        },
    )
    .unwrap()
}

fn delegation_identity(bundle_index: u32) -> ChainSubmissionIdentity {
    ChainSubmissionIdentity::new(
        "wallet",
        Network::Testnet,
        [1; 32],
        bundle_index,
        ChainSubmissionTarget::Delegation,
    )
    .unwrap()
}

fn derived(identity: ChainSubmissionIdentity, digest_byte: u8) -> DerivedChainSubmission {
    let generation = ChainSubmissionGeneration::new(
        identity.clone(),
        ChainSubmissionGenerationDigest::from_bytes([digest_byte; 32]),
    );
    DerivedChainSubmission::new(
        generation,
        ChainSubmissionRequest::Vote(VoteCommitmentWire {
            van_nullifier: hex::encode([2; 32]),
            vote_authority_note_new: hex::encode([3; 32]),
            vote_commitment: hex::encode([4; 32]),
            proposal_id: match identity.target() {
                ChainSubmissionTarget::Vote { proposal_id } => proposal_id,
                _ => unreachable!(),
            },
            proof: hex::encode([5; 8]),
            vote_round_id: hex::encode(identity.vote_round_id()),
            anchor_height: 1,
            r_vpk: hex::encode([6; 32]),
            vote_auth_sig: hex::encode([7; 64]),
        }),
        ExpectedTreeLayout::Vote {
            successor_van: [3; 32],
            vote_commitment: [4; 32],
        },
        vec![match identity.target() {
            ChainSubmissionTarget::Vote { proposal_id } => proposal_id,
            _ => unreachable!(),
        }],
    )
}

fn derived_delegation(
    identity: ChainSubmissionIdentity,
    digest_byte: u8,
) -> DerivedChainSubmission {
    DerivedChainSubmission::new(
        ChainSubmissionGeneration::new(
            identity.clone(),
            ChainSubmissionGenerationDigest::from_bytes([digest_byte; 32]),
        ),
        ChainSubmissionRequest::Delegation(DelegationSubmissionWire {
            rk: "rk".to_string(),
            spend_auth_sig: "signature".to_string(),
            tx1_effects: "effects".to_string(),
            nf_signed: "nullifier".to_string(),
            cmx_new: "cmx".to_string(),
            gov_comm: "van".to_string(),
            gov_nullifiers: vec!["governance-nullifier".to_string()],
            proof: "proof".to_string(),
            vote_round_id: hex::encode(identity.vote_round_id()),
        }),
        ExpectedTreeLayout::Delegation {
            delegation_van: [4; 32],
        },
        vec![],
    )
}

fn derived_batch(
    identity: ChainSubmissionIdentity,
    ordered_proposal_ids: Vec<u32>,
) -> DerivedChainSubmission {
    let votes = ordered_proposal_ids
        .iter()
        .map(|proposal_id| VoteCommitmentWire {
            van_nullifier: BASE64_STANDARD.encode([2; 32]),
            vote_authority_note_new: BASE64_STANDARD.encode([3; 32]),
            vote_commitment: BASE64_STANDARD.encode([4; 32]),
            proposal_id: *proposal_id,
            proof: hex::encode([5; 8]),
            vote_round_id: hex::encode(identity.vote_round_id()),
            anchor_height: 1,
            r_vpk: BASE64_STANDARD.encode([6; 32]),
            vote_auth_sig: BASE64_STANDARD.encode([7; 64]),
        })
        .collect();
    DerivedChainSubmission::new(
        ChainSubmissionGeneration::new(
            identity.clone(),
            ChainSubmissionGenerationDigest::from_bytes([9; 32]),
        ),
        ChainSubmissionRequest::VoteBatch(VoteCommitmentBatchWire { votes }),
        ExpectedTreeLayout::VoteBatch {
            final_successor_van: [3; 32],
            vote_commitments: vec![[4; 32]; ordered_proposal_ids.len()],
        },
        ordered_proposal_ids,
    )
}

fn accepted_with_hash(hash: &str) -> ChainHttpResponse {
    ChainHttpResponse::json(
        200,
        format!(r#"{{"tx_hash":"{hash}","code":0,"log":""}}"#).into_bytes(),
    )
}

fn accepted() -> ChainHttpResponse {
    accepted_with_hash(HASH)
}

fn accepted_batch(identity: &ChainSubmissionIdentity) -> ChainHttpResponse {
    let ChainSubmissionTarget::VoteBatch {
        ordered_batch_digest,
    } = identity.target()
    else {
        panic!("batch response requires a vote-batch identity")
    };
    ChainHttpResponse::json(
        200,
        format!(
            r#"{{"tx_hash":"{HASH}","code":0,"log":"","batch_digest":"{}"}}"#,
            hex::encode(ordered_batch_digest)
        )
        .into_bytes(),
    )
}

fn pending() -> ChainHttpResponse {
    ChainHttpResponse::json(404, br#"{"error":"tx not found"}"#.to_vec())
}

fn tree_responses(leaves: &[[u8; 32]]) -> Vec<ChainHttpResponse> {
    if leaves.is_empty() {
        return vec![ChainHttpResponse::json(
            200,
            br#"{"tree":{"next_index":0,"height":0}}"#.to_vec(),
        )];
    }
    let mut frontier: Frontier<MerkleHashVote, { TREE_DEPTH as u8 }> = Frontier::empty();
    for leaf in leaves {
        assert!(frontier.append(MerkleHashVote::from_bytes(leaf).unwrap()));
    }
    let root = BASE64_STANDARD.encode(frontier.root().to_bytes());
    let encoded = leaves
        .iter()
        .map(|leaf| BASE64_STANDARD.encode(leaf))
        .collect::<Vec<_>>();
    vec![
        ChainHttpResponse::json(
            200,
            serde_json::to_vec(&serde_json::json!({
                "tree": { "next_index": leaves.len(), "root": root, "height": 1 }
            }))
            .unwrap(),
        ),
        ChainHttpResponse::json(
            200,
            serde_json::to_vec(&serde_json::json!({
                "blocks": [{
                    "height": 1, "start_index": 0, "leaves": encoded, "root": root
                }],
                "next_from_height": 0
            }))
            .unwrap(),
        ),
    ]
}

fn rejected() -> ChainHttpResponse {
    ChainHttpResponse::json(
        422,
        br#"{"tx_hash":null,"code":17,"log":"sensitive server log"}"#.to_vec(),
    )
}

fn rejected_with_hash() -> ChainHttpResponse {
    ChainHttpResponse::json(
        422,
        format!(r#"{{"tx_hash":"{HASH}","code":17,"log":"sensitive server log"}}"#).into_bytes(),
    )
}

fn confirmed(identity: &ChainSubmissionIdentity) -> ChainHttpResponse {
    let event = TxEvent {
        event_type: "cast_vote".to_string(),
        attributes: vec![
            TxEventAttribute {
                key: "vote_round_id".to_string(),
                value: hex::encode(identity.vote_round_id()),
            },
            TxEventAttribute {
                key: "leaf_index".to_string(),
                value: "7,8".to_string(),
            },
        ],
    };
    ChainHttpResponse::json(
        200,
        serde_json::to_vec(&serde_json::json!({
            "height": "9",
            "code": 0,
            "log": "",
            "events": [event],
        }))
        .unwrap(),
    )
}

fn batch_confirmed(
    identity: &ChainSubmissionIdentity,
    ordered_proposal_ids: &[u32],
) -> ChainHttpResponse {
    let proposal_ids = ordered_proposal_ids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let positions = (0..ordered_proposal_ids.len())
        .map(|index| (8 + index as u64).to_string())
        .collect::<Vec<_>>()
        .join(",");
    let nullifiers = std::iter::repeat_n(hex::encode([2; 32]), ordered_proposal_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let event = TxEvent {
        event_type: "cast_vote_batch".to_string(),
        attributes: vec![
            TxEventAttribute {
                key: "vote_round_id".to_string(),
                value: hex::encode(identity.vote_round_id()),
            },
            TxEventAttribute {
                key: "batch_digest".to_string(),
                value: hex::encode([9; 32]),
            },
            TxEventAttribute {
                key: "batch_size".to_string(),
                value: ordered_proposal_ids.len().to_string(),
            },
            TxEventAttribute {
                key: "final_van_leaf_index".to_string(),
                value: "7".to_string(),
            },
            TxEventAttribute {
                key: "vote_commitment_leaf_indices".to_string(),
                value: positions,
            },
            TxEventAttribute {
                key: "proposal_ids".to_string(),
                value: proposal_ids,
            },
            TxEventAttribute {
                key: "van_nullifiers".to_string(),
                value: nullifiers,
            },
        ],
    };
    ChainHttpResponse::json(
        200,
        serde_json::to_vec(&serde_json::json!({
            "height": "9", "code": 0, "log": "", "events": [event]
        }))
        .unwrap(),
    )
}

fn delegation_confirmed(identity: &ChainSubmissionIdentity) -> ChainHttpResponse {
    let event = TxEvent {
        event_type: "delegate_vote".to_string(),
        attributes: vec![
            TxEventAttribute {
                key: "vote_round_id".to_string(),
                value: hex::encode(identity.vote_round_id()),
            },
            TxEventAttribute {
                key: "leaf_index".to_string(),
                value: "7".to_string(),
            },
        ],
    };
    ChainHttpResponse::json(
        200,
        serde_json::to_vec(&serde_json::json!({
            "height": "9",
            "code": 0,
            "log": "",
            "events": [event],
        }))
        .unwrap(),
    )
}

fn coordinator_over(
    transport: Arc<ScriptedTransport>,
    store: Arc<InMemoryChainSubmissionStore>,
    clock: ManualClock,
    tracking_window_seconds: u64,
) -> ChainSubmissionCoordinator<Arc<ScriptedTransport>, InMemoryChainSubmissionStore, ManualClock> {
    coordinator(transport, store, clock, tracking_window_seconds)
}

fn coordinator(
    transport: Arc<ScriptedTransport>,
    store: Arc<InMemoryChainSubmissionStore>,
    clock: ManualClock,
    tracking_window_seconds: u64,
) -> ChainSubmissionCoordinator<Arc<ScriptedTransport>, InMemoryChainSubmissionStore, ManualClock> {
    let protocol = ChainProtocolClient::new(
        transport,
        Network::Testnet,
        &["https://chain.example".to_string()],
    )
    .unwrap();
    ChainSubmissionCoordinator::new(
        protocol,
        store,
        clock,
        CoordinatorPolicy::new(Duration::from_secs(tracking_window_seconds), 1, vec![]).unwrap(),
    )
    .unwrap()
}

fn seed_hashless_tree_recovery(
    store: &InMemoryChainSubmissionStore,
    request: &StoreAdvancementRequest,
) {
    let StoreAdmission::Ready { derived, .. } = store.admit(request, true, 1, 90).unwrap() else {
        panic!("fresh recovery fixture reservation")
    };
    let generation = derived.generation().clone();
    store
        .classify_post(
            &generation,
            SubmissionObservation::UsableCandidateHash(CandidateTransactionHash::from_bytes(
                [0x55; 32],
            )),
            91,
        )
        .unwrap();
    let diagnostic = ChainSubmissionDiagnostic::from_redacted_message(
        ChainSubmissionDiagnosticKind::ChainRejected,
        "fixture candidate committed unsuccessfully",
    );
    store
        .reconcile(
            &generation,
            SubmissionObservation::CandidateCommittedFailure(diagnostic.clone()),
            Some(diagnostic),
            92,
        )
        .unwrap();
}

/// Leaves `request`'s row hashless `Recovering` with an `AmbiguousDispatch`
/// diagnostic: one reserved POST whose response was lost.
fn seed_dispatch_ambiguity(
    store: &InMemoryChainSubmissionStore,
    request: &StoreAdvancementRequest,
) {
    let StoreAdmission::Ready { derived, .. } = store.admit(request, true, 1, 90).unwrap() else {
        panic!("fresh ambiguity fixture reservation")
    };
    store
        .classify_post(
            derived.generation(),
            SubmissionObservation::PossiblyDispatched(
                ChainSubmissionDiagnostic::from_redacted_message(
                    ChainSubmissionDiagnosticKind::AmbiguousDispatch,
                    "fixture response was lost after dispatch",
                ),
            ),
            91,
        )
        .unwrap();
}

#[tokio::test]
async fn reservation_commits_before_post_and_accepted_hash_is_tracking() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.require_reservation_before_post(Arc::clone(&store), identity.clone());
    transport.queue(Ok(accepted()));
    transport.queue(Ok(pending()));
    let clock = ManualClock::new(100);
    let coordinator = coordinator(Arc::clone(&transport), Arc::clone(&store), clock, 10);

    let result = coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking { .. })
    ));
    assert_eq!(transport.methods(), vec!["POST", "GET"]);
    let record = store.record(&identity).unwrap();
    assert_eq!(record.tracking_started_at(), Some(100));
    assert_eq!(record.committed_post_reservations(), 1);
}

#[tokio::test]
async fn delegation_uses_the_same_lifecycle_and_atomic_confirmation_path() {
    let identity = delegation_identity(0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived_delegation(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(accepted()));
    transport.queue(Ok(delegation_confirmed(&identity)));

    let result = coordinator(transport, Arc::clone(&store), ManualClock::new(100), 10)
        .advance(
            StoreAdvancementRequest::delegation(identity.clone(), [7; 64]),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(matches!(result, ChainSubmissionResult::Confirmed(_)));
    let projection = store.projection(&identity).unwrap();
    assert_eq!(projection.final_van_position(), 7);
    assert!(projection.vote_commitment_positions().is_empty());
}

#[tokio::test]
async fn single_ambiguous_attempt_stays_recovering_and_retries_on_the_next_advance() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Err(ChainTransportError::possibly_dispatched("timeout")));
    transport.queue(Ok(accepted()));
    transport.queue(Ok(pending()));
    let coordinator = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    );

    let first = coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        first,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            ref diagnostic,
        }) if diagnostic.kind() == ChainSubmissionDiagnosticKind::AmbiguousDispatch
    ));
    assert_eq!(transport.methods(), vec!["POST"]);
    assert_eq!(
        store
            .record(&identity)
            .unwrap()
            .committed_post_reservations(),
        1
    );

    // A later invocation receives a fresh budget and reserves directly.
    let second = coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        second,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking { .. })
    ));
    assert_eq!(transport.methods(), vec!["POST", "POST", "GET"]);
    assert_eq!(
        store
            .record(&identity)
            .unwrap()
            .committed_post_reservations(),
        2
    );
}

#[tokio::test]
async fn definitely_unsent_failure_removes_fresh_authority() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Err(ChainTransportError::definitely_unsent("refused")));
    let coordinator = coordinator(transport, Arc::clone(&store), ManualClock::new(100), 10);

    let failure = coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Transport);
    assert!(failure.strongest_state().is_none());
    assert!(store.record(&identity).is_none());
}

fn explorer_fallback_page() -> ChainHttpResponse {
    // What a node that does not serve the route answered on 2026-09-08:
    // the explorer's single-page fallback, HTTP 200 and HTML.
    ChainHttpResponse::new(
        200,
        b"<!doctype html>\n<html lang=\"en\">".to_vec(),
        Some("text/html; charset=utf-8".to_string()),
        Vec::new(),
    )
}

#[tokio::test]
async fn an_unsupported_endpoint_is_definitely_unsent_and_reported_as_protocol() {
    // The only configured node answers from outside the API. Nothing was
    // dispatched, so the fresh reservation is released and no retry is spent
    // on an answer that cannot change. A later pass against an upgraded node
    // submits the same generation afresh.
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(explorer_fallback_page()));
    let coordinator = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    );

    let failure = coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Protocol);
    assert!(failure.strongest_state().is_none());
    assert!(
        failure
            .message()
            .contains("does not serve /shielded-vote/v1/cast-vote"),
        "{}",
        failure.message()
    );
    assert!(store.record(&identity).is_none());
    assert_eq!(transport.methods(), vec!["POST"], "no retry, no poll");

    let upgraded = Arc::new(ScriptedTransport::default());
    upgraded.queue(Ok(accepted()));
    upgraded.queue(Ok(pending()));
    coordinator_over(upgraded, Arc::clone(&store), ManualClock::new(200), 10)
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        store.record(&identity).unwrap().state(),
        SubmissionRecordState::Tracking { .. }
    ));
}

#[tokio::test]
async fn an_unsupported_endpoint_rotates_to_the_next_configured_node() {
    // Two nodes, the first behind an old release. The attempt budget rotates
    // to the second node, which accepts; the first is not asked again.
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(explorer_fallback_page()));
    transport.queue(Ok(accepted()));
    transport.queue(Ok(confirmed(&identity)));
    let protocol = ChainProtocolClient::new(
        Arc::clone(&transport),
        Network::Testnet,
        &[
            "https://old.example".to_string(),
            "https://new.example".to_string(),
        ],
    )
    .unwrap();
    let coordinator = ChainSubmissionCoordinator::new(
        protocol,
        Arc::clone(&store),
        ManualClock::new(100),
        CoordinatorPolicy::new(Duration::from_secs(10), 2, vec![Duration::from_millis(1)]).unwrap(),
    )
    .unwrap();

    coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    let urls = transport.post_urls();
    assert_eq!(urls.len(), 2, "{urls:?}");
    assert!(urls[0].starts_with("https://old.example/"), "{urls:?}");
    assert!(urls[1].starts_with("https://new.example/"), "{urls:?}");
    assert!(matches!(
        store.record(&identity).unwrap().state(),
        SubmissionRecordState::Confirmed(_)
    ));
}

#[tokio::test]
async fn tracking_deadline_survives_polling_and_coordinator_restart() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let first_transport = Arc::new(ScriptedTransport::default());
    first_transport.queue(Ok(accepted()));
    first_transport.queue(Ok(pending()));
    let clock = ManualClock::new(100);
    coordinator(first_transport, Arc::clone(&store), clock.clone(), 10)
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    clock.set(111);
    let restarted_transport = Arc::new(ScriptedTransport::default());
    restarted_transport.queue(Ok(pending()));
    let result = coordinator(restarted_transport, Arc::clone(&store), clock, 10)
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: Some(_),
            ..
        })
    ));
    assert_eq!(
        store.record(&identity).unwrap().tracking_started_at(),
        Some(100)
    );
}

#[tokio::test]
async fn hash_confirmation_updates_submission_and_projection_atomically() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(accepted()));
    transport.queue(Ok(confirmed(&identity)));
    let result = coordinator(transport, Arc::clone(&store), ManualClock::new(100), 10)
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(matches!(result, ChainSubmissionResult::Confirmed(_)));
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Confirmed
    );
    let projection = store.projection(&identity).unwrap();
    assert_eq!(projection.final_van_position(), 7);
    assert_eq!(projection.vote_commitment_positions(), &[8]);
    assert_eq!(projection.transaction_hash(), Some(HASH.parse().unwrap()));
}

#[tokio::test]
async fn ambiguous_confirmation_attributes_leave_tracking_authoritative() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(accepted()));
    let event = TxEvent {
        event_type: "cast_vote".to_string(),
        attributes: vec![
            TxEventAttribute {
                key: "vote_round_id".to_string(),
                value: hex::encode(identity.vote_round_id()),
            },
            TxEventAttribute {
                key: "leaf_index".to_string(),
                value: "7,8".to_string(),
            },
            TxEventAttribute {
                key: "leaf_index".to_string(),
                value: "9,10".to_string(),
            },
        ],
    };
    transport.queue(Ok(ChainHttpResponse::json(
        200,
        serde_json::to_vec(&serde_json::json!({
            "height": "9",
            "code": 0,
            "log": "",
            "events": [event],
        }))
        .unwrap(),
    )));

    let failure = coordinator(transport, Arc::clone(&store), ManualClock::new(100), 10)
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Protocol);
    assert_eq!(
        failure.strongest_state().unwrap().state(),
        ChainSubmissionState::Tracking
    );
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Tracking
    );
    assert!(store.projection(&identity).is_none());
}

#[tokio::test]
async fn cancelled_entry_with_no_state_performs_no_derivation_or_network_work() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    let transport = Arc::new(ScriptedTransport::default());
    let control = ManualControl::default();
    control.cancelled.store(true, Ordering::SeqCst);
    let result = coordinator(Arc::clone(&transport), store, ManualClock::new(100), 10)
        .advance(StoreAdvancementRequest::vote(identity), &control)
        .await
        .unwrap();

    assert_eq!(result, ChainSubmissionResult::Cancelled);
    assert!(transport.methods().is_empty());
}

#[tokio::test]
async fn cancellation_after_reservation_before_dispatch_removes_fresh_reservation() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    // Admission and the explicit pre-dispatch check observe active work. The
    // biased dispatch select observes cancellation before polling the POST.
    let control = CancelOnCheck::new(3);

    let result = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance(StoreAdvancementRequest::vote(identity.clone()), &control)
    .await
    .unwrap();

    assert_eq!(result, ChainSubmissionResult::Cancelled);
    assert!(store.record(&identity).is_none());
    assert!(transport.methods().is_empty());
}

#[tokio::test]
async fn cancelled_batch_entry_requires_no_recovery_or_roster_derivation() {
    let batch = batch_identity(0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    let transport = Arc::new(ScriptedTransport::default());
    let control = ManualControl::default();
    control.cancelled.store(true, Ordering::SeqCst);

    let result = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance(
        StoreAdvancementRequest::vote_batch(batch.clone(), vec![1, 2]).unwrap(),
        &control,
    )
    .await
    .unwrap();

    assert_eq!(result, ChainSubmissionResult::Cancelled);
    assert!(store.record(&batch).is_none());
    assert!(transport.methods().is_empty());
}

#[tokio::test]
async fn cancelled_batch_entry_returns_authoritative_batch_before_stale_roster() {
    let batch = batch_identity(0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived_batch(batch.clone(), vec![1, 2]));
    store.seed_batch_roster(batch.clone(), vec![1, 2]);
    let initial_request = StoreAdvancementRequest::vote_batch(batch.clone(), vec![1, 2]).unwrap();
    assert!(matches!(
        store.admit(&initial_request, true, 1, 1).unwrap(),
        StoreAdmission::Ready {
            fresh_reservation: true,
            ..
        }
    ));
    let roster_reads_before_cancellation = store.batch_roster_reads();
    let transport = Arc::new(ScriptedTransport::default());
    let control = ManualControl::default();
    control.cancelled.store(true, Ordering::SeqCst);

    let result = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance(
        StoreAdvancementRequest::vote_batch(batch.clone(), vec![1, 3]).unwrap(),
        &control,
    )
    .await
    .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            ..
        })
    ));
    assert_eq!(
        store.record(&batch).unwrap().durable_state(),
        ChainSubmissionState::Recovering
    );
    assert_eq!(store.batch_roster_reads(), roster_reads_before_cancellation);
    assert!(transport.methods().is_empty());
}

#[tokio::test]
async fn cancelled_entry_normalizes_abandoned_submitting_without_network_work() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    assert!(matches!(
        store
            .admit(
                &StoreAdvancementRequest::vote(identity.clone()),
                true,
                1,
                90
            )
            .unwrap(),
        StoreAdmission::Ready {
            fresh_reservation: true,
            ..
        }
    ));
    let transport = Arc::new(ScriptedTransport::default());
    let control = ManualControl::default();
    control.cancelled.store(true, Ordering::SeqCst);

    let result = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance(StoreAdvancementRequest::vote(identity.clone()), &control)
    .await
    .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            ..
        })
    ));
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Recovering
    );
    assert!(transport.methods().is_empty());
}

#[tokio::test]
async fn chain_rejection_preserves_bound_recovery_and_redacts_diagnostics() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(rejected()));

    let coordinator = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    );
    let result = coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(
        matches!(result, ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            ref diagnostic,
        }) if diagnostic.kind() == ChainSubmissionDiagnosticKind::ChainRejected
            && diagnostic.message().contains("sensitive"))
    );
    let record = store.record(&identity).unwrap();
    assert_eq!(record.durable_state(), ChainSubmissionState::Recovering);

    let replay = coordinator
        .advance(
            StoreAdvancementRequest::vote(identity),
            &ManualControl::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        replay,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            ..
        })
    ));
    assert_eq!(
        transport.methods(),
        vec!["POST"],
        "staged hashless recovery must not redispatch without a tree pass"
    );
}

#[tokio::test]
async fn changed_generation_is_rejected_before_reconciliation() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Err(ChainTransportError::possibly_dispatched("timeout")));
    let coordinator = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    );
    coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();
    store.seed_derivation(derived(identity.clone(), 2));

    let failure = coordinator
        .advance(
            StoreAdvancementRequest::vote(identity),
            &ManualControl::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::InvalidInput);
    assert_eq!(transport.methods(), vec!["POST"]);
}

#[tokio::test]
async fn rejected_recovery_accepts_only_the_same_generation() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(rejected()));
    let coordinator = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    );
    coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    let replay = coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        replay,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            ..
        })
    ));

    store.seed_derivation(derived(identity.clone(), 2));
    let failure = coordinator
        .advance(
            StoreAdvancementRequest::vote(identity),
            &ManualControl::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(failure.kind(), ChainSubmissionFailureKind::InvalidInput);
    assert_eq!(transport.methods(), vec!["POST"]);
}

#[tokio::test]
async fn failed_confirmation_rolls_back_submission_and_projection() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let first_transport = Arc::new(ScriptedTransport::default());
    first_transport.queue(Ok(accepted()));
    first_transport.queue(Ok(pending()));
    let clock = ManualClock::new(100);
    coordinator(first_transport, Arc::clone(&store), clock.clone(), 10)
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    store.fail_next_confirmation();
    let second_transport = Arc::new(ScriptedTransport::default());
    second_transport.queue(Ok(confirmed(&identity)));
    let failure = coordinator(second_transport, Arc::clone(&store), clock, 10)
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Storage);
    assert_eq!(
        failure.strongest_state().unwrap().state(),
        ChainSubmissionState::Tracking
    );
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Tracking
    );
    assert!(store.projection(&identity).is_none());
}

#[tokio::test]
async fn cancellation_after_confirmation_commit_point_cannot_suppress_persistence() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(accepted()));
    transport.queue(Ok(confirmed(&identity)));
    let control = Arc::new(ManualControl::default());
    store.after_next_confirmation_validation({
        let control = Arc::clone(&control);
        move || control.cancelled.store(true, Ordering::SeqCst)
    });

    let result = coordinator(transport, Arc::clone(&store), ManualClock::new(100), 10)
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            control.as_ref(),
        )
        .await
        .unwrap();

    assert!(matches!(result, ChainSubmissionResult::Confirmed(_)));
    assert!(control.is_cancelled());
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Confirmed
    );
    assert!(store.projection(&identity).is_some());
}

#[tokio::test]
async fn reservation_failure_dispatches_nothing_and_writes_nothing() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    store.fail_next_commit();
    let transport = Arc::new(ScriptedTransport::default());

    let failure = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance(
        StoreAdvancementRequest::vote(identity.clone()),
        &ManualControl::default(),
    )
    .await
    .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Storage);
    assert!(failure.strongest_state().is_none());
    assert!(store.record(&identity).is_none());
    assert!(transport.methods().is_empty());
}

#[tokio::test]
async fn failed_cancelled_entry_normalization_reports_possible_dispatch() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    store
        .admit(
            &StoreAdvancementRequest::vote(identity.clone()),
            true,
            1,
            90,
        )
        .unwrap();
    store.fail_next_commit();
    let transport = Arc::new(ScriptedTransport::default());
    let control = ManualControl::default();
    control.cancelled.store(true, Ordering::SeqCst);

    let failure = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance(StoreAdvancementRequest::vote(identity.clone()), &control)
    .await
    .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Storage);
    assert_eq!(
        failure.strongest_state().unwrap().evidence(),
        ChainSubmissionStateEvidence::KnownPossiblyDispatched
    );
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Submitting
    );
    assert!(transport.methods().is_empty());
}

#[tokio::test]
async fn failed_active_entry_normalization_reports_possible_dispatch() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    store
        .admit(
            &StoreAdvancementRequest::vote(identity.clone()),
            true,
            1,
            90,
        )
        .unwrap();
    store.fail_next_commit();
    let transport = Arc::new(ScriptedTransport::default());

    let failure = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance(
        StoreAdvancementRequest::vote(identity.clone()),
        &ManualControl::default(),
    )
    .await
    .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Storage);
    assert_eq!(
        failure.strongest_state().unwrap().evidence(),
        ChainSubmissionStateEvidence::KnownPossiblyDispatched
    );
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Submitting
    );
    assert!(transport.methods().is_empty());
}

#[test]
fn batch_roster_mismatch_never_creates_a_reservation() {
    let batch = batch_identity(0);
    for proposed in [vec![1], vec![2, 1], vec![1, 2, 3]] {
        let store = InMemoryChainSubmissionStore::default();
        store.seed_derivation(derived_batch(batch.clone(), vec![1, 2]));
        let request = StoreAdvancementRequest::vote_batch(batch.clone(), proposed).unwrap();

        let failure = store
            .admit(&request, true, 1, 100)
            .err()
            .expect("mismatched roster must fail");

        assert_eq!(failure.kind(), ChainSubmissionFailureKind::InvalidInput);
        assert!(store.record(&batch).is_none());
    }

    let duplicate = StoreAdvancementRequest::vote_batch(batch, vec![1, 1])
        .err()
        .expect("duplicate roster must fail");
    assert_eq!(duplicate.kind(), ChainSubmissionFailureKind::InvalidInput);
}

#[test]
fn batch_request_supplies_its_complete_recovery_independent_lock_set() {
    let batch = batch_identity(0);
    let request = StoreAdvancementRequest::vote_batch(batch.clone(), vec![2, 1]).unwrap();

    assert_eq!(
        request.applicable_identities(),
        vec![identity(1, 0), identity(2, 0), batch]
    );
}

#[test]
fn batch_request_enforces_protocol_action_bounds() {
    assert_eq!(crate::vote::MAX_VOTE_BATCH_ACTIONS, 50);

    let batch = batch_identity(0);
    let one = StoreAdvancementRequest::vote_batch(batch.clone(), vec![1]).unwrap();
    assert_eq!(one.applicable_identities().len(), 2);

    let maximum = (1..=crate::vote::MAX_VOTE_BATCH_ACTIONS as u32).collect::<Vec<_>>();
    let maximum = StoreAdvancementRequest::vote_batch(batch.clone(), maximum).unwrap();
    assert_eq!(
        maximum.applicable_identities().len(),
        crate::vote::MAX_VOTE_BATCH_ACTIONS + 1
    );

    let over_maximum = (1..=crate::vote::MAX_VOTE_BATCH_ACTIONS as u32 + 1).collect::<Vec<_>>();
    let failure = StoreAdvancementRequest::vote_batch(batch, over_maximum)
        .err()
        .expect("an oversized batch roster must fail before lock allocation");
    assert_eq!(failure.kind(), ChainSubmissionFailureKind::InvalidInput);
}

#[test]
fn persisted_batch_roster_mismatch_never_checks_unlocked_members() {
    let batch = batch_identity(0);
    let store = InMemoryChainSubmissionStore::default();
    store.seed_derivation(derived_batch(batch.clone(), vec![1, 2]));
    let request = StoreAdvancementRequest::vote_batch(batch.clone(), vec![1, 2]).unwrap();
    store.seed_batch_roster(batch.clone(), vec![1, 2, 3]);

    let failure = store
        .admit(&request, true, 1, 100)
        .err()
        .expect("changed persisted roster must fail before admission");

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::InvalidInput);
    assert!(store.record(&batch).is_none());
}

#[tokio::test]
async fn exclusive_round_gate_prevents_batch_roster_reads() {
    let batch = batch_identity(0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived_batch(batch.clone(), vec![1, 2]));
    let exclusive = store
        .coordination()
        .try_acquire_round_exclusive(&batch)
        .ok()
        .expect("round should initially be idle");
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Err(ChainTransportError::definitely_unsent("refused")));
    let (advance_started, advance_started_rx) = tokio::sync::oneshot::channel();
    let task = {
        let coordinator = coordinator(
            Arc::clone(&transport),
            Arc::clone(&store),
            ManualClock::new(100),
            10,
        );
        let batch = batch.clone();
        tokio::spawn(async move {
            advance_started
                .send(())
                .expect("advance-start receiver must remain live");
            coordinator
                .advance(
                    StoreAdvancementRequest::vote_batch(batch, vec![1, 2]).unwrap(),
                    &ManualControl::default(),
                )
                .await
        })
    };

    advance_started_rx
        .await
        .expect("advance task must start before waiting on the round gate");
    tokio::task::yield_now().await;
    assert!(!task.is_finished());
    assert_eq!(store.batch_roster_reads(), 0);
    assert!(store.record(&batch).is_none());
    assert!(transport.methods().is_empty());

    drop(exclusive);
    let failure = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Transport);
    assert_eq!(store.batch_roster_reads(), 1);
    assert!(store.record(&batch).is_none());
    assert_eq!(transport.methods(), vec!["POST"]);
}

#[test]
fn active_batch_admission_requires_a_persisted_roster() {
    let batch = batch_identity(0);
    let store = InMemoryChainSubmissionStore::default();
    let request = StoreAdvancementRequest::vote_batch(batch.clone(), vec![1, 2]).unwrap();

    let failure = store
        .admit(&request, true, 1, 100)
        .err()
        .expect("active batch admission requires a persisted roster");

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::InvalidInput);
    assert!(store.record(&batch).is_none());
}

#[tokio::test]
async fn omitted_batch_member_fails_before_its_unlocked_row_is_read() {
    let batch = batch_identity(0);
    let member_one = identity(1, 0);
    let unlocked_member = identity(2, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived_batch(batch.clone(), vec![1, 2]));
    store.seed_derivation(derived(unlocked_member.clone(), 2));
    assert!(matches!(
        store
            .admit(&StoreAdvancementRequest::vote(unlocked_member), true, 1, 1)
            .unwrap(),
        StoreAdmission::Ready {
            fresh_reservation: true,
            ..
        }
    ));
    store.require_admission_identity_locks(vec![batch.clone(), member_one]);
    let transport = Arc::new(ScriptedTransport::default());
    let request = StoreAdvancementRequest::vote_batch(batch.clone(), vec![1]).unwrap();

    let failure = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance(request, &ManualControl::default())
    .await
    .expect_err("omitted member must fail");

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::InvalidInput);
    assert!(failure.strongest_state().is_none());
    assert!(store.record(&batch).is_none());
    assert!(transport.methods().is_empty());
}

#[test]
fn request_kind_must_match_the_identity_target() {
    let identity = delegation_identity(0);
    let store = InMemoryChainSubmissionStore::default();
    let request = StoreAdvancementRequest::vote(identity.clone());

    let failure = store
        .admit(&request, true, 1, 100)
        .err()
        .expect("mismatched request kind must fail");

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::InvalidInput);
    assert!(store.record(&identity).is_none());
}

#[tokio::test]
async fn committed_failure_moves_tracking_to_recovery_and_clears_recovery_candidate() {
    let tracking_identity = identity(1, 0);
    let tracking_store = Arc::new(InMemoryChainSubmissionStore::default());
    tracking_store.seed_derivation(derived(tracking_identity.clone(), 1));
    let first_transport = Arc::new(ScriptedTransport::default());
    first_transport.queue(Ok(accepted()));
    first_transport.queue(Ok(pending()));
    coordinator(
        first_transport,
        Arc::clone(&tracking_store),
        ManualClock::new(100),
        10,
    )
    .advance(
        StoreAdvancementRequest::vote(tracking_identity.clone()),
        &ManualControl::default(),
    )
    .await
    .unwrap();
    let failed_status = ChainHttpResponse::json(
        422,
        br#"{"height":"9","code":12,"log":"rejected","events":[]}"#.to_vec(),
    );
    let tracking_transport = Arc::new(ScriptedTransport::default());
    tracking_transport.queue(Ok(failed_status.clone()));
    let tracking_result = coordinator(
        tracking_transport,
        Arc::clone(&tracking_store),
        ManualClock::new(101),
        10,
    )
    .advance(
        StoreAdvancementRequest::vote(tracking_identity.clone()),
        &ManualControl::default(),
    )
    .await
    .unwrap();
    assert!(matches!(
        tracking_result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            ref diagnostic,
        }) if diagnostic.kind() == ChainSubmissionDiagnosticKind::ChainRejected
    ));
    assert_eq!(
        tracking_store
            .record(&tracking_identity)
            .unwrap()
            .durable_state(),
        ChainSubmissionState::Recovering
    );

    let recovering_identity = identity(2, 1);
    let recovering_store = Arc::new(InMemoryChainSubmissionStore::default());
    recovering_store.seed_derivation(derived(recovering_identity.clone(), 2));
    let start_transport = Arc::new(ScriptedTransport::default());
    start_transport.queue(Ok(accepted()));
    start_transport.queue(Ok(pending()));
    coordinator(
        start_transport,
        Arc::clone(&recovering_store),
        ManualClock::new(100),
        1,
    )
    .advance(
        StoreAdvancementRequest::vote(recovering_identity.clone()),
        &ManualControl::default(),
    )
    .await
    .unwrap();
    let expiry_transport = Arc::new(ScriptedTransport::default());
    expiry_transport.queue(Ok(pending()));
    coordinator(
        expiry_transport,
        Arc::clone(&recovering_store),
        ManualClock::new(101),
        1,
    )
    .advance(
        StoreAdvancementRequest::vote(recovering_identity.clone()),
        &ManualControl::default(),
    )
    .await
    .unwrap();
    let recovery_transport = Arc::new(ScriptedTransport::default());
    recovery_transport.queue(Ok(failed_status));
    let recovery_result = coordinator(
        recovery_transport,
        Arc::clone(&recovering_store),
        ManualClock::new(102),
        1,
    )
    .advance(
        StoreAdvancementRequest::vote(recovering_identity.clone()),
        &ManualControl::default(),
    )
    .await
    .unwrap();
    assert!(matches!(
        recovery_result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            ..
        })
    ));
}

#[tokio::test]
async fn attempts_cycle_endpoints_and_exhaust_to_hashless_recovery() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Err(ChainTransportError::definitely_unsent("first refused")));
    transport.queue(Err(ChainTransportError::possibly_dispatched(
        "second timed out",
    )));
    transport.queue(Err(ChainTransportError::possibly_dispatched(
        "third timed out",
    )));
    let protocol = ChainProtocolClient::new(
        Arc::clone(&transport),
        Network::Testnet,
        &[
            "https://one.example".to_string(),
            "https://two.example".to_string(),
        ],
    )
    .unwrap();
    let coordinator = ChainSubmissionCoordinator::new(
        protocol,
        Arc::clone(&store),
        ManualClock::new(100),
        CoordinatorPolicy::new(
            Duration::from_secs(10),
            3,
            vec![Duration::from_millis(1), Duration::from_millis(1)],
        )
        .unwrap(),
    )
    .unwrap();

    let result = coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            ref diagnostic,
        }) if diagnostic.kind() == ChainSubmissionDiagnosticKind::AmbiguousDispatch
    ));
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Recovering
    );
    assert_eq!(transport.methods(), vec!["POST", "POST", "POST"]);
    assert_eq!(
        transport.post_urls(),
        vec![
            "https://one.example/shielded-vote/v1/cast-vote",
            "https://two.example/shielded-vote/v1/cast-vote",
            "https://one.example/shielded-vote/v1/cast-vote",
        ]
    );
    assert_eq!(
        store
            .record(&identity)
            .unwrap()
            .committed_post_reservations(),
        3
    );
}

#[tokio::test(start_paused = true)]
async fn ambiguous_retry_waits_for_configured_backoff() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Err(ChainTransportError::possibly_dispatched("timeout")));
    transport.queue(Ok(accepted()));
    transport.queue(Ok(pending()));
    let protocol = ChainProtocolClient::new(
        Arc::clone(&transport),
        Network::Testnet,
        &["https://chain.example".to_string()],
    )
    .unwrap();
    let coordinator = Arc::new(
        ChainSubmissionCoordinator::new(
            protocol,
            store,
            ManualClock::new(100),
            CoordinatorPolicy::new(Duration::from_secs(10), 2, vec![Duration::from_secs(5)])
                .unwrap(),
        )
        .unwrap(),
    );
    let task = tokio::spawn(async move {
        coordinator
            .advance(
                StoreAdvancementRequest::vote(identity),
                &ManualControl::default(),
            )
            .await
    });
    tokio::task::yield_now().await;
    assert_eq!(transport.methods(), vec!["POST"]);

    tokio::time::advance(Duration::from_secs(4)).await;
    tokio::task::yield_now().await;
    assert_eq!(transport.methods(), vec!["POST"]);

    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(matches!(
        task.await.unwrap().unwrap(),
        ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking { .. })
    ));
    assert_eq!(transport.methods(), vec!["POST", "POST", "GET"]);
}

#[tokio::test]
async fn malformed_accepted_response_reserves_the_next_bounded_retry() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(ChainHttpResponse::json(
        200,
        br#"{"code":0,"log":""}"#.to_vec(),
    )));
    transport.queue(Ok(accepted()));
    transport.queue(Ok(pending()));
    let protocol = ChainProtocolClient::new(
        Arc::clone(&transport),
        Network::Testnet,
        &["https://chain.example".to_string()],
    )
    .unwrap();
    let coordinator = ChainSubmissionCoordinator::new(
        protocol,
        Arc::clone(&store),
        ManualClock::new(100),
        CoordinatorPolicy::new(Duration::from_secs(10), 2, vec![Duration::from_millis(1)]).unwrap(),
    )
    .unwrap();

    let result = coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking { .. })
    ));
    assert_eq!(transport.methods(), vec!["POST", "POST", "GET"]);
    let record = store.record(&identity).unwrap();
    assert_eq!(record.durable_state(), ChainSubmissionState::Tracking);
    assert_eq!(record.committed_post_reservations(), 2);
}

#[tokio::test]
async fn nonfinal_candidate_hash_collision_uses_the_remaining_bounded_retry() {
    let owner_identity = identity(1, 0);
    let retrying_identity = identity(2, 1);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(owner_identity.clone(), 1));
    store.seed_derivation(derived(retrying_identity.clone(), 2));

    let owner_transport = Arc::new(ScriptedTransport::default());
    owner_transport.queue(Ok(accepted()));
    owner_transport.queue(Ok(pending()));
    coordinator(
        owner_transport,
        Arc::clone(&store),
        ManualClock::new(90),
        10,
    )
    .advance(
        StoreAdvancementRequest::vote(owner_identity),
        &ManualControl::default(),
    )
    .await
    .unwrap();

    let retry_transport = Arc::new(ScriptedTransport::default());
    retry_transport.queue(Ok(accepted()));
    retry_transport.queue(Ok(accepted_with_hash(&"12".repeat(32))));
    retry_transport.queue(Ok(pending()));
    let protocol = ChainProtocolClient::new(
        Arc::clone(&retry_transport),
        Network::Testnet,
        &["https://chain.example".to_string()],
    )
    .unwrap();
    let coordinator = ChainSubmissionCoordinator::new(
        protocol,
        Arc::clone(&store),
        ManualClock::new(100),
        CoordinatorPolicy::new(Duration::from_secs(10), 2, vec![Duration::from_millis(1)]).unwrap(),
    )
    .unwrap();

    let result = coordinator
        .advance(
            StoreAdvancementRequest::vote(retrying_identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking { .. })
    ));
    assert_eq!(retry_transport.methods(), vec!["POST", "POST", "GET"]);
    let record = store.record(&retrying_identity).unwrap();
    assert_eq!(record.durable_state(), ChainSubmissionState::Tracking);
    assert_eq!(record.committed_post_reservations(), 2);
}

#[tokio::test]
async fn invalid_protocol_ambiguity_is_retryable_on_a_later_advance() {
    let identity = identity(1, 0);
    let request = StoreAdvancementRequest::vote(identity.clone());
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let StoreAdmission::Ready { derived, .. } = store.admit(&request, true, 1, 90).unwrap() else {
        panic!("fresh reservation")
    };
    store
        .classify_post(
            derived.generation(),
            SubmissionObservation::PossiblyDispatched(
                ChainSubmissionDiagnostic::from_redacted_message(
                    ChainSubmissionDiagnosticKind::InvalidProtocolResponse,
                    "vote-chain mutation redirect was rejected",
                ),
            ),
            91,
        )
        .unwrap();
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(accepted()));
    transport.queue(Ok(pending()));

    let result = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance(request, &ManualControl::default())
    .await
    .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking { .. })
    ));
    assert_eq!(transport.methods(), vec!["POST", "GET"]);
    let record = store.record(&identity).unwrap();
    assert_eq!(record.durable_state(), ChainSubmissionState::Tracking);
    assert_eq!(record.committed_post_reservations(), 2);
}

#[tokio::test]
async fn definite_rejection_recovery_never_reserves_an_ambiguous_retry() {
    let identity = identity(1, 0);
    let request = StoreAdvancementRequest::vote(identity.clone());
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let StoreAdmission::Ready { derived, .. } = store.admit(&request, true, 1, 90).unwrap() else {
        panic!("fresh reservation")
    };
    store
        .classify_post(
            derived.generation(),
            SubmissionObservation::DefiniteRejection(
                ChainSubmissionDiagnostic::from_redacted_message(
                    ChainSubmissionDiagnosticKind::ChainRejected,
                    "vote chain rejected transaction with code 7",
                ),
            ),
            91,
        )
        .unwrap();
    let transport = Arc::new(ScriptedTransport::default());

    let result = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance(request, &ManualControl::default())
    .await
    .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            ..
        })
    ));
    assert!(transport.methods().is_empty());
    let record = store.record(&identity).unwrap();
    assert_eq!(record.durable_state(), ChainSubmissionState::Recovering);
    assert_eq!(record.committed_post_reservations(), 1);
}

#[tokio::test]
async fn accepted_retry_short_circuits_to_normal_hash_tracking() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Err(ChainTransportError::possibly_dispatched("timeout")));
    transport.queue(Ok(accepted()));
    transport.queue(Ok(pending()));
    let protocol = ChainProtocolClient::new(
        Arc::clone(&transport),
        Network::Testnet,
        &["https://chain.example".to_string()],
    )
    .unwrap();
    let coordinator = ChainSubmissionCoordinator::new(
        protocol,
        Arc::clone(&store),
        ManualClock::new(100),
        CoordinatorPolicy::new(Duration::from_secs(10), 2, vec![Duration::from_millis(1)]).unwrap(),
    )
    .unwrap();

    let result = coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking { .. })
    ));
    assert_eq!(transport.methods(), vec!["POST", "POST", "GET"]);
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Tracking
    );
}

#[tokio::test]
async fn nullifier_spent_after_ambiguity_is_terminal_and_idempotent() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Err(ChainTransportError::possibly_dispatched("timeout")));
    transport.queue(Ok(ChainHttpResponse::json(
        422,
        br#"{"code":2,"log":"must not be retained"}"#.to_vec(),
    )));
    let protocol = ChainProtocolClient::new(
        Arc::clone(&transport),
        Network::Testnet,
        &["https://chain.example".to_string()],
    )
    .unwrap();
    let coordinator = ChainSubmissionCoordinator::new(
        protocol,
        Arc::clone(&store),
        ManualClock::new(100),
        CoordinatorPolicy::new(Duration::from_secs(10), 2, vec![Duration::from_millis(1)]).unwrap(),
    )
    .unwrap();

    for _ in 0..2 {
        let result = coordinator
            .advance(
                StoreAdvancementRequest::vote(identity.clone()),
                &ManualControl::default(),
            )
            .await
            .unwrap();
        assert!(matches!(
            result,
            ChainSubmissionResult::SubmittedWithoutHash(ref diagnostic)
                if diagnostic.kind() == ChainSubmissionDiagnosticKind::NullifierAlreadySpent
                    && !diagnostic.message().contains("must not be retained")
        ));
    }
    assert_eq!(transport.methods(), vec!["POST", "POST"]);
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::SubmittedWithoutHash
    );
    assert!(store.projection(&identity).is_none());
}

#[tokio::test]
async fn other_rejection_after_ambiguity_surfaces_error_and_preserves_ambiguity() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Err(ChainTransportError::possibly_dispatched("timeout")));
    transport.queue(Ok(rejected()));
    let protocol = ChainProtocolClient::new(
        transport,
        Network::Testnet,
        &["https://chain.example".to_string()],
    )
    .unwrap();
    let coordinator = ChainSubmissionCoordinator::new(
        protocol,
        Arc::clone(&store),
        ManualClock::new(100),
        CoordinatorPolicy::new(Duration::from_secs(10), 2, vec![Duration::from_millis(1)]).unwrap(),
    )
    .unwrap();

    let failure = coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Protocol);
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Recovering
    );
}

#[tokio::test]
async fn same_identity_concurrency_releases_only_one_post() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(accepted()));
    transport.queue(Ok(pending()));
    transport.queue(Ok(pending()));
    let (entered, release) = transport.gate_first_post();
    let coordinator = Arc::new(coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    ));

    let first = {
        let coordinator = Arc::clone(&coordinator);
        let identity = identity.clone();
        tokio::spawn(async move {
            coordinator
                .advance(
                    StoreAdvancementRequest::vote(identity),
                    &ManualControl::default(),
                )
                .await
        })
    };
    entered.notified().await;
    let second = {
        let coordinator = Arc::clone(&coordinator);
        let identity = identity.clone();
        tokio::spawn(async move {
            coordinator
                .advance(
                    StoreAdvancementRequest::vote(identity),
                    &ManualControl::default(),
                )
                .await
        })
    };
    tokio::task::yield_now().await;
    assert_eq!(transport.methods(), vec!["POST"]);
    release.notify_one();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();

    assert_eq!(transport.methods(), vec!["POST", "GET", "GET"]);
    assert_eq!(
        store
            .record(&identity)
            .unwrap()
            .committed_post_reservations(),
        1
    );
}

#[tokio::test]
async fn same_batch_concurrency_releases_only_one_atomic_post() {
    let identity = batch_identity(0);
    let proposals = vec![1, 2];
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived_batch(identity.clone(), proposals.clone()));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(accepted_batch(&identity)));
    transport.queue(Ok(pending()));
    transport.queue(Ok(pending()));
    let (entered, release) = transport.gate_first_post();
    let coordinator = Arc::new(coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    ));

    let first = {
        let coordinator = Arc::clone(&coordinator);
        let identity = identity.clone();
        let proposals = proposals.clone();
        tokio::spawn(async move {
            coordinator
                .advance(
                    StoreAdvancementRequest::vote_batch(identity, proposals).unwrap(),
                    &ManualControl::default(),
                )
                .await
        })
    };
    entered.notified().await;
    let second = {
        let coordinator = Arc::clone(&coordinator);
        let identity = identity.clone();
        tokio::spawn(async move {
            coordinator
                .advance(
                    StoreAdvancementRequest::vote_batch(identity, proposals).unwrap(),
                    &ManualControl::default(),
                )
                .await
        })
    };
    tokio::task::yield_now().await;
    assert_eq!(transport.methods(), vec!["POST"]);
    release.notify_one();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();

    assert_eq!(transport.methods(), vec!["POST", "GET", "GET"]);
    assert_eq!(
        store
            .record(&identity)
            .unwrap()
            .committed_post_reservations(),
        1
    );
}

#[tokio::test]
async fn coordinators_for_one_store_share_the_same_lock_authority() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(accepted()));
    transport.queue(Ok(pending()));
    transport.queue(Ok(pending()));
    let (entered, release) = transport.gate_first_post();
    let first_coordinator = Arc::new(coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    ));
    let second_coordinator = Arc::new(coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    ));

    let first = {
        let coordinator = Arc::clone(&first_coordinator);
        let identity = identity.clone();
        tokio::spawn(async move {
            coordinator
                .advance(
                    StoreAdvancementRequest::vote(identity),
                    &ManualControl::default(),
                )
                .await
        })
    };
    entered.notified().await;
    let second = {
        let coordinator = Arc::clone(&second_coordinator);
        let identity = identity.clone();
        tokio::spawn(async move {
            coordinator
                .advance(
                    StoreAdvancementRequest::vote(identity),
                    &ManualControl::default(),
                )
                .await
        })
    };
    tokio::task::yield_now().await;
    assert_eq!(transport.methods(), vec!["POST"]);

    release.notify_one();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();

    assert_eq!(transport.methods(), vec!["POST", "GET", "GET"]);
    assert_eq!(
        store
            .record(&identity)
            .unwrap()
            .committed_post_reservations(),
        1
    );
}

#[tokio::test]
async fn independent_bundles_progress_while_another_post_is_blocked() {
    let first_identity = identity(1, 0);
    let second_identity = identity(2, 1);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(first_identity.clone(), 1));
    store.seed_derivation(derived(second_identity.clone(), 2));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(accepted()));
    transport.queue(Ok(pending()));
    transport.queue(Ok(accepted()));
    transport.queue(Ok(pending()));
    let (entered, release) = transport.gate_first_post();
    let coordinator = Arc::new(coordinator(
        Arc::clone(&transport),
        store,
        ManualClock::new(100),
        10,
    ));

    let first = {
        let coordinator = Arc::clone(&coordinator);
        tokio::spawn(async move {
            coordinator
                .advance(
                    StoreAdvancementRequest::vote(first_identity),
                    &ManualControl::default(),
                )
                .await
        })
    };
    entered.notified().await;
    let second = {
        let coordinator = Arc::clone(&coordinator);
        tokio::spawn(async move {
            coordinator
                .advance(
                    StoreAdvancementRequest::vote(second_identity),
                    &ManualControl::default(),
                )
                .await
        })
    };

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if transport
                .methods()
                .iter()
                .filter(|method| **method == "POST")
                .count()
                == 2
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("unrelated bundle should not wait for the first POST");
    second.await.unwrap().unwrap();
    release.notify_one();
    first.await.unwrap().unwrap();

    assert_eq!(
        transport
            .methods()
            .iter()
            .filter(|method| **method == "POST")
            .count(),
        2
    );
}

#[tokio::test]
async fn same_bundle_blocks_a_successor_until_the_predecessor_is_authoritative() {
    let first_identity = identity(1, 0);
    let second_identity = identity(2, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(first_identity.clone(), 1));
    store.seed_derivation(derived(second_identity.clone(), 2));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(accepted()));
    transport.queue(Ok(pending()));
    transport.queue(Ok(accepted()));
    transport.queue(Ok(pending()));
    let (entered, release) = transport.gate_first_post();
    let coordinator = Arc::new(coordinator(
        Arc::clone(&transport),
        store,
        ManualClock::new(100),
        10,
    ));

    let first = {
        let coordinator = Arc::clone(&coordinator);
        tokio::spawn(async move {
            coordinator
                .advance(
                    StoreAdvancementRequest::vote(first_identity),
                    &ManualControl::default(),
                )
                .await
        })
    };
    entered.notified().await;
    let second = {
        let coordinator = Arc::clone(&coordinator);
        tokio::spawn(async move {
            coordinator
                .advance(
                    StoreAdvancementRequest::vote(second_identity),
                    &ManualControl::default(),
                )
                .await
        })
    };
    tokio::task::yield_now().await;
    assert_eq!(transport.methods(), vec!["POST"]);
    release.notify_one();
    first.await.unwrap().unwrap();
    let failure = second.await.unwrap().unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::InvalidInput);
    assert!(failure.strongest_state().is_none());
    assert_eq!(
        transport
            .methods()
            .iter()
            .filter(|method| **method == "POST")
            .count(),
        1
    );
}

#[tokio::test]
async fn final_candidate_hash_collision_stays_recovering_without_lookup() {
    let first_identity = identity(1, 0);
    let second_identity = identity(2, 1);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(first_identity.clone(), 1));
    store.seed_derivation(derived(second_identity.clone(), 2));

    let first_transport = Arc::new(ScriptedTransport::default());
    first_transport.queue(Ok(accepted()));
    first_transport.queue(Ok(pending()));
    coordinator(
        Arc::clone(&first_transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance(
        StoreAdvancementRequest::vote(first_identity),
        &ManualControl::default(),
    )
    .await
    .unwrap();

    let second_transport = Arc::new(ScriptedTransport::default());
    second_transport.queue(Ok(accepted()));
    let coordinator = coordinator(
        Arc::clone(&second_transport),
        Arc::clone(&store),
        ManualClock::new(101),
        10,
    );
    let result = coordinator
        .advance(
            StoreAdvancementRequest::vote(second_identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            ref diagnostic,
        }) if diagnostic.kind() == ChainSubmissionDiagnosticKind::InvalidProtocolResponse
    ));
    // No lookup on the colliding hash, and no redispatch in this invocation.
    assert_eq!(second_transport.methods(), vec!["POST"]);
    let record = store.record(&second_identity).unwrap();
    assert_eq!(record.durable_state(), ChainSubmissionState::Recovering);
    assert_eq!(record.committed_post_reservations(), 1);

    second_transport.queue(Ok(accepted_with_hash(&"12".repeat(32))));
    second_transport.queue(Ok(pending()));
    let result = coordinator
        .advance(
            StoreAdvancementRequest::vote(second_identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking { .. })
    ));
    assert_eq!(second_transport.methods(), vec!["POST", "POST", "GET"]);
}

#[tokio::test]
async fn atomically_confirmed_predecessor_allows_the_next_bundle_generation() {
    let first_identity = identity(1, 0);
    let second_identity = identity(2, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(first_identity.clone(), 1));
    store.seed_derivation(derived(second_identity.clone(), 2));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(accepted()));
    transport.queue(Ok(confirmed(&first_identity)));
    transport.queue(Ok(ChainHttpResponse::json(
        200,
        format!(r#"{{"tx_hash":"{}","code":0,"log":""}}"#, "12".repeat(32)).into_bytes(),
    )));
    transport.queue(Ok(pending()));
    let coordinator = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    );

    let first = coordinator
        .advance(
            StoreAdvancementRequest::vote(first_identity),
            &ManualControl::default(),
        )
        .await
        .unwrap();
    let second = coordinator
        .advance(
            StoreAdvancementRequest::vote(second_identity),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(matches!(first, ChainSubmissionResult::Confirmed(_)));
    assert!(matches!(
        second,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking { .. })
    ));
    assert_eq!(transport.methods(), vec!["POST", "GET", "POST", "GET"]);
}

#[tokio::test]
async fn confirmed_successor_refuses_delegation_reservation() {
    let vote_identity = identity(1, 0);
    let delegation = delegation_identity(0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(vote_identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(accepted()));
    transport.queue(Ok(confirmed(&vote_identity)));
    let coordinator = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    );
    let vote = coordinator
        .advance(
            StoreAdvancementRequest::vote(vote_identity),
            &ManualControl::default(),
        )
        .await
        .unwrap();
    assert!(matches!(vote, ChainSubmissionResult::Confirmed(_)));

    let failure = coordinator
        .advance(
            StoreAdvancementRequest::delegation(delegation.clone(), [7; 64]),
            &ManualControl::default(),
        )
        .await
        .expect_err("a confirmed successor must refuse a delegation reservation");

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::InvalidInput);
    assert!(store.record(&delegation).is_none());
    assert_eq!(transport.methods(), vec!["POST", "GET"]);
}

#[tokio::test]
async fn cancellation_during_final_dispatch_persists_hashless_recovery() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    let (entered, _release) = transport.gate_first_post();
    let coordinator = Arc::new(coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    ));
    let control = Arc::new(ManualControl::default());
    let task = {
        let coordinator = Arc::clone(&coordinator);
        let control = Arc::clone(&control);
        let identity = identity.clone();
        tokio::spawn(async move {
            coordinator
                .advance(StoreAdvancementRequest::vote(identity), control.as_ref())
                .await
        })
    };
    entered.notified().await;
    control.cancelled.store(true, Ordering::SeqCst);

    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            ref diagnostic,
        }) if diagnostic.kind() == ChainSubmissionDiagnosticKind::AmbiguousDispatch
    ));
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Recovering
    );
    assert_eq!(transport.methods(), vec!["POST"]);
}

#[tokio::test]
async fn operation_epoch_change_during_final_dispatch_persists_hashless_recovery() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    let (entered, _release) = transport.gate_first_post();
    let coordinator = Arc::new(coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    ));
    let control = Arc::new(ManualControl::default());
    let task = {
        let coordinator = Arc::clone(&coordinator);
        let control = Arc::clone(&control);
        let identity = identity.clone();
        tokio::spawn(async move {
            coordinator
                .advance(StoreAdvancementRequest::vote(identity), control.as_ref())
                .await
        })
    };
    entered.notified().await;
    control.epoch.store(1, Ordering::SeqCst);

    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            ref diagnostic,
        }) if diagnostic.kind() == ChainSubmissionDiagnosticKind::AmbiguousDispatch
    ));
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Recovering
    );
}

#[tokio::test]
async fn failed_post_classification_reports_known_possible_dispatch() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Err(ChainTransportError::possibly_dispatched("timeout")));
    transport.fail_classification_after_post(Arc::clone(&store));

    let failure = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance(
        StoreAdvancementRequest::vote(identity.clone()),
        &ManualControl::default(),
    )
    .await
    .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Storage);
    assert_eq!(
        failure.strongest_state().unwrap().evidence(),
        ChainSubmissionStateEvidence::KnownPossiblyDispatched
    );
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Submitting
    );
    assert_eq!(transport.methods(), vec!["POST"]);
}

#[tokio::test]
async fn failed_tracking_reconciliation_reports_the_durable_state() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let initial_transport = Arc::new(ScriptedTransport::default());
    initial_transport.queue(Ok(accepted()));
    initial_transport.queue(Ok(pending()));
    coordinator(
        initial_transport,
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance(
        StoreAdvancementRequest::vote(identity.clone()),
        &ManualControl::default(),
    )
    .await
    .unwrap();

    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(pending()));
    transport.fail_commit_after_lookup(Arc::clone(&store));
    let failure = coordinator(transport, Arc::clone(&store), ManualClock::new(101), 10)
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Storage);
    assert_eq!(
        failure.strongest_state().unwrap().state(),
        ChainSubmissionState::Tracking
    );
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Tracking
    );
}

#[tokio::test]
async fn nonfinal_ambiguous_recovery_retry_continues_bounded_retries() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    seed_hashless_tree_recovery(&store, &StoreAdvancementRequest::vote(identity.clone()));
    let transport = Arc::new(ScriptedTransport::default());
    for response in tree_responses(&[[8; 32]]) {
        transport.queue(Ok(response));
    }
    transport.queue(Err(ChainTransportError::possibly_dispatched("timeout")));
    transport.queue(Ok(accepted()));
    transport.queue(Ok(pending()));
    let protocol = ChainProtocolClient::new(
        Arc::clone(&transport),
        Network::Testnet,
        &["https://chain.example".to_string()],
    )
    .unwrap();
    let coordinator = ChainSubmissionCoordinator::new(
        protocol,
        Arc::clone(&store),
        ManualClock::new(100),
        CoordinatorPolicy::new(Duration::from_secs(10), 2, vec![Duration::from_millis(1)]).unwrap(),
    )
    .unwrap();

    let result = coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking { .. })
    ));
    // One scan authorizes the first retry; the second retry needs only the
    // configured backoff and a durable ambiguous-retry reservation.
    assert_eq!(
        transport.methods(),
        vec!["GET", "GET", "POST", "POST", "GET"]
    );
    let record = store.record(&identity).unwrap();
    assert_eq!(record.durable_state(), ChainSubmissionState::Tracking);
    assert_eq!(record.committed_post_reservations(), 3);
}

#[tokio::test(start_paused = true)]
async fn ambiguous_recovery_retry_qualifies_for_a_direct_retry_on_the_next_advance() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    seed_hashless_tree_recovery(&store, &StoreAdvancementRequest::vote(identity.clone()));
    let transport = Arc::new(ScriptedTransport::default());
    for response in tree_responses(&[[8; 32]]) {
        transport.queue(Ok(response));
    }
    transport.queue(Err(ChainTransportError::possibly_dispatched("timeout")));
    let protocol = ChainProtocolClient::new(
        Arc::clone(&transport),
        Network::Testnet,
        &["https://chain.example".to_string()],
    )
    .unwrap();
    let coordinator = Arc::new(
        ChainSubmissionCoordinator::new(
            protocol,
            Arc::clone(&store),
            ManualClock::new(100),
            CoordinatorPolicy::new(Duration::from_secs(10), 2, vec![Duration::from_secs(5)])
                .unwrap(),
        )
        .unwrap(),
    );
    let control = Arc::new(ManualControl::default());
    let task = {
        let coordinator = Arc::clone(&coordinator);
        let control = Arc::clone(&control);
        let identity = identity.clone();
        tokio::spawn(async move {
            coordinator
                .advance_with_recovery(
                    StoreAdvancementRequest::vote(identity),
                    ChainRecoveryMode::ExactTree,
                    &*control,
                )
                .await
        })
    };
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if transport.methods().len() == 3 {
            break;
        }
    }
    assert_eq!(transport.methods(), vec!["GET", "GET", "POST"]);

    // Cancel during the backoff: the ambiguity is already durable.
    control.cancelled.store(true, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(1)).await;
    let result = task.await.unwrap().unwrap();
    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            ref diagnostic,
        }) if diagnostic.kind() == ChainSubmissionDiagnosticKind::AmbiguousDispatch
    ));
    let record = store.record(&identity).unwrap();
    assert_eq!(record.committed_post_reservations(), 2);
    assert!(record.state().permits_ambiguous_retry());

    // The next invocation reserves directly; no tree pass precedes the POST.
    transport.queue(Ok(accepted()));
    transport.queue(Ok(pending()));
    let result = coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking { .. })
    ));
    assert_eq!(
        transport.methods(),
        vec!["GET", "GET", "POST", "POST", "GET"]
    );
    assert_eq!(
        store
            .record(&identity)
            .unwrap()
            .committed_post_reservations(),
        3
    );
}

/// Transport whose POST never completes. `marks_dispatch` decides whether it
/// crosses the handoff boundary before hanging.
struct HangingPostTransport {
    marks_dispatch: bool,
}

impl ChainTransport for HangingPostTransport {
    fn chain_get<'a>(&'a self, _request: ChainHttpRequest) -> ChainTransportFuture<'a> {
        Box::pin(async { unreachable!("no lookup is expected") })
    }

    fn chain_post_json<'a>(
        &'a self,
        _request: ChainHttpRequest,
        _json: Vec<u8>,
    ) -> ChainTransportFuture<'a> {
        Box::pin(async { unreachable!("the dispatch-aware POST is used") })
    }

    fn chain_post_json_with_dispatch<'a>(
        &'a self,
        _request: ChainHttpRequest,
        _json: Vec<u8>,
        dispatch: ChainPostDispatch,
    ) -> ChainTransportFuture<'a> {
        let marks_dispatch = self.marks_dispatch;
        Box::pin(async move {
            if marks_dispatch {
                dispatch.mark_possible();
            }
            std::future::pending().await
        })
    }
}

fn coordinator_over_hanging_post(
    marks_dispatch: bool,
    store: Arc<InMemoryChainSubmissionStore>,
) -> ChainSubmissionCoordinator<Arc<HangingPostTransport>, InMemoryChainSubmissionStore, ManualClock>
{
    let protocol = ChainProtocolClient::new(
        Arc::new(HangingPostTransport { marks_dispatch }),
        Network::Testnet,
        &["https://chain.example".to_string()],
    )
    .unwrap();
    ChainSubmissionCoordinator::new(
        protocol,
        store,
        ManualClock::new(100),
        CoordinatorPolicy::new(Duration::from_secs(10), 1, vec![]).unwrap(),
    )
    .unwrap()
}

#[tokio::test(start_paused = true)]
async fn post_timeout_before_the_dispatch_marker_is_definitely_unsent() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let coordinator = coordinator_over_hanging_post(false, Arc::clone(&store));

    let failure = coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Transport);
    assert!(failure.strongest_state().is_none());
    assert!(store.record(&identity).is_none());
}

#[tokio::test(start_paused = true)]
async fn post_timeout_after_the_dispatch_marker_is_ambiguous() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let coordinator = coordinator_over_hanging_post(true, Arc::clone(&store));

    let result = coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            ref diagnostic,
        }) if diagnostic.kind() == ChainSubmissionDiagnosticKind::AmbiguousDispatch
    ));
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Recovering
    );
}

#[tokio::test]
async fn nullifier_spent_after_unresolved_dispatch_confirms_from_tree_under_exact_tree() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Err(ChainTransportError::possibly_dispatched("timeout")));
    transport.queue(Ok(ChainHttpResponse::json(
        422,
        br#"{"code":2,"log":"nullifier already spent"}"#.to_vec(),
    )));
    for response in tree_responses(&[[8; 32], [3; 32], [4; 32], [7; 32]]) {
        transport.queue(Ok(response));
    }
    let coordinator = coordinator_with_attempts(Arc::clone(&transport), Arc::clone(&store), 2);

    let result = coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap();

    let ChainSubmissionResult::Confirmed(confirmation) = result else {
        panic!("code 2 after ambiguity with a matching layout must confirm")
    };
    assert_eq!(confirmation.final_van_position(), 1);
    assert_eq!(confirmation.vote_commitment_positions(), &[2]);
    assert_eq!(transport.methods(), vec!["POST", "POST", "GET", "GET"]);
    assert!(store.projection(&identity).is_some());
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Confirmed
    );
}

#[tokio::test]
async fn nullifier_spent_after_unresolved_dispatch_with_no_match_is_terminal_under_exact_tree() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Err(ChainTransportError::possibly_dispatched("timeout")));
    transport.queue(Ok(ChainHttpResponse::json(
        422,
        br#"{"code":2,"log":"nullifier already spent"}"#.to_vec(),
    )));
    for response in tree_responses(&[[8; 32]]) {
        transport.queue(Ok(response));
    }
    let coordinator = coordinator_with_attempts(Arc::clone(&transport), Arc::clone(&store), 2);

    let result = coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::SubmittedWithoutHash(ref diagnostic)
            if diagnostic.kind() == ChainSubmissionDiagnosticKind::NullifierAlreadySpent
    ));
    // The no-match authorization was discarded: no further POST.
    assert_eq!(transport.methods(), vec!["POST", "POST", "GET", "GET"]);
    let record = store.record(&identity).unwrap();
    assert_eq!(
        record.durable_state(),
        ChainSubmissionState::SubmittedWithoutHash
    );
    assert_eq!(record.committed_post_reservations(), 2);
    assert!(store.projection(&identity).is_none());

    let before = transport.methods();
    coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap();
    assert_eq!(transport.methods(), before);
}

#[tokio::test]
async fn nullifier_spent_tree_pass_failure_keeps_unresolved_dispatch() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Err(ChainTransportError::possibly_dispatched("timeout")));
    transport.queue(Ok(ChainHttpResponse::json(
        422,
        br#"{"code":2,"log":"nullifier already spent"}"#.to_vec(),
    )));
    transport.queue(Err(ChainTransportError::possibly_dispatched("tree down")));
    let coordinator = coordinator_with_attempts(Arc::clone(&transport), Arc::clone(&store), 2);

    let failure = coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Transport);
    assert_eq!(
        failure.strongest_state().unwrap().state(),
        ChainSubmissionState::Recovering
    );
    let record = store.record(&identity).unwrap();
    assert_eq!(record.durable_state(), ChainSubmissionState::Recovering);
    assert!(record.state().has_unresolved_dispatch());
    assert_eq!(transport.methods(), vec!["POST", "POST", "GET"]);

    // A later pass converges: the scan runs first and confirms.
    for response in tree_responses(&[[8; 32], [3; 32], [4; 32], [7; 32]]) {
        transport.queue(Ok(response));
    }
    let result = coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::vote(identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap();
    assert!(matches!(result, ChainSubmissionResult::Confirmed(_)));
    assert_eq!(
        transport.methods(),
        vec!["POST", "POST", "GET", "GET", "GET"]
    );
}

#[tokio::test]
async fn nonfinal_recovery_hash_collision_continues_bounded_retries() {
    let owner_identity = identity(1, 0);
    let recovering_identity = identity(2, 1);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(owner_identity.clone(), 1));
    store.seed_derivation(derived(recovering_identity.clone(), 2));
    let owner_transport = Arc::new(ScriptedTransport::default());
    owner_transport.queue(Ok(accepted()));
    owner_transport.queue(Ok(pending()));
    coordinator(
        owner_transport,
        Arc::clone(&store),
        ManualClock::new(90),
        10,
    )
    .advance(
        StoreAdvancementRequest::vote(owner_identity),
        &ManualControl::default(),
    )
    .await
    .unwrap();

    let request = StoreAdvancementRequest::vote(recovering_identity.clone());
    seed_hashless_tree_recovery(&store, &request);
    let transport = Arc::new(ScriptedTransport::default());
    for response in tree_responses(&[[8; 32]]) {
        transport.queue(Ok(response));
    }
    transport.queue(Ok(accepted()));
    transport.queue(Ok(accepted_with_hash(&"12".repeat(32))));
    transport.queue(Ok(pending()));
    let coordinator = coordinator_with_attempts(Arc::clone(&transport), Arc::clone(&store), 2);

    let result = coordinator
        .advance_with_recovery(
            request,
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking { .. })
    ));
    assert_eq!(
        transport.methods(),
        vec!["GET", "GET", "POST", "POST", "GET"]
    );
    let record = store.record(&recovering_identity).unwrap();
    assert_eq!(record.durable_state(), ChainSubmissionState::Tracking);
    assert_eq!(record.committed_post_reservations(), 3);
}

#[tokio::test(start_paused = true)]
async fn interruption_inside_a_continued_retry_loop_keeps_the_reservation() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    seed_hashless_tree_recovery(&store, &StoreAdvancementRequest::vote(identity.clone()));
    let transport = Arc::new(ScriptedTransport::default());
    for response in tree_responses(&[[8; 32]]) {
        transport.queue(Ok(response));
    }
    transport.queue(Err(ChainTransportError::possibly_dispatched("timeout")));
    let coordinator = Arc::new(coordinator_with_attempts(
        Arc::clone(&transport),
        Arc::clone(&store),
        3,
    ));
    let control = Arc::new(ManualControl::default());
    let task = {
        let coordinator = Arc::clone(&coordinator);
        let control = Arc::clone(&control);
        let identity = identity.clone();
        tokio::spawn(async move {
            coordinator
                .advance_with_recovery(
                    StoreAdvancementRequest::vote(identity),
                    ChainRecoveryMode::ExactTree,
                    &*control,
                )
                .await
        })
    };
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if transport.methods().len() == 3 {
            break;
        }
    }
    assert_eq!(transport.methods(), vec!["GET", "GET", "POST"]);

    // Hold the continued loop's POST at the transport, then cancel: the
    // durable reservation made for it is kept, and nothing is removed.
    let (entered, _release) = transport.gate_first_post();
    tokio::time::advance(Duration::from_millis(2)).await;
    entered.notified().await;
    control.cancelled.store(true, Ordering::SeqCst);
    tokio::time::advance(Duration::from_millis(50)).await;

    let result = task.await.unwrap().unwrap();
    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            ref diagnostic,
        }) if diagnostic.kind() == ChainSubmissionDiagnosticKind::AmbiguousDispatch
    ));
    let record = store.record(&identity).unwrap();
    assert_eq!(record.durable_state(), ChainSubmissionState::Recovering);
    assert_eq!(record.committed_post_reservations(), 3);
    assert_eq!(transport.methods(), vec!["GET", "GET", "POST", "POST"]);
}

#[tokio::test]
async fn imported_delegation_never_scans_under_exact_tree() {
    let identity = delegation_identity(0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived_imported_delegation(identity.clone()));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(pending()));
    let clock = ManualClock::new(100);
    let coordinator = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        clock.clone(),
        10,
    );
    coordinator
        .advance(
            StoreAdvancementRequest::imported_delegation(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    clock.set(111);
    transport.queue(Ok(pending()));
    let result = coordinator
        .advance_with_recovery(
            StoreAdvancementRequest::imported_delegation(identity.clone()),
            ChainRecoveryMode::ExactTree,
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: Some(_),
            ..
        })
    ));
    // Two status lookups; no tree request and no POST.
    assert_eq!(transport.methods(), vec!["GET", "GET"]);
    assert_eq!(
        store
            .record(&identity)
            .unwrap()
            .committed_post_reservations(),
        0
    );
}

fn coordinator_with_attempts(
    transport: Arc<ScriptedTransport>,
    store: Arc<InMemoryChainSubmissionStore>,
    attempts: usize,
) -> ChainSubmissionCoordinator<Arc<ScriptedTransport>, InMemoryChainSubmissionStore, ManualClock> {
    let protocol = ChainProtocolClient::new(
        transport,
        Network::Testnet,
        &["https://chain.example".to_string()],
    )
    .unwrap();
    ChainSubmissionCoordinator::new(
        protocol,
        store,
        ManualClock::new(100),
        CoordinatorPolicy::new(
            Duration::from_secs(10),
            attempts,
            vec![Duration::from_millis(1); attempts - 1],
        )
        .unwrap(),
    )
    .unwrap()
}

fn derived_imported_delegation(identity: ChainSubmissionIdentity) -> DerivedChainSubmission {
    DerivedChainSubmission::new(
        ChainSubmissionGeneration::new(
            identity,
            ChainSubmissionGenerationDigest::from_bytes([9; 32]),
        ),
        ChainSubmissionRequest::ImportedDelegation(CandidateTransactionHash::from_bytes(
            [0x66; 32],
        )),
        ExpectedTreeLayout::Delegation {
            delegation_van: [4; 32],
        },
        vec![],
    )
}

#[tokio::test]
async fn a_pass_bound_to_an_earlier_epoch_is_refused_under_the_current_one() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    let control = ManualControl::default();
    // The host moved on to epoch 1; the caller's work began under epoch 0.
    control.epoch.store(1, Ordering::SeqCst);

    let failure = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance_in_epoch(
        StoreAdvancementRequest::vote(identity.clone()),
        ChainRecoveryMode::StatusOnly,
        &control,
        0,
    )
    .await
    .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::InvalidInput);
    assert!(
        failure.message().contains("operation epoch changed"),
        "{}",
        failure.message()
    );
    assert!(
        transport.methods().is_empty(),
        "no request may be sent for the stale epoch: {:?}",
        transport.methods()
    );
}
