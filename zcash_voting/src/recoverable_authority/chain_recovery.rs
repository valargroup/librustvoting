//! Finalized-chain reconciliation for one-transaction recoverable ballots.
//!
//! Reconciliation authenticates a bundle's deterministic initial VAN and asks
//! whether that VAN's native nullifier remains unspent or was consumed by one
//! atomic ballot. There is no successor-VAN cursor or proposal mask.

use crate::{
    types::{validate_vote_chain_id, VotingError},
    zkp2::derive_van_nullifier_from_spending_key,
};

use super::RecoverableBundleUseV1;

/// Candidate finalized chain point supplied with reconciliation evidence.
///
/// The external verifier must independently authenticate this block hash and
/// vote-commitment-tree root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryCheckpointV1 {
    vote_chain_id: String,
    height: u64,
    block_hash: [u8; 32],
    vote_commitment_tree_root: [u8; 32],
}

impl RecoveryCheckpointV1 {
    pub fn new(
        vote_chain_id: impl Into<String>,
        height: u64,
        block_hash: [u8; 32],
        vote_commitment_tree_root: [u8; 32],
    ) -> Result<Self, VotingError> {
        let vote_chain_id = vote_chain_id.into();
        validate_vote_chain_id(&vote_chain_id)?;
        Ok(Self {
            vote_chain_id,
            height,
            block_hash,
            vote_commitment_tree_root,
        })
    }

    pub fn vote_chain_id(&self) -> &str {
        &self.vote_chain_id
    }

    pub fn height(&self) -> u64 {
        self.height
    }

    pub fn block_hash(&self) -> &[u8; 32] {
        &self.block_hash
    }

    pub fn vote_commitment_tree_root(&self) -> &[u8; 32] {
        &self.vote_commitment_tree_root
    }
}

/// The bundle's initial VAN plus its authentication path at the checkpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VotingAuthorityVanWitnessV1 {
    van: [u8; 32],
    merkle_path: vote_commitment_tree::MerklePath,
}

impl VotingAuthorityVanWitnessV1 {
    pub fn new(van: [u8; 32], merkle_path: vote_commitment_tree::MerklePath) -> Self {
        Self { van, merkle_path }
    }

    pub fn van(&self) -> &[u8; 32] {
        &self.van
    }

    pub fn position(&self) -> u64 {
        u64::from(self.merkle_path.position())
    }

    pub fn merkle_path(&self) -> &vote_commitment_tree::MerklePath {
        &self.merkle_path
    }
}

/// Stable position of the transaction that consumed the initial authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ConfirmedTransitionPositionV1 {
    block_height: u64,
    transaction_index: u32,
}

impl ConfirmedTransitionPositionV1 {
    pub fn new(block_height: u64, transaction_index: u32) -> Self {
        Self {
            block_height,
            transaction_index,
        }
    }

    pub fn block_height(&self) -> u64 {
        self.block_height
    }

    pub fn transaction_index(&self) -> u32 {
        self.transaction_index
    }
}

/// Chain transaction kind reported for the initial VAN nullifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmedVotingAuthorityConsumerKindV1 {
    /// One atomic batch carrying these ordered proposal IDs.
    AtomicBatch { proposal_ids: Vec<u32> },
    /// A singleton transaction, which recoverable version 1 never creates.
    Singleton { proposal_id: u32 },
    /// A transaction kind that this library cannot classify as a ballot.
    Other,
}

/// The optional confirmed transaction that consumed the initial VAN nullifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmedVotingAuthorityConsumerV1 {
    position: ConfirmedTransitionPositionV1,
    consumer_id: [u8; 32],
    kind: ConfirmedVotingAuthorityConsumerKindV1,
}

impl ConfirmedVotingAuthorityConsumerV1 {
    pub fn new(
        position: ConfirmedTransitionPositionV1,
        consumer_id: [u8; 32],
        kind: ConfirmedVotingAuthorityConsumerKindV1,
    ) -> Self {
        Self {
            position,
            consumer_id,
            kind,
        }
    }

    pub fn position(&self) -> ConfirmedTransitionPositionV1 {
        self.position
    }

    pub fn consumer_id(&self) -> &[u8; 32] {
        &self.consumer_id
    }

    pub fn kind(&self) -> &ConfirmedVotingAuthorityConsumerKindV1 {
        &self.kind
    }
}

