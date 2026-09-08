//! Real cast inputs with a cached delegation proof and scripted remote peers.
//! ZKP1 proving is covered separately; this driver verifies a real SpendAuth
//! signature and the executor generates real ZKP2 proofs.

use super::super::fixtures::{executor_ready_to_cast, ROUND_ID};
use crate::chain_submission::{ChainHttpRequest, ChainHttpResponse, ChainTransportFuture};
use crate::delegate::{DelegationProofStatus, SignedDelegationBundle};
use crate::delegation_pipeline::{DelegationDriver, DelegationSigner};
use crate::helper::transport::{HelperFuture, HelperResponse, HelperTransport};
use crate::{
    backend::pasta_curves::{
        group::{ff::PrimeField, Group, GroupEncoding},
        pallas,
    },
    *,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

pub(super) const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

pub(super) struct Driver {
    pub db: Arc<round::VotingDb>,
    pub signature: [u8; 64],
    pub prepared: AtomicUsize,
    pub signed: AtomicUsize,
}

pub(super) fn database() -> (Arc<round::VotingDb>, Arc<Driver>) {
    use crate::backend::orchard::{
        keys::SpendAuthorizingKey,
        primitives::redpallas::{SpendAuth, VerificationKey},
    };
    use crate::backend::zcash_keys::keys::UnifiedSpendingKey;
    let executor = executor_ready_to_cast("wallet").0;
    let db = executor.database();
    let key = UnifiedSpendingKey::from_seed(&Network::Regtest, &[0x42; 64], zip32::AccountId::ZERO)
        .unwrap();
    let alpha = pallas::Scalar::from(7);
    let randomized = SpendAuthorizingKey::from(key.orchard()).randomize(&alpha);
    let rk: [u8; 32] = (&VerificationKey::<SpendAuth>::from(&randomized)).into();
    let signature = randomized.sign(voting_crypto_deps::rand::rngs::OsRng, &[0x49; 32]);
    {
        let conn = db.conn();
        conn.execute(
            "UPDATE bundles SET delegation_tx_hash=NULL, van_leaf_position=NULL, alpha=?1, dummy_nullifiers=?2, padded_note_data=?3, padded_note_secrets=?4",
            rusqlite::params![alpha.to_repr().to_vec(), vec![0x13u8; 32 * 4], vec![0x14u8; 32 * 4], vec![0x15u8; 64 * 4]],
        )
        .unwrap();
        conn.execute(
            "UPDATE rounds SET ea_pk=?1",
            [pallas::Point::generator().to_bytes().to_vec()],
        )
        .unwrap();
        let inputs = storage::queries::load_zkp2_inputs(&conn, ROUND_ID, "wallet", 0).unwrap();
        let (nf, cmx, van): (Vec<u8>, Vec<u8>, Vec<u8>) = conn
            .query_row(
                "SELECT nf_signed, cmx_new, gov_comm FROM bundles",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(inputs.total_note_value, governance::BALLOT_DIVISOR);
        storage::queries::store_proof_result_fields_with_van_comm(
            &conn,
            ROUND_ID,
            "wallet",
            0,
            &rk,
            &vec![vec![0x12; 32]; governance::BUNDLE_NOTE_SLOTS],
            &nf,
            &cmx,
            &van,
        )
        .unwrap();
    }
    let driver = Arc::new(Driver {
        db: db.clone(),
        signature: (&signature).into(),
        prepared: AtomicUsize::new(0),
        signed: AtomicUsize::new(0),
    });
    (db, driver)
}

impl DelegationDriver for Driver {
    fn round_id(&self) -> &str {
        ROUND_ID
    }
    fn network(&self) -> Network {
        Network::Regtest
    }
    fn wallet_id(&self) -> &str {
        "wallet"
    }
    fn delegation_target(&self) -> Option<VotingHotkeyTarget> {
        Some(
            VotingHotkey::from_stored_secret(&[0x21; 64], Network::Regtest)
                .unwrap()
                .delegation_target(),
        )
    }
    fn shares_database_with(&self, db: &round::VotingDb) -> bool {
        self.db.shares_connection_with(db)
    }
    fn prepare_blocking(
        &self,
        bundle: u32,
        _: &PirFleet,
        _: &dyn types::DelegationProgressReporter,
    ) -> Result<DelegationProofStatus, VotingError> {
        self.prepared.fetch_add(1, Ordering::SeqCst);
        storage::queries::store_proof(&self.db.conn(), ROUND_ID, "wallet", bundle, &[0x61; 96])?;
        Ok(DelegationProofStatus::Reused)
    }
    fn prove_and_sign_blocking(
        &self,
        bundle: u32,
        _: &DelegationSigner,
        _: &PirFleet,
        _: &dyn types::DelegationProgressReporter,
    ) -> Result<SignedDelegationBundle, VotingError> {
        self.signed.fetch_add(1, Ordering::SeqCst);
        Ok(SignedDelegationBundle {
            submission: delegate::submission_with_conn(
                &self.db.conn(),
                "wallet",
                ROUND_ID,
                bundle,
                self.signature,
            )?,
            pczt_bytes: Vec::new(),
            eligible_weight_zatoshi: governance::BALLOT_DIVISOR,
            delegated_weight_zatoshi: governance::BALLOT_DIVISOR,
            bundle_count: 1,
            bundle_index: bundle,
        })
    }
    fn resign_blocking(&self, _: u32, _: &DelegationSigner) -> Result<[u8; 64], VotingError> {
        panic!("combined recovery must not resign delegation")
    }
}

pub(super) fn context(driver: Arc<Driver>) -> RoundHostContext {
    let mut host = super::super::fixtures::host_with_delegation(
        &ChainSubmissionControl::new(0),
        "wallet",
        &driver.db,
    );
    host.delegation.as_mut().unwrap().driver = driver;
    // Last-moment mode produces one immediate share per proposal.
    host.now_seconds = 99_999;
    host
}

pub(super) fn executor(
    db: Arc<round::VotingDb>,
    peers: Arc<Peers>,
    signing: bool,
) -> RoundExecutor<std::sync::Arc<Peers>> {
    RoundExecutor::with_transport(
        db,
        peers.clone(),
        ChainSubmissionClientConfig::for_network(
            Network::Regtest,
            vec!["http://chain.invalid".into()],
        ),
        HelperClient::new(peers, HelperHealth::default()),
    )
    .unwrap()
    .with_binding(RoundBinding {
        round_id: ROUND_ID.into(),
        network: Network::Regtest,
        proposals: vec![
            ProposalRosterEntry {
                proposal_id: 1,
                num_options: 2,
            },
            ProposalRosterEntry {
                proposal_id: 2,
                num_options: 3,
            },
        ],
        hotkey_secret: signing.then(|| zeroize::Zeroizing::new(vec![0x21; 64])),
    })
    .unwrap()
}

pub(super) struct Peers {
    pub db: Arc<round::VotingDb>,
    pub posts: Mutex<Vec<Vec<u8>>>,
    pub deliveries: AtomicUsize,
    /// How many leading chain POSTs answer with a definite rejection.
    pub rejections: AtomicUsize,
}

impl Peers {
    pub fn new(db: Arc<round::VotingDb>) -> Arc<Self> {
        Arc::new(Self {
            db,
            posts: Mutex::new(Vec::new()),
            deliveries: AtomicUsize::new(0),
            rejections: AtomicUsize::new(0),
        })
    }
}

impl ChainTransport for Peers {
    fn chain_post_json<'a>(
        &'a self,
        request: ChainHttpRequest,
        json: Vec<u8>,
    ) -> ChainTransportFuture<'a> {
        assert!(
            request.url().ends_with("/delegate-and-cast-vote-batch"),
            "unexpected chain POST: {}",
            request.url()
        );
        assert_eq!(self.deliveries.load(Ordering::SeqCst), 0);
        let envelope: wire::DelegateAndVoteBatchWire = serde_json::from_slice(&json).unwrap();
        let digest = envelope.authorization_digest().unwrap();
        for vote in &envelope.batch.votes {
            assert_eq!(
                self.db.vote_phase(ROUND_ID, 0, vote.proposal_id).unwrap(),
                phases::VotePhase::SubmissionManaged
            );
        }
        self.posts.lock().unwrap().push(json);
        let rejected = self
            .rejections
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        Box::pin(async move {
            if rejected {
                return Ok(ChainHttpResponse::json(
                    422,
                    serde_json::to_vec(&serde_json::json!({"code":7,"log":"round closed","batch_digest":hex::encode(digest)})).unwrap(),
                ));
            }
            Ok(ChainHttpResponse::json(200, serde_json::to_vec(&serde_json::json!({"tx_hash":HASH,"code":0,"batch_digest":hex::encode(digest)})).unwrap()))
        })
    }
    fn chain_get<'a>(&'a self, request: ChainHttpRequest) -> ChainTransportFuture<'a> {
        assert!(
            request.url().contains(HASH),
            "unexpected chain GET: {}",
            request.url()
        );
        let posts = self.posts.lock().unwrap();
        let envelope: wire::DelegateAndVoteBatchWire =
            serde_json::from_slice(posts.last().unwrap()).unwrap();
        let attrs = [
            ("round_id", ROUND_ID.to_owned()),
            ("nullifier_count", governance::BUNDLE_NOTE_SLOTS.to_string()),
            (
                "batch_digest",
                hex::encode(envelope.authorization_digest().unwrap()),
            ),
            ("batch_size", envelope.batch.votes.len().to_string()),
            ("final_van_leaf_index", "7".into()),
            (
                "vote_commitment_leaf_indices",
                (0..envelope.batch.votes.len())
                    .map(|i| (8 + i).to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            (
                "proposal_ids",
                envelope
                    .batch
                    .votes
                    .iter()
                    .map(|v| v.proposal_id.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            (
                "van_nullifiers",
                envelope
                    .batch
                    .votes
                    .iter()
                    .map(|v| hex::encode(STANDARD.decode(&v.van_nullifier).unwrap()))
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        ];
        let json = serde_json::to_vec(&serde_json::json!({"height":"10","code":0,"log":"","events":[{"type":"delegate_and_cast_vote_batch","attributes":attrs.into_iter().map(|(key,value)| serde_json::json!({"key":key,"value":value})).collect::<Vec<_>>()}]})).unwrap();
        Box::pin(async move { Ok(ChainHttpResponse::json(200, json)) })
    }
}

impl HelperTransport for Peers {
    fn get<'a>(&'a self, _: &'a str, _: Duration) -> HelperFuture<'a> {
        Box::pin(async { Ok(HelperResponse::json(200, br#"{"status":"ok"}"#.to_vec())) })
    }
    fn post_json<'a>(&'a self, _: &'a str, _: Vec<u8>, _: Duration) -> HelperFuture<'a> {
        assert_eq!(
            self.db.delegation_phase(ROUND_ID, 0).unwrap(),
            phases::DelegationPhase::Confirmed
        );
        for proposal in [1, 2] {
            assert_eq!(
                self.db.vote_phase(ROUND_ID, 0, proposal).unwrap(),
                phases::VotePhase::Confirmed
            );
        }
        self.deliveries.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(HelperResponse::json(
                200,
                br#"{"status":"queued"}"#.to_vec(),
            ))
        })
    }
}
