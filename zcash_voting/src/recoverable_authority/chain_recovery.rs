//! Finalized-chain reconciliation for one-shot recoverable ballots.
//!
//! A recoverable bundle has one authority spend. Reconciliation authenticates
//! its initial VAN and asks whether that VAN's native nullifier is unspent or
//! consumed by one atomic ballot transaction. It never exposes a successor VAN
//! or a remaining proposal-authority mask.

use crate::{
    config::VerifiedRoundAuthV3,
    delegation_capability::ValidatedDelegationCapabilityMaterialV1,
    hotkey::VOTING_HOTKEY_ADDRESS_INDEX,
    types::{validate_vote_chain_id, VotingError},
    zkp2::{
        derive_van_nullifier_from_spending_key, plan_vote_authority_transition_from_spending_key,
    },
};

use super::{
    BundleMaterialSourceV1, RecoverableSelfCustodyBundleV1, RecoverableVotingHotkeyV1,
    VotingAuthorityRootV1, VotingAuthoritySelectionV1,
};

const INITIAL_PROPOSAL_AUTHORITY: u64 = 0xFFFF;

/// Candidate finalized chain point supplied with reconciliation evidence.
///
/// The external evidence verifier must independently authenticate this block
/// hash and vote-commitment-tree root.
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
    /// A singleton cast-vote transaction, which recoverable v1 never creates.
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

/// Untrusted inputs to validate against one finalized checkpoint.
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
/// through that exact checkpoint, or prove that no such consumer exists.
pub trait VotingAuthorityRecoveryEvidenceVerifierV1 {
    fn verify_finalized_complete_evidence(
        &self,
        checkpoint: &RecoveryCheckpointV1,
        initial_consumer_id: &[u8; 32],
        consumer: Option<&ConfirmedVotingAuthorityConsumerV1>,
    ) -> Result<(), VotingError>;
}

/// Why a confirmed authority spend cannot be treated as a recoverable ballot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsupportedRecoverableBallotSpendV1 {
    Singleton,
    NonCanonicalAtomicBatch,
    Other,
}

/// Finalized status of one recoverable bundle's only ballot opportunity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoverableBallotChainStatusV1 {
    /// The initial authority remains available for the bundle's one ballot.
    Unspent,
    /// The initial authority was consumed by one canonical atomic batch.
    TerminalAtomicBatch {
        position: ConfirmedTransitionPositionV1,
        proposal_ids: Vec<u32>,
    },
    /// The initial authority was spent, but not by the permitted transaction.
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

/// Reconciles one self-custody bundle without reconstructing reusable authority.
pub fn reconcile_recoverable_ballot_v1(
    authority_root: &VotingAuthorityRootV1,
    authority_selection: &VotingAuthoritySelectionV1,
    current_round: &VerifiedRoundAuthV3,
    bundle: &RecoverableSelfCustodyBundleV1,
    evidence: &VotingAuthorityRecoveryEvidenceV1,
    evidence_verifier: &dyn VotingAuthorityRecoveryEvidenceVerifierV1,
) -> Result<ReconciledRecoverableBallotV1, VotingError> {
    authority_root.validate_current_selection(authority_selection, current_round)?;
    if authority_selection.bundle_source() != BundleMaterialSourceV1::RecoverableSelfCustody {
        return Err(invalid_recovery(
            "voting authority selection does not permit self-custody reconciliation",
        ));
    }
    let hotkey = authority_root.voting_hotkey()?;
    let van_blinding = bundle.derive_van_blinding(authority_root);
    let total_note_value = bundle.notes().iter().try_fold(0u64, |total, note| {
        total
            .checked_add(note.value)
            .ok_or_else(|| invalid_recovery("recoverable bundle note value overflowed"))
    })?;
    reconcile_recoverable_ballot_from_material_v1(
        &hotkey,
        current_round.context().proposal_count(),
        total_note_value,
        van_blinding.as_bytes(),
        evidence,
        evidence_verifier,
    )
}