/// Untrusted chain inputs to validate at one finalized checkpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VotingAuthorityRecoveryEvidenceV1 {
    finalized_checkpoint: RecoveryCheckpointV1,
    initial_witness: VotingAuthorityVanWitnessV1,
    consumer: Option<ConfirmedVotingAuthorityConsumerV1>,
}

impl VotingAuthorityRecoveryEvidenceV1 {
    pub fn new(
        finalized_checkpoint: RecoveryCheckpointV1,
        initial_witness: Option<VotingAuthorityVanWitnessV1>,
        consumer: Option<ConfirmedVotingAuthorityConsumerV1>,
    ) -> Result<Self, VotingError> {
        let initial_witness =
            initial_witness.ok_or_else(|| invalid_recovery("initial VAN witness is missing"))?;
        Ok(Self {
            finalized_checkpoint,
            initial_witness,
            consumer,
        })
    }

    pub fn finalized_checkpoint(&self) -> &RecoveryCheckpointV1 {
        &self.finalized_checkpoint
    }

    pub fn initial_witness(&self) -> &VotingAuthorityVanWitnessV1 {
        &self.initial_witness
    }

    pub fn consumer(&self) -> Option<&ConfirmedVotingAuthorityConsumerV1> {
        self.consumer.as_ref()
    }
}

/// External trust boundary for finalized and complete nullifier lookup.
///
/// The implementation must authenticate `checkpoint` and prove that
/// `consumer` is the unique confirmed consumer of `initial_consumer_id`
/// through that checkpoint, or prove that no such consumer exists.
pub trait VotingAuthorityRecoveryEvidenceVerifierV1 {
    fn verify_finalized_complete_evidence(
        &self,
        checkpoint: &RecoveryCheckpointV1,
        initial_consumer_id: &[u8; 32],
        consumer: Option<&ConfirmedVotingAuthorityConsumerV1>,
    ) -> Result<(), VotingError>;
}

/// Why a confirmed authority spend cannot be treated as its permitted ballot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsupportedRecoverableBallotSpendV1 {
    Singleton,
    NonCanonicalAtomicBatch,
    Other,
}

/// Finalized status of one recoverable bundle's only ballot opportunity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoverableBallotChainStatusV1 {
    Unspent,
    TerminalAtomicBatch {
        position: ConfirmedTransitionPositionV1,
        proposal_ids: Vec<u32>,
    },
    UnsupportedSpent {
        position: ConfirmedTransitionPositionV1,
        reason: UnsupportedRecoverableBallotSpendV1,
    },
}

/// Reconciled terminal-or-unspent state for one recoverable bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconciledRecoverableBallotV1 {
    initial_leaf_position: u64,
    status: RecoverableBallotChainStatusV1,
    finalized_checkpoint: RecoveryCheckpointV1,
}

impl ReconciledRecoverableBallotV1 {
    pub fn initial_leaf_position(&self) -> u64 {
        self.initial_leaf_position
    }

    pub fn status(&self) -> &RecoverableBallotChainStatusV1 {
        &self.status
    }

    pub fn finalized_checkpoint(&self) -> &RecoveryCheckpointV1 {
        &self.finalized_checkpoint
    }
}

