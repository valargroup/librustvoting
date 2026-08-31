//! Pure recovery of a recoverable bundle's confirmed voting-authority chain.
//!
//! This module does not fetch chain data or write wallet state. It verifies VCT
//! paths, native consumer nullifiers, ordering, and every authority transition
//! locally. An explicit external verifier remains responsible for independently
//! authenticating the exact finalized checkpoint and proving the transition
//! stream complete through that checkpoint.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    config::VerifiedRoundAuthV3,
    delegation_capability::ValidatedDelegationCapabilityMaterialV1,
    hotkey::VOTING_HOTKEY_ADDRESS_INDEX,
    types::{validate_vote_chain_id, VotingError},
    vote::MAX_VOTE_BATCH_ACTIONS,
    zkp2::{
        derive_van_nullifier_from_spending_key, plan_vote_authority_transition_from_spending_key,
    },
};

use super::{
    BundleMaterialSourceV1, RecoverableSelfCustodyBundleV1, RecoverableVotingHotkeyV1,
    VotingAuthorityRootV1, VotingAuthoritySelectionV1,
};

const INITIAL_PROPOSAL_AUTHORITY: u64 = 0xFFFF;

/// Candidate finalized chain point supplied with recovery evidence.
///
/// Recovery requires an external evidence verifier to authenticate this block
/// hash and VCT root and to prove completeness through the checkpoint.
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

/// One VAN plus its authentication path at the recovery checkpoint.
///
/// Construction only parses the path. Recovery independently recomputes the
/// checkpoint root before treating the leaf as validated.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RootValidatedVanLeafV1 {
    van: [u8; 32],
    position: u64,
}

/// Stable position used to order confirmed singleton transactions and atomic
/// batches within the recovery evidence.
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

/// One confirmed vote action supplied as untrusted chain evidence.
///
/// `sequence` is zero-based across singleton and atomic actions. `consumer_id`
/// must be the chain-visible native nullifier of the consumed VAN; recovery
/// recomputes and compares it before accepting the action. A singleton carries
/// its successor witness. In an atomic batch only the final action carries the
/// batch's one final successor witness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmedVotingAuthorityActionV1 {
    sequence: u8,
    consumer_id: [u8; 32],
    proposal_id: u32,
    successor_witness: Option<VotingAuthorityVanWitnessV1>,
}

impl ConfirmedVotingAuthorityActionV1 {
    pub fn new(
        sequence: u8,
        consumer_id: [u8; 32],
        proposal_id: u32,
        successor_witness: Option<VotingAuthorityVanWitnessV1>,
    ) -> Self {
        Self {
            sequence,
            consumer_id,
            proposal_id,
            successor_witness,
        }
    }

    pub fn sequence(&self) -> u8 {
        self.sequence
    }

    pub fn consumer_id(&self) -> &[u8; 32] {
        &self.consumer_id
    }

    pub fn proposal_id(&self) -> u32 {
        self.proposal_id
    }

    pub fn successor_witness(&self) -> Option<&VotingAuthorityVanWitnessV1> {
        self.successor_witness.as_ref()
    }
}

/// One confirmed singleton transaction or atomic batch, in chain order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmedVotingAuthorityTransitionV1 {
    Singleton {
        position: ConfirmedTransitionPositionV1,
        action: ConfirmedVotingAuthorityActionV1,
    },
    Atomic {
        position: ConfirmedTransitionPositionV1,
        actions: Vec<ConfirmedVotingAuthorityActionV1>,
    },
}

impl ConfirmedVotingAuthorityTransitionV1 {
    pub fn singleton(
        position: ConfirmedTransitionPositionV1,
        action: ConfirmedVotingAuthorityActionV1,
    ) -> Self {
        Self::Singleton { position, action }
    }

    pub fn atomic(
        position: ConfirmedTransitionPositionV1,
        actions: Vec<ConfirmedVotingAuthorityActionV1>,
    ) -> Self {
        Self::Atomic { position, actions }
    }

    fn position(&self) -> ConfirmedTransitionPositionV1 {
        match self {
            Self::Singleton { position, .. } | Self::Atomic { position, .. } => *position,
        }
    }

    fn actions(&self) -> &[ConfirmedVotingAuthorityActionV1] {
        match self {
            Self::Singleton { action, .. } => std::slice::from_ref(action),
            Self::Atomic { actions, .. } => actions,
        }
    }
}

/// Untrusted recovery inputs to validate against one finalized checkpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VotingAuthorityRecoveryEvidenceV1 {
    finalized_checkpoint: RecoveryCheckpointV1,
    initial_witness: VotingAuthorityVanWitnessV1,
    transitions: Vec<ConfirmedVotingAuthorityTransitionV1>,
}