/// Reconciles one bundle from an authority-validated custody capability.
pub fn reconcile_delegation_capability_ballot_v1(
    authority_root: &VotingAuthorityRootV1,
    authority_selection: &VotingAuthoritySelectionV1,
    current_round: &VerifiedRoundAuthV3,
    capability: &ValidatedDelegationCapabilityMaterialV1,
    bundle_index: u32,
    evidence: &VotingAuthorityRecoveryEvidenceV1,
    evidence_verifier: &dyn VotingAuthorityRecoveryEvidenceVerifierV1,
) -> Result<ReconciledRecoverableBallotV1, VotingError> {
    authority_root.validate_current_selection(authority_selection, current_round)?;
    if authority_selection.bundle_source() != BundleMaterialSourceV1::CustodyCapability {
        return Err(invalid_recovery(
            "voting authority selection does not permit custody reconciliation",
        ));
    }
    let hotkey = authority_root.voting_hotkey()?;
    if capability.target() != &hotkey.delegation_target()
        || capability.vote_chain_id() != hotkey.context().vote_chain_id()
        || capability.vote_round_id() != hotkey.context().vote_round_id()
    {
        return Err(invalid_recovery(
            "validated custody capability does not match the selected voting authority",
        ));
    }
    let bundle = capability
        .bundles()
        .iter()
        .find(|bundle| bundle.bundle_index() == bundle_index)
        .ok_or_else(|| invalid_recovery("validated custody capability bundle was not found"))?;
    if evidence.initial_witness().van() != bundle.van_commitment() {
        return Err(invalid_recovery(
            "root-validated initial VAN leaf does not match the validated custody capability",
        ));
    }
    reconcile_recoverable_ballot_from_material_v1(
        &hotkey,
        current_round.context().proposal_count(),
        bundle.total_note_value(),
        bundle.van_blinding(),
        evidence,
        evidence_verifier,
    )
}