/// Reconciles one bundle without reconstructing a successor authority.
pub fn reconcile_recoverable_ballot_v1(
    authority: RecoverableBundleUseV1<'_>,
    evidence: &VotingAuthorityRecoveryEvidenceV1,
    evidence_verifier: &dyn VotingAuthorityRecoveryEvidenceVerifierV1,
) -> Result<ReconciledRecoverableBallotV1, VotingError> {
    let root = authority.authority_root();
    let round = authority.current_round();
    let expected = authority.expected_material()?;
    if evidence.finalized_checkpoint.vote_chain_id() != round.vote_chain_id() {
        return Err(invalid_recovery(
            "recovery checkpoint vote chain does not match the voting authority",
        ));
    }

    let initial_position = validate_initial_van_witness(
        &evidence.initial_witness,
        &evidence.finalized_checkpoint,
        &expected.van,
    )?;
    let hotkey = root.voting_hotkey()?;
    let initial_consumer_id = derive_van_nullifier_from_spending_key(
        &hotkey.orchard_spending_key(),
        root.context().vote_round_id(),
        &expected.van,
    )?;

    if let Some(consumer) = evidence.consumer() {
        if consumer.position.block_height > evidence.finalized_checkpoint.height {
            return Err(invalid_recovery(
                "confirmed authority consumer lies after the finalized checkpoint",
            ));
        }
        if consumer.consumer_id != initial_consumer_id {
            return Err(invalid_recovery(
                "confirmed authority consumer does not match the native initial VAN nullifier",
            ));
        }
    }
    evidence_verifier.verify_finalized_complete_evidence(
        &evidence.finalized_checkpoint,
        &initial_consumer_id,
        evidence.consumer(),
    )?;

    let status = match evidence.consumer() {
        None => RecoverableBallotChainStatusV1::Unspent,
        Some(consumer) => match consumer.kind() {
            ConfirmedVotingAuthorityConsumerKindV1::AtomicBatch { proposal_ids }
                if canonical_ballot_ids(proposal_ids, round.proposal_count()) =>
            {
                RecoverableBallotChainStatusV1::TerminalAtomicBatch {
                    position: consumer.position(),
                    proposal_ids: proposal_ids.clone(),
                }
            }
            ConfirmedVotingAuthorityConsumerKindV1::AtomicBatch { .. } => {
                RecoverableBallotChainStatusV1::UnsupportedSpent {
                    position: consumer.position(),
                    reason: UnsupportedRecoverableBallotSpendV1::NonCanonicalAtomicBatch,
                }
            }
            ConfirmedVotingAuthorityConsumerKindV1::Singleton { .. } => {
                RecoverableBallotChainStatusV1::UnsupportedSpent {
                    position: consumer.position(),
                    reason: UnsupportedRecoverableBallotSpendV1::Singleton,
                }
            }
            ConfirmedVotingAuthorityConsumerKindV1::Other => {
                RecoverableBallotChainStatusV1::UnsupportedSpent {
                    position: consumer.position(),
                    reason: UnsupportedRecoverableBallotSpendV1::Other,
                }
            }
        },
    };

    Ok(ReconciledRecoverableBallotV1 {
        initial_leaf_position: initial_position,
        status,
        finalized_checkpoint: evidence.finalized_checkpoint.clone(),
    })
}

fn canonical_ballot_ids(proposal_ids: &[u32], proposal_count: u32) -> bool {
    !proposal_ids.is_empty()
        && proposal_ids
            .iter()
            .all(|proposal_id| (1..=proposal_count).contains(proposal_id))
        && proposal_ids.windows(2).all(|pair| pair[0] < pair[1])
}

fn validate_initial_van_witness(
    witness: &VotingAuthorityVanWitnessV1,
    checkpoint: &RecoveryCheckpointV1,
    expected_van: &[u8; 32],
) -> Result<u64, VotingError> {
    let position = witness.position();
    if position >= (1u64 << crate::vote::VAN_AUTH_PATH_LEN) {
        return Err(invalid_recovery(
            "VAN witness position exceeds the VCT capacity",
        ));
    }
    let van = crate::governance::bytes_to_fp(witness.van())?;
    let root = crate::governance::bytes_to_fp(checkpoint.vote_commitment_tree_root())?;
    if !witness.merkle_path.verify(van, root) {
        return Err(invalid_recovery(
            "VAN witness does not authenticate to the recovery checkpoint root",
        ));
    }
    if witness.van() != expected_van {
        return Err(invalid_recovery(
            "root-validated initial VAN leaf does not match the recoverable authority and bundle",
        ));
    }
    Ok(position)
}