impl VotingAuthorityRecoveryEvidenceV1 {
    pub fn new(
        finalized_checkpoint: RecoveryCheckpointV1,
        initial_witness: Option<VotingAuthorityVanWitnessV1>,
        transitions: Vec<ConfirmedVotingAuthorityTransitionV1>,
    ) -> Result<Self, VotingError> {
        let initial_witness =
            initial_witness.ok_or_else(|| invalid_recovery("initial VAN witness is missing"))?;
        Ok(Self {
            finalized_checkpoint,
            initial_witness,
            transitions,
        })
    }

    pub fn finalized_checkpoint(&self) -> &RecoveryCheckpointV1 {
        &self.finalized_checkpoint
    }

    pub fn initial_witness(&self) -> &VotingAuthorityVanWitnessV1 {
        &self.initial_witness
    }

    pub fn transitions(&self) -> &[ConfirmedVotingAuthorityTransitionV1] {
        &self.transitions
    }
}

/// External trust boundary for finalized and complete vote-chain evidence.
///
/// The implementation must independently authenticate `checkpoint` (including
/// its block hash and VCT root), establish that `transitions` contains every
/// confirmed action consuming the reconstructed authority through that exact
/// checkpoint, and establish that `terminal_consumer_id` has no confirmed
/// consumer through the checkpoint. Merely accepting these arguments is not a
/// valid implementation. Local recovery separately verifies every VCT path,
/// native VAN, native consumer nullifier, transition, and authority mask.
pub trait VotingAuthorityRecoveryEvidenceVerifierV1 {
    fn verify_finalized_complete_evidence(
        &self,
        checkpoint: &RecoveryCheckpointV1,
        transitions: &[ConfirmedVotingAuthorityTransitionV1],
        terminal_consumer_id: &[u8; 32],
    ) -> Result<(), VotingError>;
}

/// Latest authority state recovered from complete finalized evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredVotingAuthorityStateV1 {
    latest_leaf: RootValidatedVanLeafV1,
    remaining_proposal_authority: u64,
    finalized_checkpoint: RecoveryCheckpointV1,
}

impl RecoveredVotingAuthorityStateV1 {
    pub fn latest_van(&self) -> &[u8; 32] {
        &self.latest_leaf.van
    }

    pub fn latest_leaf_position(&self) -> u64 {
        self.latest_leaf.position
    }

    pub fn remaining_proposal_authority(&self) -> u64 {
        self.remaining_proposal_authority
    }

    pub fn finalized_checkpoint(&self) -> &RecoveryCheckpointV1 {
        &self.finalized_checkpoint
    }
}

/// Recomputes the unique recoverable VAN chain through a finalized checkpoint.
///
/// This function performs no I/O. It rejects incomplete or differently rooted
/// evidence, noncontiguous action sequences, unordered/ambiguous transaction
/// positions, duplicate or conflicting consumers, repeated proposals, absent
/// successors, and any successor that differs from the native vote transition.
/// Acceptance additionally requires `evidence_verifier` to authenticate the
/// checkpoint and prove that the transition stream is complete through it.
pub fn recover_voting_authority_chain_v1(
    authority_root: &VotingAuthorityRootV1,
    authority_selection: &VotingAuthoritySelectionV1,
    current_round: &VerifiedRoundAuthV3,
    bundle: &RecoverableSelfCustodyBundleV1,
    evidence: &VotingAuthorityRecoveryEvidenceV1,
    evidence_verifier: &dyn VotingAuthorityRecoveryEvidenceVerifierV1,
) -> Result<RecoveredVotingAuthorityStateV1, VotingError> {
    authority_root.validate_current_selection(authority_selection, current_round)?;
    if authority_selection.bundle_source() != BundleMaterialSourceV1::RecoverableSelfCustody {
        return Err(invalid_recovery(
            "voting authority selection does not permit self-custody recovery",
        ));
    }
    let hotkey = authority_root.voting_hotkey()?;
    let van_blinding = bundle.derive_van_blinding(authority_root);
    let total_note_value = bundle.notes().iter().try_fold(0u64, |total, note| {
        total
            .checked_add(note.value)
            .ok_or_else(|| invalid_recovery("recoverable bundle note value overflowed"))
    })?;
    recover_voting_authority_chain_from_material_v1(
        &hotkey,
        total_note_value,
        van_blinding.as_bytes(),
        evidence,
        evidence_verifier,
    )
}