fn reconcile_recoverable_ballot_from_material_v1(
    hotkey: &RecoverableVotingHotkeyV1,
    proposal_count: u32,
    total_note_value: u64,
    van_blinding: &[u8; 32],
    evidence: &VotingAuthorityRecoveryEvidenceV1,
    evidence_verifier: &dyn VotingAuthorityRecoveryEvidenceVerifierV1,
) -> Result<ReconciledRecoverableBallotV1, VotingError> {
    if evidence.finalized_checkpoint.vote_chain_id() != hotkey.context().vote_chain_id() {
        return Err(invalid_recovery(
            "recovery checkpoint vote chain does not match the voting authority",
        ));
    }

    let spending_key = hotkey.orchard_spending_key();
    let round_id = hotkey.context().vote_round_id();
    let initial_plan = plan_vote_authority_transition_from_spending_key(
        &spending_key,
        VOTING_HOTKEY_ADDRESS_INDEX,
        total_note_value,
        van_blinding,
        round_id,
        1,
        INITIAL_PROPOSAL_AUTHORITY,
    )?;
    let initial_position = validate_initial_van_witness(
        &evidence.initial_witness,
        &evidence.finalized_checkpoint,
        &initial_plan.vote_authority_note_old,
    )?;
    let initial_consumer_id = derive_van_nullifier_from_spending_key(
        &spending_key,
        round_id,
        &initial_plan.vote_authority_note_old,
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
                if canonical_ballot_ids(proposal_ids, proposal_count) =>
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
            RecoverableVanBlindingV1, RegisteredKeyApplicationV1, SoftwareRegisteredKeyRequestV1,
            VotingAuthorityContextV1, VotingAuthorityRootV1,
        },
        types::{Network, NoteInfo},
    };

    fn note(position: u64) -> NoteInfo {
        NoteInfo {
            commitment: vec![position as u8 + 1; 32],
            nullifier: vec![position as u8 + 17; 32],
            value: BALLOT_DIVISOR,
            position,
            diversifier: vec![0; 11],
            rho: vec![0; 32],
            rseed: vec![0; 32],
            scope: 0,
            ufvk_str: String::new(),
        }
    }

    struct AuthorityFixture {
        root: VotingAuthorityRootV1,
        selection: VotingAuthoritySelectionV1,
        round_auth: VerifiedRoundAuthV3,
        hotkey: RecoverableVotingHotkeyV1,
        bundle: RecoverableSelfCustodyBundleV1,
        blinding: RecoverableVanBlindingV1,
    }

    fn authority_fixture() -> AuthorityFixture {
        let context = VotingAuthorityContextV1::from_fingerprint(
            Network::Testnet,
            0,
            [0x22; 32],
            "vote-chain-test",
            [0x01; 32],
        )
        .unwrap();
        let request =
            SoftwareRegisteredKeyRequestV1::new(RegisteredKeyApplicationV1::new(1), context);
        let root = VotingAuthorityRootV1::from_registered_key_output(&request, [0x55; 64]);
        let round_auth = super::super::test_verified_round_auth_v3(root.context());
        let selection = VotingAuthoritySelectionV1::bind(
            &root,
            BundleMaterialSourceV1::RecoverableSelfCustody,
            &round_auth,
        )
        .unwrap();
        let hotkey = root.voting_hotkey().unwrap();
        let bundle =
            RecoverableSelfCustodyBundleV1::from_canonical_bundle(0, vec![note(0), note(1)])
                .unwrap();
        let blinding = bundle.derive_van_blinding(&root);
        AuthorityFixture {
            root,
            selection,
            round_auth,
            hotkey,
            bundle,
            blinding,
        }
    }

    struct AcceptingVerifier;

    impl VotingAuthorityRecoveryEvidenceVerifierV1 for AcceptingVerifier {
        fn verify_finalized_complete_evidence(
            &self,
            checkpoint: &RecoveryCheckpointV1,
            _initial_consumer_id: &[u8; 32],
            _consumer: Option<&ConfirmedVotingAuthorityConsumerV1>,
        ) -> Result<(), VotingError> {
            if checkpoint.block_hash() != &[0x44; 32] {
                return Err(invalid_recovery("fixture checkpoint is not authenticated"));
            }
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
            Err(invalid_recovery("finalized completeness was not proven"))
        }
    }

    fn initial_material(authority: &AuthorityFixture) -> ([u8; 32], [u8; 32]) {
        let total_note_value = authority.bundle.notes().iter().map(|note| note.value).sum();
        let spending_key = authority.hotkey.orchard_spending_key();
        let plan = plan_vote_authority_transition_from_spending_key(
            &spending_key,
            VOTING_HOTKEY_ADDRESS_INDEX,
            total_note_value,
            authority.blinding.as_bytes(),
            authority.hotkey.context().vote_round_id(),
            1,
            INITIAL_PROPOSAL_AUTHORITY,
        )
        .unwrap();
        let consumer = derive_van_nullifier_from_spending_key(
            &spending_key,
            authority.hotkey.context().vote_round_id(),
            &plan.vote_authority_note_old,
        )
        .unwrap();
        (plan.vote_authority_note_old, consumer)
    }

    fn evidence(
        authority: &AuthorityFixture,
        kind: Option<ConfirmedVotingAuthorityConsumerKindV1>,
    ) -> VotingAuthorityRecoveryEvidenceV1 {
        let (initial_van, consumer_id) = initial_material(authority);
        let mut tree = vote_commitment_tree::MemoryTreeServer::empty();
        for position in 0..10u64 {
            tree.append(pallas::Base::from(1_000 + position)).unwrap();
        }
        tree.append(crate::governance::bytes_to_fp(&initial_van).unwrap())
            .unwrap();
        tree.checkpoint(100).unwrap();
        let checkpoint =
            RecoveryCheckpointV1::new("vote-chain-test", 100, [0x44; 32], tree.root().to_repr())
                .unwrap();
        let witness = VotingAuthorityVanWitnessV1::new(
            initial_van,
            tree.path(10, 100).expect("fixture initial VAN path"),
        );
        let consumer = kind.map(|kind| {
            ConfirmedVotingAuthorityConsumerV1::new(
                ConfirmedTransitionPositionV1::new(90, 2),
                consumer_id,
                kind,
            )
        });
        VotingAuthorityRecoveryEvidenceV1::new(checkpoint, Some(witness), consumer).unwrap()
    }

    fn reconcile(
        authority: &AuthorityFixture,
        evidence: &VotingAuthorityRecoveryEvidenceV1,
        verifier: &dyn VotingAuthorityRecoveryEvidenceVerifierV1,
    ) -> Result<ReconciledRecoverableBallotV1, VotingError> {
        reconcile_recoverable_ballot_v1(
            &authority.root,
            &authority.selection,
            &authority.round_auth,
            &authority.bundle,
            evidence,
            verifier,
        )
    }

    #[test]
    fn unspent_reconciliation_returns_only_the_initial_position() {
        let authority = authority_fixture();
        let result =
            reconcile(&authority, &evidence(&authority, None), &AcceptingVerifier).unwrap();
        assert_eq!(result.initial_leaf_position(), 10);
        assert_eq!(result.status(), &RecoverableBallotChainStatusV1::Unspent);
    }

    #[test]
    fn one_action_atomic_batch_is_terminal() {
        let authority = authority_fixture();
        let evidence = evidence(
            &authority,
            Some(ConfirmedVotingAuthorityConsumerKindV1::AtomicBatch {
                proposal_ids: vec![2],
            }),
        );
        assert_eq!(
            reconcile(&authority, &evidence, &AcceptingVerifier)
                .unwrap()
                .status(),
            &RecoverableBallotChainStatusV1::TerminalAtomicBatch {
                position: ConfirmedTransitionPositionV1::new(90, 2),
                proposal_ids: vec![2],
            }
        );
    }

    #[test]
    fn singleton_and_noncanonical_batches_fail_closed() {
        let authority = authority_fixture();
        let singleton = evidence(
            &authority,
            Some(ConfirmedVotingAuthorityConsumerKindV1::Singleton { proposal_id: 1 }),
        );
        assert!(matches!(
            reconcile(&authority, &singleton, &AcceptingVerifier)
                .unwrap()
                .status(),
            RecoverableBallotChainStatusV1::UnsupportedSpent {
                reason: UnsupportedRecoverableBallotSpendV1::Singleton,
                ..
            }
        ));

        for proposal_ids in [vec![], vec![2, 1], vec![1, 1], vec![4]] {
            let malformed = evidence(
                &authority,
                Some(ConfirmedVotingAuthorityConsumerKindV1::AtomicBatch { proposal_ids }),
            );
            assert!(matches!(
                reconcile(&authority, &malformed, &AcceptingVerifier)
                    .unwrap()
                    .status(),
                RecoverableBallotChainStatusV1::UnsupportedSpent {
                    reason: UnsupportedRecoverableBallotSpendV1::NonCanonicalAtomicBatch,
                    ..
                }
            ));
        }
    }

    #[test]
    fn reconciliation_requires_native_consumer_and_external_completeness() {
        let authority = authority_fixture();
        let mut wrong_consumer = evidence(
            &authority,
            Some(ConfirmedVotingAuthorityConsumerKindV1::AtomicBatch {
                proposal_ids: vec![1, 3],
            }),
        );
        wrong_consumer.consumer.as_mut().unwrap().consumer_id[0] ^= 1;
        assert!(reconcile(&authority, &wrong_consumer, &AcceptingVerifier)
            .unwrap_err()
            .to_string()
            .contains("native initial VAN nullifier"));

        let unspent = evidence(&authority, None);
        assert!(reconcile(&authority, &unspent, &RejectingVerifier)
            .unwrap_err()
            .to_string()
            .contains("completeness"));
    }
}