fn invalid_recovery(message: impl Into<String>) -> VotingError {
    VotingError::InvalidInput {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        backend::pasta_curves::{group::ff::PrimeField, pallas},
        governance::BALLOT_DIVISOR,
        recoverable_authority::{
            test_validated_recoverable_voting_round_v1, RecoverableBundleMaterialV1,
            RecoverableSelfCustodyBundleV1, RegisteredKeyApplicationV1,
            SoftwareRegisteredKeyRequestV1, ValidatedRecoverableVotingRoundV1,
            VotingAuthorityContextV1, VotingAuthorityRootBindingV1, VotingAuthorityRootV1,
        },
        types::{Network, NoteInfo},
        wire::VotingRoundParams,
    };
    use vote_commitment_tree::MemoryTreeServer;

    const ROUND_ID: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    const CHAIN_ID: &str = "vote-chain-test";
    const CHECKPOINT_HEIGHT: u32 = 100;

    struct Fixture {
        root: VotingAuthorityRootV1,
        binding: VotingAuthorityRootBindingV1,
        round: ValidatedRecoverableVotingRoundV1,
        bundle: RecoverableSelfCustodyBundleV1,
    }

    impl Fixture {
        fn new() -> Self {
            let context = VotingAuthorityContextV1::from_fingerprint(
                Network::Testnet,
                0,
                [0x22; 32],
                CHAIN_ID,
                [0x01; 32],
            )
            .unwrap();
            let request = SoftwareRegisteredKeyRequestV1::new(
                RegisteredKeyApplicationV1::new(0xA11C),
                context,
            );
            let root = VotingAuthorityRootV1::from_registered_key_output(&request, [0x55; 64]);
            let binding = VotingAuthorityRootBindingV1::bind(&root);
            let round = test_validated_recoverable_voting_round_v1(
                Network::Testnet,
                CHAIN_ID,
                round_params(),
                [0xAB; 32],
                3,
            );
            let bundle =
                RecoverableSelfCustodyBundleV1::from_canonical_bundle(0, vec![note()]).unwrap();
            Self {
                root,
                binding,
                round,
                bundle,
            }
        }

        fn authority(&self) -> RecoverableBundleUseV1<'_> {
            RecoverableBundleUseV1::new(
                &self.root,
                &self.binding,
                &self.round,
                RecoverableBundleMaterialV1::RecoverableSelfCustody(self.bundle.identity()),
            )
            .unwrap()
        }

        fn initial_van(&self) -> [u8; 32] {
            self.authority().expected_material().unwrap().van
        }

        fn initial_consumer_id(&self) -> [u8; 32] {
            let spending_key = self.root.voting_hotkey().unwrap().orchard_spending_key();
            derive_van_nullifier_from_spending_key(
                &spending_key,
                self.root.context().vote_round_id(),
                &self.initial_van(),
            )
            .unwrap()
        }

        fn evidence(
            &self,
            consumer: Option<ConfirmedVotingAuthorityConsumerV1>,
        ) -> VotingAuthorityRecoveryEvidenceV1 {
            let initial_van = self.initial_van();
            let leaf = Option::<pallas::Base>::from(pallas::Base::from_repr(initial_van)).unwrap();
            let mut tree = MemoryTreeServer::empty();
            tree.append(pallas::Base::from(9)).unwrap();
            let position = tree.append(leaf).unwrap();
            tree.checkpoint(CHECKPOINT_HEIGHT).unwrap();
            let path = tree.path(position, CHECKPOINT_HEIGHT).unwrap();
            let checkpoint = RecoveryCheckpointV1::new(
                CHAIN_ID,
                CHECKPOINT_HEIGHT as u64,
                [0xBC; 32],
                tree.root().to_repr(),
            )
            .unwrap();
            VotingAuthorityRecoveryEvidenceV1::new(
                checkpoint,
                Some(VotingAuthorityVanWitnessV1::new(initial_van, path)),
                consumer,
            )
            .unwrap()
        }
    }

    fn round_params() -> VotingRoundParams {
        VotingRoundParams {
            vote_round_id: ROUND_ID.to_string(),
            snapshot_height: 1000,
            ea_pk: vec![0xEA; 32],
            nc_root: vec![0xAA; 32],
            nullifier_imt_root: vec![0xBB; 32],
        }
    }

    fn note() -> NoteInfo {
        NoteInfo {
            commitment: vec![0x01; 32],
            nullifier: vec![0x02; 32],
            value: BALLOT_DIVISOR,
            position: 0,
            diversifier: vec![0x03; 11],
            rho: vec![0x04; 32],
            rseed: vec![0x05; 32],
            scope: 0,
            ufvk_str: "uview1test".to_string(),
        }
    }

    struct AcceptingVerifier {
        expected_consumer_id: [u8; 32],
    }

    impl VotingAuthorityRecoveryEvidenceVerifierV1 for AcceptingVerifier {
        fn verify_finalized_complete_evidence(
            &self,
            _checkpoint: &RecoveryCheckpointV1,
            initial_consumer_id: &[u8; 32],
            _consumer: Option<&ConfirmedVotingAuthorityConsumerV1>,
        ) -> Result<(), VotingError> {
            assert_eq!(initial_consumer_id, &self.expected_consumer_id);
            Ok(())
        }
    }

    struct RejectingVerifier;

    impl VotingAuthorityRecoveryEvidenceVerifierV1 for RejectingVerifier {
        fn verify_finalized_complete_evidence(
            &self,
            _checkpoint: &RecoveryCheckpointV1,
            _initial_consumer_id: &[u8; 32],
            _consumer: Option<&ConfirmedVotingAuthorityConsumerV1>,
        ) -> Result<(), VotingError> {
            Err(invalid_recovery("finalized lookup is incomplete"))
        }
    }

    #[test]
    fn reconciliation_is_binary_for_canonical_atomic_ballots() {
        let fixture = Fixture::new();
        let verifier = AcceptingVerifier {
            expected_consumer_id: fixture.initial_consumer_id(),
        };

        let unspent = reconcile_recoverable_ballot_v1(
            fixture.authority(),
            &fixture.evidence(None),
            &verifier,
        )
        .unwrap();
        assert_eq!(unspent.initial_leaf_position(), 1);
        assert_eq!(unspent.status(), &RecoverableBallotChainStatusV1::Unspent);

        let position = ConfirmedTransitionPositionV1::new(99, 4);
        let consumer = ConfirmedVotingAuthorityConsumerV1::new(
            position,
            fixture.initial_consumer_id(),
            ConfirmedVotingAuthorityConsumerKindV1::AtomicBatch {
                proposal_ids: vec![1, 3],
            },
        );
        let terminal = reconcile_recoverable_ballot_v1(
            fixture.authority(),
            &fixture.evidence(Some(consumer)),
            &verifier,
        )
        .unwrap();
        assert_eq!(
            terminal.status(),
            &RecoverableBallotChainStatusV1::TerminalAtomicBatch {
                position,
                proposal_ids: vec![1, 3],
            }
        );
    }

    #[test]
    fn reconciliation_marks_noncanonical_spends_unsupported() {
        let fixture = Fixture::new();
        let verifier = AcceptingVerifier {
            expected_consumer_id: fixture.initial_consumer_id(),
        };
        let position = ConfirmedTransitionPositionV1::new(99, 4);
        let consumer = ConfirmedVotingAuthorityConsumerV1::new(
            position,
            fixture.initial_consumer_id(),
            ConfirmedVotingAuthorityConsumerKindV1::AtomicBatch {
                proposal_ids: vec![1, 1],
            },
        );

        let reconciled = reconcile_recoverable_ballot_v1(
            fixture.authority(),
            &fixture.evidence(Some(consumer)),
            &verifier,
        )
        .unwrap();
        assert_eq!(
            reconciled.status(),
            &RecoverableBallotChainStatusV1::UnsupportedSpent {
                position,
                reason: UnsupportedRecoverableBallotSpendV1::NonCanonicalAtomicBatch,
            }
        );

        let singleton = ConfirmedVotingAuthorityConsumerV1::new(
            position,
            fixture.initial_consumer_id(),
            ConfirmedVotingAuthorityConsumerKindV1::Singleton { proposal_id: 1 },
        );
        let reconciled = reconcile_recoverable_ballot_v1(
            fixture.authority(),
            &fixture.evidence(Some(singleton)),
            &verifier,
        )
        .unwrap();
        assert_eq!(
            reconciled.status(),
            &RecoverableBallotChainStatusV1::UnsupportedSpent {
                position,
                reason: UnsupportedRecoverableBallotSpendV1::Singleton,
            }
        );
    }

    #[test]
    fn reconciliation_requires_authenticated_complete_chain_evidence() {
        let fixture = Fixture::new();
        let evidence = fixture.evidence(None);
        let err =
            reconcile_recoverable_ballot_v1(fixture.authority(), &evidence, &RejectingVerifier)
                .unwrap_err();
        assert!(err.to_string().contains("lookup is incomplete"), "{err}");

        let wrong_consumer = ConfirmedVotingAuthorityConsumerV1::new(
            ConfirmedTransitionPositionV1::new(99, 4),
            [0xFF; 32],
            ConfirmedVotingAuthorityConsumerKindV1::AtomicBatch {
                proposal_ids: vec![1, 3],
            },
        );
        let err = reconcile_recoverable_ballot_v1(
            fixture.authority(),
            &fixture.evidence(Some(wrong_consumer)),
            &AcceptingVerifier {
                expected_consumer_id: fixture.initial_consumer_id(),
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("native initial VAN nullifier"),
            "{err}"
        );
    }

    #[test]
    fn recovery_evidence_rejects_a_missing_initial_witness() {
        let checkpoint = RecoveryCheckpointV1::new(CHAIN_ID, 100, [0xBC; 32], [0xCD; 32]).unwrap();
        let err = VotingAuthorityRecoveryEvidenceV1::new(checkpoint, None, None).unwrap_err();
        assert!(
            err.to_string().contains("initial VAN witness is missing"),
            "{err}"
        );
    }
}