/// Recomputes one custody capability bundle's recoverable VAN chain.
///
/// `bundle` must come from
/// [`crate::delegation_capability::validate_recoverable_delegation_capability_v1`],
/// which binds its target, chain, round, and source to the recoverable voting
/// authority. This function performs no I/O and shares the same traversal and
/// evidence checks as [`recover_voting_authority_chain_v1`].
pub fn recover_delegation_capability_voting_authority_chain_v1(
    authority_root: &VotingAuthorityRootV1,
    authority_selection: &VotingAuthoritySelectionV1,
    current_round: &VerifiedRoundAuthV3,
    capability: &ValidatedDelegationCapabilityMaterialV1,
    bundle_index: u32,
    evidence: &VotingAuthorityRecoveryEvidenceV1,
    evidence_verifier: &dyn VotingAuthorityRecoveryEvidenceVerifierV1,
) -> Result<RecoveredVotingAuthorityStateV1, VotingError> {
    authority_root.validate_current_selection(authority_selection, current_round)?;
    if authority_selection.bundle_source() != BundleMaterialSourceV1::CustodyCapability {
        return Err(invalid_recovery(
            "voting authority selection does not permit custody recovery",
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
    recover_voting_authority_chain_from_material_v1(
        &hotkey,
        bundle.total_note_value(),
        bundle.van_blinding(),
        evidence,
        evidence_verifier,
    )
}

pub(crate) fn recover_voting_authority_chain_from_material_v1(
    hotkey: &RecoverableVotingHotkeyV1,
    total_note_value: u64,
    van_blinding: &[u8; 32],
    evidence: &VotingAuthorityRecoveryEvidenceV1,
    evidence_verifier: &dyn VotingAuthorityRecoveryEvidenceVerifierV1,
) -> Result<RecoveredVotingAuthorityStateV1, VotingError> {
    if evidence.finalized_checkpoint.vote_chain_id() != hotkey.context().vote_chain_id() {
        return Err(invalid_recovery(
            "recovery checkpoint vote chain does not match the voting authority",
        ));
    }
    let action_count = evidence
        .transitions
        .iter()
        .try_fold(0usize, |count, transition| {
            count
                .checked_add(transition.actions().len())
                .ok_or_else(|| invalid_recovery("recovery action count overflowed"))
        })?;
    if action_count > MAX_VOTE_BATCH_ACTIONS {
        return Err(invalid_recovery(format!(
            "authority recovery supports at most {MAX_VOTE_BATCH_ACTIONS} actions, got {action_count}"
        )));
    }

    let spending_key = hotkey.orchard_spending_key();
    let round_id = hotkey.context().vote_round_id();

    // Proposal 1 is available in the initial 16-bit authority mask. Its plan
    // exposes the initial VAN without mutating recovered state.
    let initial_plan = plan_vote_authority_transition_from_spending_key(
        &spending_key,
        VOTING_HOTKEY_ADDRESS_INDEX,
        total_note_value,
        van_blinding,
        round_id,
        1,
        INITIAL_PROPOSAL_AUTHORITY,
    )?;
    let initial_leaf =
        validate_van_witness(&evidence.initial_witness, &evidence.finalized_checkpoint)?;
    if initial_plan.vote_authority_note_old != initial_leaf.van {
        return Err(invalid_recovery(
            "root-validated initial VAN leaf does not match the recoverable authority and bundle",
        ));
    }

    let mut current_van = initial_leaf.van;
    let mut latest_root_validated_leaf = initial_leaf;
    let mut remaining_authority = INITIAL_PROPOSAL_AUTHORITY;
    let mut expected_sequence = 0u8;
    let mut last_position = None;
    let mut consumers = BTreeMap::new();
    let mut proposals = BTreeSet::new();
    let mut leaf_positions = BTreeSet::from([initial_leaf.position]);

    for transition in &evidence.transitions {
        let position = transition.position();
        if position.block_height > evidence.finalized_checkpoint.height {
            return Err(invalid_recovery(
                "confirmed transition lies after the finalized checkpoint",
            ));
        }
        if let Some(previous) = last_position {
            if position == previous {
                return Err(invalid_recovery(
                    "ambiguous transition groups share one chain position",
                ));
            }
            if position < previous {
                return Err(invalid_recovery(
                    "confirmed transition groups are not in chain order",
                ));
            }
        }
        last_position = Some(position);

        if let ConfirmedVotingAuthorityTransitionV1::Atomic { actions, .. } = transition {
            if actions.len() < 2 {
                return Err(invalid_recovery(
                    "atomic transition must contain at least two ordered actions",
                ));
            }
        }

        let actions = transition.actions();
        let is_atomic = matches!(
            transition,
            ConfirmedVotingAuthorityTransitionV1::Atomic { .. }
        );
        for (action_index, action) in actions.iter().enumerate() {
            let successor_identity = action
                .successor_witness
                .as_ref()
                .map(|witness| (*witness.van(), witness.position()));
            if let Some(previous) = consumers.get(&action.consumer_id) {
                if previous == &(action.proposal_id, successor_identity) {
                    return Err(invalid_recovery("duplicate confirmed authority consumer"));
                }
                return Err(invalid_recovery(
                    "conflicting confirmed actions share one authority consumer",
                ));
            }
            consumers.insert(action.consumer_id, (action.proposal_id, successor_identity));

            let expected_consumer =
                derive_van_nullifier_from_spending_key(&spending_key, round_id, &current_van)?;
            if action.consumer_id != expected_consumer {
                return Err(invalid_recovery(
                    "confirmed authority consumer does not match the native VAN nullifier",
                ));
            }

            if action.sequence > expected_sequence {
                return Err(invalid_recovery(
                    "confirmed authority action sequence contains a gap",
                ));
            }
            if action.sequence < expected_sequence {
                return Err(invalid_recovery(
                    "confirmed authority action sequence is ambiguous or out of order",
                ));
            }
            expected_sequence = expected_sequence
                .checked_add(1)
                .ok_or_else(|| invalid_recovery("recovery action sequence overflowed"))?;

            if !proposals.insert(action.proposal_id) {
                return Err(invalid_recovery(
                    "proposal authority is consumed more than once",
                ));
            }
            let is_final_atomic_action = is_atomic && action_index + 1 == actions.len();
            if is_atomic && !is_final_atomic_action && action.successor_witness.is_some() {
                return Err(invalid_recovery(
                    "intermediate atomic action must not carry a root-validated successor leaf",
                ));
            }
            let plan = plan_vote_authority_transition_from_spending_key(
                &spending_key,
                VOTING_HOTKEY_ADDRESS_INDEX,
                total_note_value,
                van_blinding,
                round_id,
                action.proposal_id,
                remaining_authority,
            )?;
            if plan.vote_authority_note_old != current_van {
                return Err(invalid_recovery(
                    "confirmed authority action does not consume the current VAN",
                ));
            }

            if !is_atomic || is_final_atomic_action {
                let successor_witness = action.successor_witness.as_ref().ok_or_else(|| {
                    invalid_recovery(
                        "confirmed authority transition is missing its root-validated successor leaf",
                    )
                })?;
                let successor_leaf =
                    validate_van_witness(successor_witness, &evidence.finalized_checkpoint)?;
                if successor_leaf.position <= latest_root_validated_leaf.position
                    || !leaf_positions.insert(successor_leaf.position)
                {
                    return Err(invalid_recovery(
                        "root-validated successor leaf positions are ambiguous or out of order",
                    ));
                }
                if plan.vote_authority_note_new != successor_leaf.van {
                    return Err(invalid_recovery(
                        "confirmed successor VAN does not match the native authority transition",
                    ));
                }
                latest_root_validated_leaf = successor_leaf;
            }
            current_van = plan.vote_authority_note_new;
            remaining_authority = plan.proposal_authority_new;
        }
    }

    let terminal_consumer_id =
        derive_van_nullifier_from_spending_key(&spending_key, round_id, &current_van)?;
    evidence_verifier.verify_finalized_complete_evidence(
        &evidence.finalized_checkpoint,
        &evidence.transitions,
        &terminal_consumer_id,
    )?;

    Ok(RecoveredVotingAuthorityStateV1 {
        latest_leaf: latest_root_validated_leaf,
        remaining_proposal_authority: remaining_authority,
        finalized_checkpoint: evidence.finalized_checkpoint.clone(),
    })
}

fn validate_van_witness(
    witness: &VotingAuthorityVanWitnessV1,
    checkpoint: &RecoveryCheckpointV1,
) -> Result<RootValidatedVanLeafV1, VotingError> {
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
    Ok(RootValidatedVanLeafV1 {
        van: *witness.van(),
        position,
    })
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
        delegation_capability::{
            validate_recoverable_delegation_capability_v1, DelegationCapabilityBundleV1,
            DelegationCapabilityV1, ValidateRecoverableDelegationCapabilityV1Params,
        },
        governance::BALLOT_DIVISOR,
        recoverable_authority::{
            RecoverableVanBlindingV1, RegisteredKeyApplicationV1, SoftwareRegisteredKeyRequestV1,
            VotingAuthorityContextV1, VotingAuthorityRootV1,
        },
        types::{Network, NoteInfo, VotingRoundParams},
    };
    use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};

    fn note(value: u64, position: u64) -> NoteInfo {
        NoteInfo {
            commitment: vec![position as u8 + 1; 32],
            nullifier: vec![position as u8 + 17; 32],
            value,
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
        round_auth: crate::config::VerifiedRoundAuthV3,
        hotkey: RecoverableVotingHotkeyV1,
        bundle: RecoverableSelfCustodyBundleV1,
        blinding: RecoverableVanBlindingV1,
    }

    fn authority_fixture_for_source(bundle_source: BundleMaterialSourceV1) -> AuthorityFixture {
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
        let selection =
            VotingAuthoritySelectionV1::bind(&root, bundle_source, &round_auth).unwrap();
        let hotkey = root.voting_hotkey().unwrap();
        let bundle = RecoverableSelfCustodyBundleV1::from_canonical_bundle(
            0,
            vec![note(BALLOT_DIVISOR, 0), note(BALLOT_DIVISOR, 1)],
        )
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

    fn authority_fixture() -> AuthorityFixture {
        authority_fixture_for_source(BundleMaterialSourceV1::RecoverableSelfCustody)
    }

    fn recover_fixture(
        authority: &AuthorityFixture,
        evidence: &VotingAuthorityRecoveryEvidenceV1,
    ) -> Result<RecoveredVotingAuthorityStateV1, VotingError> {
        recover_voting_authority_chain_v1(
            &authority.root,
            &authority.selection,
            &authority.round_auth,
            &authority.bundle,
            evidence,
            &FixtureEvidenceVerifier,
        )
    }

    struct FixtureEvidenceVerifier;

    impl VotingAuthorityRecoveryEvidenceVerifierV1 for FixtureEvidenceVerifier {
        fn verify_finalized_complete_evidence(
            &self,
            checkpoint: &RecoveryCheckpointV1,
            _transitions: &[ConfirmedVotingAuthorityTransitionV1],
            _terminal_consumer_id: &[u8; 32],
        ) -> Result<(), VotingError> {
            if checkpoint.block_hash() != &[0x44; 32] {
                return Err(invalid_recovery(
                    "fixture checkpoint is not independently authenticated",
                ));
            }
            Ok(())
        }
    }

    struct RejectingEvidenceVerifier;

    impl VotingAuthorityRecoveryEvidenceVerifierV1 for RejectingEvidenceVerifier {
        fn verify_finalized_complete_evidence(
            &self,
            _checkpoint: &RecoveryCheckpointV1,
            _transitions: &[ConfirmedVotingAuthorityTransitionV1],
            _terminal_consumer_id: &[u8; 32],
        ) -> Result<(), VotingError> {
            Err(invalid_recovery(
                "external evidence source did not prove finalized completeness",
            ))
        }
    }

    struct NativeChain {
        initial: [u8; 32],
        successors: Vec<[u8; 32]>,
        consumers: Vec<[u8; 32]>,
    }

    fn native_chain(
        hotkey: &RecoverableVotingHotkeyV1,
        bundle: &RecoverableSelfCustodyBundleV1,
        blinding: &RecoverableVanBlindingV1,
        proposals: &[u32],
    ) -> NativeChain {
        let total_note_value = bundle.notes().iter().map(|note| note.value).sum();
        let spending_key = hotkey.orchard_spending_key();
        let mut authority = INITIAL_PROPOSAL_AUTHORITY;
        let mut initial = None;
        let mut successors = Vec::new();
        let mut consumers = Vec::new();
        for proposal_id in proposals {
            let transition = plan_vote_authority_transition_from_spending_key(
                &spending_key,
                VOTING_HOTKEY_ADDRESS_INDEX,
                total_note_value,
                blinding.as_bytes(),
                hotkey.context().vote_round_id(),
                *proposal_id,
                authority,
            )
            .unwrap();
            initial.get_or_insert(transition.vote_authority_note_old);
            consumers.push(
                derive_van_nullifier_from_spending_key(
                    &spending_key,
                    hotkey.context().vote_round_id(),
                    &transition.vote_authority_note_old,
                )
                .unwrap(),
            );
            successors.push(transition.vote_authority_note_new);
            authority = transition.proposal_authority_new;
        }
        NativeChain {
            initial: initial.unwrap(),
            successors,
            consumers,
        }
    }

    fn checkpoint(vote_commitment_tree_root: [u8; 32]) -> RecoveryCheckpointV1 {
        RecoveryCheckpointV1::new(
            "vote-chain-test",
            100,
            [0x44; 32],
            vote_commitment_tree_root,
        )
        .unwrap()
    }

    fn evidence(
        initial: [u8; 32],
        mut transitions: Vec<ConfirmedVotingAuthorityTransitionV1>,
    ) -> VotingAuthorityRecoveryEvidenceV1 {
        let mut leaves = BTreeMap::from([(10_u32, initial)]);
        for transition in &transitions {
            for action in transition.actions() {
                if let Some(witness) = action.successor_witness() {
                    let position = u32::try_from(witness.position()).unwrap();
                    leaves.entry(position).or_insert(*witness.van());
                }
            }
        }
        let max_position = *leaves.keys().max().unwrap();
        let mut tree = vote_commitment_tree::MemoryTreeServer::empty();
        for position in 0..=max_position {
            let leaf = leaves
                .get(&position)
                .copied()
                .unwrap_or_else(|| pallas::Base::from(1_000_000 + u64::from(position)).to_repr());
            tree.append(crate::governance::bytes_to_fp(&leaf).unwrap())
                .unwrap();
        }
        tree.checkpoint(100).unwrap();
        let initial_witness = VotingAuthorityVanWitnessV1::new(
            initial,
            tree.path(10, 100).expect("fixture initial VAN path"),
        );
        for transition in &mut transitions {
            let actions = match transition {
                ConfirmedVotingAuthorityTransitionV1::Singleton { action, .. } => {
                    std::slice::from_mut(action)
                }
                ConfirmedVotingAuthorityTransitionV1::Atomic { actions, .. } => actions,
            };
            for action in actions {
                if let Some(witness) = action.successor_witness.take() {
                    let position = u32::try_from(witness.position()).unwrap();
                    action.successor_witness = Some(VotingAuthorityVanWitnessV1::new(
                        *witness.van(),
                        tree.path(u64::from(position), 100)
                            .expect("fixture successor VAN path"),
                    ));
                }
            }
        }
        VotingAuthorityRecoveryEvidenceV1::new(
            checkpoint(tree.root().to_repr()),
            Some(initial_witness),
            transitions,
        )
        .unwrap()
    }

    fn action(
        sequence: u8,
        consumer_id: [u8; 32],
        proposal_id: u32,
        successor: Option<([u8; 32], u64)>,
    ) -> ConfirmedVotingAuthorityActionV1 {
        ConfirmedVotingAuthorityActionV1::new(
            sequence,
            consumer_id,
            proposal_id,
            successor.map(|(van, position)| {
                let empty = vote_commitment_tree::MerkleHashVote::from_fp(pallas::Base::zero());
                VotingAuthorityVanWitnessV1::new(
                    van,
                    vote_commitment_tree::MerklePath::from_parts(
                        u32::try_from(position).unwrap(),
                        [empty; vote_commitment_tree::TREE_DEPTH],
                    ),
                )
            }),
        )
    }

    #[test]
    fn recovers_ordered_singleton_and_atomic_transitions() {
        let authority = authority_fixture();
        let chain = native_chain(
            &authority.hotkey,
            &authority.bundle,
            &authority.blinding,
            &[1, 3, 2],
        );
        let evidence = evidence(
            chain.initial,
            vec![
                ConfirmedVotingAuthorityTransitionV1::singleton(
                    ConfirmedTransitionPositionV1::new(90, 0),
                    action(0, chain.consumers[0], 1, Some((chain.successors[0], 11))),
                ),
                ConfirmedVotingAuthorityTransitionV1::atomic(
                    ConfirmedTransitionPositionV1::new(91, 2),
                    vec![
                        action(1, chain.consumers[1], 3, None),
                        action(2, chain.consumers[2], 2, Some((chain.successors[2], 13))),
                    ],
                ),
            ],
        );

        let recovered = recover_fixture(&authority, &evidence).unwrap();

        assert_eq!(recovered.latest_van(), &chain.successors[2]);
        assert_eq!(recovered.latest_leaf_position(), 13);
        assert_eq!(
            recovered.remaining_proposal_authority(),
            INITIAL_PROPOSAL_AUTHORITY & !(1 << 1) & !(1 << 3) & !(1 << 2)
        );
        assert_eq!(
            recovered.finalized_checkpoint(),
            evidence.finalized_checkpoint()
        );
    }

    #[test]
    fn no_vote_recovery_returns_root_validated_initial_leaf() {
        let authority = authority_fixture();
        let chain = native_chain(
            &authority.hotkey,
            &authority.bundle,
            &authority.blinding,
            &[1],
        );
        let evidence = evidence(chain.initial, vec![]);

        let recovered = recover_fixture(&authority, &evidence).unwrap();

        assert_eq!(recovered.latest_van(), &chain.initial);
        assert_eq!(recovered.latest_leaf_position(), 10);
        assert_eq!(
            recovered.remaining_proposal_authority(),
            INITIAL_PROPOSAL_AUTHORITY
        );
    }

    #[test]
    fn no_vote_recovery_requires_external_finalized_completeness() {
        let authority = authority_fixture();
        let chain = native_chain(
            &authority.hotkey,
            &authority.bundle,
            &authority.blinding,
            &[1],
        );
        let evidence = evidence(chain.initial, vec![]);

        let error = recover_voting_authority_chain_v1(
            &authority.root,
            &authority.selection,
            &authority.round_auth,
            &authority.bundle,
            &evidence,
            &RejectingEvidenceVerifier,
        )
        .unwrap_err();

        assert!(error.to_string().contains("finalized completeness"));
    }

    #[test]
    fn recovery_rejects_a_non_native_consumer_identity() {
        let authority = authority_fixture();
        let chain = native_chain(
            &authority.hotkey,
            &authority.bundle,
            &authority.blinding,
            &[1],
        );
        let evidence = evidence(
            chain.initial,
            vec![ConfirmedVotingAuthorityTransitionV1::singleton(
                ConfirmedTransitionPositionV1::new(90, 0),
                action(0, [0; 32], 1, Some((chain.successors[0], 11))),
            )],
        );

        let error = recover_fixture(&authority, &evidence).unwrap_err();
        assert!(error.to_string().contains("native VAN nullifier"));
    }

    #[test]
    fn recovery_rejects_a_stale_authenticated_round_digest() {
        let authority = authority_fixture();
        let chain = native_chain(
            &authority.hotkey,
            &authority.bundle,
            &authority.blinding,
            &[1],
        );
        let evidence = evidence(chain.initial, vec![]);

        let stale_context = VotingAuthorityContextV1::from_fingerprint(
            Network::Testnet,
            0,
            [0x22; 32],
            "vote-chain-test",
            [0x02; 32],
        )
        .unwrap();
        let stale_round = super::super::test_verified_round_auth_v3(&stale_context);
        let error = recover_voting_authority_chain_v1(
            &authority.root,
            &authority.selection,
            &stale_round,
            &authority.bundle,
            &evidence,
            &FixtureEvidenceVerifier,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("current authenticated round payload"),
            "{error}"
        );
    }

    #[test]
    fn custody_recovery_uses_validated_capability_material() {
        let authority = authority_fixture_for_source(BundleMaterialSourceV1::CustodyCapability);
        let round_params = VotingRoundParams {
            vote_round_id: hex::encode(authority.root.context().vote_round_id()),
            snapshot_height: 1_234_567,
            ea_pk: vec![0xEA; 32],
            nc_root: vec![0x03; 32],
            nullifier_imt_root: vec![0x04; 32],
        };
        let capability = DelegationCapabilityV1 {
            format_version: 1,
            vote_chain_id: authority.root.context().vote_chain_id().to_string(),
            network: "testnet".to_string(),
            vote_round_id: round_params.vote_round_id.clone(),
            address_index: 0,
            raw_orchard_address: BASE64_STANDARD.encode(authority.hotkey.raw_orchard_address()),
            bundles: vec![DelegationCapabilityBundleV1 {
                bundle_index: 0,
                num_ballots: 2,
                van_comm_rand: BASE64_STANDARD.encode({
                    let mut canonical_field = [0; 32];
                    canonical_field[0] = 7;
                    canonical_field
                }),
                delegation_tx_hash: hex::encode([0x66; 32]),
            }],
        };
        let capability_json = capability.to_json().unwrap();
        let verified_round = crate::config::test_verified_voting_round_v3(
            &authority.round_auth,
            round_params.clone(),
        );
        let validated = validate_recoverable_delegation_capability_v1(
            &capability_json,
            ValidateRecoverableDelegationCapabilityV1Params {
                authority_root: &authority.root,
                authority_selection: &authority.selection,
                voting_hotkey: &authority.hotkey,
                verified_round: &verified_round,
            },
        )
        .unwrap();
        let initial = *validated.bundles()[0].van_commitment();
        let evidence = evidence(initial, vec![]);

        let recovered = recover_delegation_capability_voting_authority_chain_v1(
            &authority.root,
            &authority.selection,
            &authority.round_auth,
            &validated,
            0,
            &evidence,
            &FixtureEvidenceVerifier,
        )
        .unwrap();

        assert_eq!(recovered.latest_van(), &initial);
        assert_eq!(recovered.latest_leaf_position(), 10);
    }

    #[test]
    fn evidence_rejects_missing_initial_leaf() {
        let error =
            VotingAuthorityRecoveryEvidenceV1::new(checkpoint([0; 32]), None, vec![]).unwrap_err();
        assert!(error.to_string().contains("initial VAN witness is missing"));
    }

    #[test]
    fn recovery_rejects_vote_chain_mismatch() {
        let authority = authority_fixture();
        let chain = native_chain(
            &authority.hotkey,
            &authority.bundle,
            &authority.blinding,
            &[1],
        );
        let mut evidence = evidence(chain.initial, vec![]);
        evidence.finalized_checkpoint.vote_chain_id = "other-chain".to_string();

        let error = recover_fixture(&authority, &evidence).unwrap_err();
        assert!(error.to_string().contains("vote chain does not match"));
    }

    #[test]
    fn recovery_rejects_gaps_missing_or_wrong_successors_and_bad_initial_van() {
        let authority = authority_fixture();
        let chain = native_chain(
            &authority.hotkey,
            &authority.bundle,
            &authority.blinding,
            &[1, 2],
        );
        for (candidate, expected) in [
            (
                action(1, chain.consumers[0], 1, Some((chain.successors[0], 11))),
                "sequence contains a gap",
            ),
            (
                action(0, chain.consumers[0], 1, None),
                "missing its root-validated successor",
            ),
            (
                action(
                    0,
                    chain.consumers[0],
                    1,
                    Some((pallas::Base::from(999u64).to_repr(), 11)),
                ),
                "does not match",
            ),
        ] {
            let evidence = evidence(
                chain.initial,
                vec![ConfirmedVotingAuthorityTransitionV1::singleton(
                    ConfirmedTransitionPositionV1::new(90, 0),
                    candidate,
                )],
            );
            let error = recover_fixture(&authority, &evidence).unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }

        let missing_atomic_final = evidence(
            chain.initial,
            vec![ConfirmedVotingAuthorityTransitionV1::atomic(
                ConfirmedTransitionPositionV1::new(90, 0),
                vec![
                    action(0, chain.consumers[0], 1, None),
                    action(1, chain.consumers[1], 2, None),
                ],
            )],
        );
        let error = recover_fixture(&authority, &missing_atomic_final).unwrap_err();
        assert!(error.to_string().contains("root-validated successor"));

        let unexpected_atomic_intermediate = evidence(
            chain.initial,
            vec![ConfirmedVotingAuthorityTransitionV1::atomic(
                ConfirmedTransitionPositionV1::new(90, 0),
                vec![
                    action(0, chain.consumers[0], 1, Some((chain.successors[0], 11))),
                    action(1, chain.consumers[1], 2, Some((chain.successors[1], 12))),
                ],
            )],
        );
        let error = recover_fixture(&authority, &unexpected_atomic_intermediate).unwrap_err();
        assert!(error.to_string().contains("intermediate atomic action"));

        let empty = evidence(pallas::Base::from(999u64).to_repr(), vec![]);
        let error = recover_fixture(&authority, &empty).unwrap_err();
        assert!(error.to_string().contains("initial VAN"));
    }

    #[test]
    fn recovery_rejects_duplicate_conflicting_and_ambiguous_consumers() {
        let authority = authority_fixture();
        let chain = native_chain(
            &authority.hotkey,
            &authority.bundle,
            &authority.blinding,
            &[1, 2],
        );

        let duplicate = evidence(
            chain.initial,
            vec![ConfirmedVotingAuthorityTransitionV1::atomic(
                ConfirmedTransitionPositionV1::new(90, 0),
                vec![
                    action(0, chain.consumers[0], 1, None),
                    action(1, chain.consumers[0], 1, None),
                    action(2, chain.consumers[1], 2, Some((chain.successors[1], 12))),
                ],
            )],
        );
        let error = recover_fixture(&authority, &duplicate).unwrap_err();
        assert!(error.to_string().contains("duplicate confirmed"));

        let conflicting = evidence(
            chain.initial,
            vec![ConfirmedVotingAuthorityTransitionV1::atomic(
                ConfirmedTransitionPositionV1::new(90, 0),
                vec![
                    action(0, chain.consumers[0], 1, None),
                    action(1, chain.consumers[0], 2, Some((chain.successors[1], 12))),
                ],
            )],
        );
        let error = recover_fixture(&authority, &conflicting).unwrap_err();
        assert!(error.to_string().contains("conflicting confirmed"));

        let ambiguous = evidence(
            chain.initial,
            vec![
                ConfirmedVotingAuthorityTransitionV1::singleton(
                    ConfirmedTransitionPositionV1::new(90, 0),
                    action(0, chain.consumers[0], 1, Some((chain.successors[0], 11))),
                ),
                ConfirmedVotingAuthorityTransitionV1::singleton(
                    ConfirmedTransitionPositionV1::new(90, 0),
                    action(1, chain.consumers[1], 2, Some((chain.successors[1], 12))),
                ),
            ],
        );
        let error = recover_fixture(&authority, &ambiguous).unwrap_err();
        assert!(error.to_string().contains("ambiguous transition groups"));
    }

    #[test]
    fn recovery_rejects_order_checkpoint_and_action_limit_violations() {
        let authority = authority_fixture();
        let chain = native_chain(
            &authority.hotkey,
            &authority.bundle,
            &authority.blinding,
            &[1, 2],
        );
        let out_of_order = evidence(
            chain.initial,
            vec![
                ConfirmedVotingAuthorityTransitionV1::singleton(
                    ConfirmedTransitionPositionV1::new(91, 0),
                    action(0, chain.consumers[0], 1, Some((chain.successors[0], 11))),
                ),
                ConfirmedVotingAuthorityTransitionV1::singleton(
                    ConfirmedTransitionPositionV1::new(90, 0),
                    action(1, chain.consumers[1], 2, Some((chain.successors[1], 12))),
                ),
            ],
        );
        let error = recover_fixture(&authority, &out_of_order).unwrap_err();
        assert!(error.to_string().contains("not in chain order"));

        let after_checkpoint = evidence(
            chain.initial,
            vec![ConfirmedVotingAuthorityTransitionV1::singleton(
                ConfirmedTransitionPositionV1::new(101, 0),
                action(0, chain.consumers[0], 1, Some((chain.successors[0], 11))),
            )],
        );
        let error = recover_fixture(&authority, &after_checkpoint).unwrap_err();
        assert!(error.to_string().contains("after the finalized checkpoint"));

        let stale_leaf_position = evidence(
            chain.initial,
            vec![ConfirmedVotingAuthorityTransitionV1::singleton(
                ConfirmedTransitionPositionV1::new(90, 0),
                action(0, chain.consumers[0], 1, Some((chain.successors[0], 10))),
            )],
        );
        let error = recover_fixture(&authority, &stale_leaf_position).unwrap_err();
        assert!(error.to_string().contains("does not authenticate"));

        let too_many = evidence(
            chain.initial,
            vec![ConfirmedVotingAuthorityTransitionV1::atomic(
                ConfirmedTransitionPositionV1::new(90, 0),
                (0..=MAX_VOTE_BATCH_ACTIONS)
                    .map(|index| {
                        action(
                            index as u8,
                            [index as u8; 32],
                            1,
                            Some(([0; 32], 11 + index as u64)),
                        )
                    })
                    .collect(),
            )],
        );
        let error = recover_fixture(&authority, &too_many).unwrap_err();
        assert!(error.to_string().contains("at most 15 actions"));
    }
}
