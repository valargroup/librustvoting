//! Behavior-oriented conformance tests for one bounded lifecycle pass.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::Notify;

use super::*;
use crate::{
    chain_submission::{
        generation::{ChainSubmissionRequest, ExpectedTreeLayout},
        protocol::ChainProtocolClient,
        store::memory::InMemoryChainSubmissionStore,
        ChainHttpRequest, ChainHttpResponse, ChainSubmissionGeneration,
        ChainSubmissionGenerationDigest, ChainSubmissionIdentity, ChainSubmissionPending,
        ChainSubmissionStateEvidence, ChainSubmissionTarget, ChainTransportError,
        ChainTransportFuture,
    },
    confirmation::{TxEvent, TxEventAttribute},
    delegate::DelegationSigner,
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

impl ChainSubmissionClock for ManualClock {
    fn now_seconds(&self) -> Result<u64, ChainSubmissionFailure> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

#[derive(Default)]
struct ManualControl {
    cancelled: AtomicBool,
    epoch: AtomicU64,
}

impl ChainSubmissionControl for ManualControl {
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

impl ChainSubmissionControl for CancelOnCheck {
    fn is_cancelled(&self) -> bool {
        self.checks.fetch_add(1, Ordering::SeqCst) + 1 >= self.cancel_at
    }

    fn operation_epoch(&self) -> u64 {
        0
    }
}

type TransportReply = Result<ChainHttpResponse, ChainTransportError>;

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
    reservation_probe: Mutex<Option<(Arc<InMemoryChainSubmissionStore>, ChainSubmissionIdentity)>>,
    post_gate: Mutex<Option<PostGate>>,
    fail_classification_store: Mutex<Option<Arc<InMemoryChainSubmissionStore>>>,
    fail_reconciliation_store: Mutex<Option<Arc<InMemoryChainSubmissionStore>>>,
}

impl ScriptedTransport {
    fn queue(&self, reply: TransportReply) {
        self.replies.lock().unwrap().push_back(reply);
    }

    fn methods(&self) -> Vec<&'static str> {
        self.methods.lock().unwrap().clone()
    }

    fn require_reservation_before_post(
        &self,
        store: Arc<InMemoryChainSubmissionStore>,
        identity: ChainSubmissionIdentity,
    ) {
        *self.reservation_probe.lock().unwrap() = Some((store, identity));
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

    fn begin_request(&self, method: &'static str) {
        if method == "POST" {
            if let Some((store, identity)) = self.reservation_probe.lock().unwrap().as_ref() {
                assert!(matches!(
                    store.record(identity).unwrap().state(),
                    SubmissionRecordState::Submitting
                ));
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
    fn chain_get<'a>(&'a self, _request: ChainHttpRequest) -> ChainTransportFuture<'a> {
        Box::pin(async move {
            self.begin_request("GET");
            if let Some(store) = self.fail_reconciliation_store.lock().unwrap().take() {
                store.fail_next_commit();
            }
            self.take_reply()
        })
    }

    fn chain_post_json<'a>(
        &'a self,
        _request: ChainHttpRequest,
        _json: Vec<u8>,
    ) -> ChainTransportFuture<'a> {
        Box::pin(async move {
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
        "chain",
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
        "chain",
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
        "chain",
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
            van_nullifier: hex::encode([2; 32]),
            vote_authority_note_new: hex::encode([3; 32]),
            vote_commitment: hex::encode([4; 32]),
            proposal_id: *proposal_id,
            proof: hex::encode([5; 8]),
            vote_round_id: hex::encode(identity.vote_round_id()),
            anchor_height: 1,
            r_vpk: hex::encode([6; 32]),
            vote_auth_sig: hex::encode([7; 64]),
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

fn accepted() -> ChainHttpResponse {
    ChainHttpResponse::json(
        200,
        format!(r#"{{"tx_hash":"{HASH}","code":0,"log":""}}"#).into_bytes(),
    )
}

fn pending() -> ChainHttpResponse {
    ChainHttpResponse::json(404, br#"{"error":"tx not found"}"#.to_vec())
}

fn rejected() -> ChainHttpResponse {
    ChainHttpResponse::json(
        422,
        br#"{"tx_hash":null,"code":17,"log":"sensitive server log"}"#.to_vec(),
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
            StoreAdvancementRequest::delegation(
                identity.clone(),
                DelegationSigner::signature([7; 64], [8; 32]),
            ),
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
async fn possible_dispatch_is_sticky_recovery_and_never_redispatches() {
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

    let first = coordinator
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();
    let second = coordinator
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
            ..
        })
    ));
    assert!(matches!(
        second,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering {
            candidate_transaction_hash: None,
            ..
        })
    ));
    assert_eq!(transport.methods(), vec!["POST"]);
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
async fn cancelled_batch_entry_preserves_requested_member_guard_without_roster_read() {
    let batch = batch_identity(0);
    let guarded_member = identity(2, 0);
    let cases = [
        (
            StoredChainSubmission::legacy_confirmed(guarded_member.clone(), 4, 5, 1),
            ChainSubmissionState::LegacyConfirmed,
        ),
        (
            StoredChainSubmission::digestless_guard(guarded_member, 1),
            ChainSubmissionState::Recovering,
        ),
    ];

    for (guard, expected_state) in cases {
        let store = Arc::new(InMemoryChainSubmissionStore::default());
        store.seed_record(guard);
        let transport = Arc::new(ScriptedTransport::default());
        let control = ManualControl::default();
        control.cancelled.store(true, Ordering::SeqCst);

        let failure = coordinator(
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
        .unwrap_err();

        assert_eq!(failure.kind(), ChainSubmissionFailureKind::InvalidInput);
        assert_eq!(failure.strongest_state().unwrap().state(), expected_state);
        assert_eq!(store.batch_roster_reads(), 0);
        assert!(store.record(&batch).is_none());
        assert!(transport.methods().is_empty());
    }
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
async fn migration_guards_return_before_derivation_or_network_work() {
    let legacy_identity = identity(1, 0);
    let digestless_identity = identity(2, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_record(StoredChainSubmission::legacy_confirmed(
        legacy_identity.clone(),
        4,
        5,
        1,
    ));
    store.seed_record(StoredChainSubmission::digestless_guard(
        digestless_identity.clone(),
        1,
    ));
    let transport = Arc::new(ScriptedTransport::default());
    let coordinator = coordinator(Arc::clone(&transport), store, ManualClock::new(100), 10);

    let legacy = coordinator
        .advance(
            StoreAdvancementRequest::vote(legacy_identity),
            &ManualControl::default(),
        )
        .await
        .unwrap();
    let digestless = coordinator
        .advance(
            StoreAdvancementRequest::vote(digestless_identity),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(
        matches!(legacy, ChainSubmissionResult::Confirmed(ref confirmation) if confirmation.source() == super::super::ChainSubmissionConfirmationSource::LegacyProjection)
    );
    assert!(
        matches!(digestless, ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering { candidate_transaction_hash: None, ref diagnostic }) if diagnostic.kind() == ChainSubmissionDiagnosticKind::RecoveryUnavailable)
    );
    assert!(transport.methods().is_empty());
}

#[tokio::test]
async fn deterministic_rejection_is_terminal_and_redacted() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(rejected()));

    let result = coordinator(transport, Arc::clone(&store), ManualClock::new(100), 10)
        .advance(
            StoreAdvancementRequest::vote(identity.clone()),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(
        matches!(result, ChainSubmissionResult::Rejected(ref diagnostic) if !diagnostic.message().contains("sensitive"))
    );
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Rejected
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
async fn terminal_state_is_idempotent_only_for_the_same_generation() {
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
    assert!(matches!(replay, ChainSubmissionResult::Rejected(_)));

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

#[tokio::test]
async fn guarded_batch_is_rejected_before_derivation_or_dispatch() {
    let member_two = identity(2, 0);
    let batch = batch_identity(0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_batch_roster(batch.clone(), vec![1, 2]);
    store.seed_record(StoredChainSubmission::digestless_guard(
        member_two.clone(),
        1,
    ));
    let transport = Arc::new(ScriptedTransport::default());

    let failure = coordinator(
        Arc::clone(&transport),
        Arc::clone(&store),
        ManualClock::new(100),
        10,
    )
    .advance(
        StoreAdvancementRequest::vote_batch(batch.clone(), vec![1, 2]).unwrap(),
        &ManualControl::default(),
    )
    .await
    .unwrap_err();

    assert_eq!(failure.kind(), ChainSubmissionFailureKind::InvalidInput);
    assert_eq!(
        failure.strongest_state().unwrap().state(),
        ChainSubmissionState::Recovering
    );
    assert!(store.record(&batch).is_none());
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
    assert_eq!(failure.kind(), ChainSubmissionFailureKind::Protocol);
    assert_eq!(store.batch_roster_reads(), 1);
    assert!(store.record(&batch).is_none());
    assert!(transport.methods().is_empty());
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
async fn omitted_batch_member_fails_before_its_unlocked_legacy_guard_is_read() {
    let batch = batch_identity(0);
    let member_one = identity(1, 0);
    let guarded_member = identity(2, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived_batch(batch.clone(), vec![1, 2]));
    store.seed_record(StoredChainSubmission::digestless_guard(
        guarded_member.clone(),
        1,
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
    .expect_err("omitted guarded member must fail");

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
async fn committed_failure_rejects_tracking_but_only_clears_recovery_candidate() {
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
        ChainSubmissionResult::Rejected(_)
    ));

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
async fn definitely_unsent_failover_is_bounded_and_ambiguity_stops_it() {
    let identity = identity(1, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_derivation(derived(identity.clone(), 1));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Err(ChainTransportError::definitely_unsent("first refused")));
    transport.queue(Err(ChainTransportError::possibly_dispatched(
        "second timed out",
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
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering { .. })
    ));
    assert_eq!(transport.methods(), vec!["POST", "POST"]);
    assert_eq!(
        store
            .record(&identity)
            .unwrap()
            .committed_post_reservations(),
        2
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
async fn candidate_hash_reuse_across_generations_fails_closed_without_lookup() {
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
    let result = coordinator(
        Arc::clone(&second_transport),
        Arc::clone(&store),
        ManualClock::new(101),
        10,
    )
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
            ..
        })
    ));
    assert_eq!(second_transport.methods(), vec!["POST"]);
    assert_eq!(
        store
            .record(&second_identity)
            .unwrap()
            .diagnostic()
            .unwrap()
            .kind(),
        ChainSubmissionDiagnosticKind::InvalidProtocolResponse
    );
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
async fn legacy_confirmed_predecessor_allows_the_next_bundle_generation() {
    let first_identity = identity(1, 0);
    let second_identity = identity(2, 0);
    let store = Arc::new(InMemoryChainSubmissionStore::default());
    store.seed_record(StoredChainSubmission::legacy_confirmed(
        first_identity,
        4,
        5,
        1,
    ));
    store.seed_derivation(derived(second_identity.clone(), 2));
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(accepted()));
    transport.queue(Ok(pending()));

    let result = coordinator(Arc::clone(&transport), store, ManualClock::new(100), 10)
        .advance(
            StoreAdvancementRequest::vote(second_identity),
            &ManualControl::default(),
        )
        .await
        .unwrap();

    assert!(matches!(
        result,
        ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking { .. })
    ));
    assert_eq!(transport.methods(), vec!["POST", "GET"]);
}

#[tokio::test]
async fn cancellation_during_dispatch_persists_recovery() {
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
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering { .. })
    ));
    assert_eq!(
        store.record(&identity).unwrap().durable_state(),
        ChainSubmissionState::Recovering
    );
    assert_eq!(transport.methods(), vec!["POST"]);
}

#[tokio::test]
async fn operation_epoch_change_during_dispatch_persists_recovery() {
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
        ChainSubmissionResult::Pending(ChainSubmissionPending::Recovering { .. })
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
