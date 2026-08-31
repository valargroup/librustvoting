#[allow(unused_imports)]
pub(crate) use crate::backend::{halo2_gadgets, orchard, pasta_curves};
use halo2_gadgets::poseidon::primitives::{self as poseidon, ConstantLength, P128Pow5T3};
use orchard::keys::FullViewingKey;
use pasta_curves::group::{
    ff::{Field, PrimeField},
    Curve, Group, GroupEncoding,
};
use pasta_curves::pallas;

use voting_circuits::vote_proof::{
    build_vote_proof_from_delegation, derive_vote_authority_transition,
};
use voting_circuits::VOTE_COMM_TREE_DEPTH;

use crate::hotkey::VOTING_HOTKEY_ACCOUNT_INDEX;
use crate::types::{
    ct_option_to_result, validate_vote_decision, EncryptedShare, Network, ProgressReporter,
    VoteCommitmentBundle, VotingError, MAX_PROPOSAL_ID, MIN_PROPOSAL_ID,
};

// Vote proof build runs circuit synthesis + MockProver + proof generation, which can
// overflow the default simulator thread stack. Run it on a dedicated large-stack thread.
const VOTE_PROOF_STACK_BYTES: usize = 64 * 1024 * 1024;

/// Public VAN values and authority masks for one preplanned vote action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VoteAuthorityTransitionPlan {
    pub vote_authority_note_old: [u8; 32],
    pub vote_authority_note_new: [u8; 32],
    pub proposal_authority_old: u64,
    pub proposal_authority_new: u64,
}

/// Derives one authority transition with the same native implementation used
/// by the proof builder, without constructing a proof.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_vote_authority_transition(
    hotkey_seed: &[u8],
    network: Network,
    address_index: u32,
    total_note_value: u64,
    gov_comm_rand: &[u8],
    voting_round_id: &[u8],
    proposal_id: u32,
    proposal_authority_old: u64,
) -> Result<VoteAuthorityTransitionPlan, VotingError> {
    let sk = crate::hotkey::spending_key_from_hotkey_seed(
        hotkey_seed,
        network,
        VOTING_HOTKEY_ACCOUNT_INDEX,
    )?;
    plan_vote_authority_transition_from_spending_key(
        &sk,
        address_index,
        total_note_value,
        gov_comm_rand,
        voting_round_id,
        proposal_id,
        proposal_authority_old,
    )
}

/// Derives one authority transition from an already-derived Orchard voting key.
///
/// Recoverable authority calls this path so its direct Orchard key is never
/// reinterpreted as a legacy UnifiedSpendingKey seed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_vote_authority_transition_from_spending_key(
    sk: &orchard::keys::SpendingKey,
    address_index: u32,
    total_note_value: u64,
    gov_comm_rand: &[u8],
    voting_round_id: &[u8],
    proposal_id: u32,
    proposal_authority_old: u64,
) -> Result<VoteAuthorityTransitionPlan, VotingError> {
    let gov_comm_rand = parse_base("gov_comm_rand", gov_comm_rand)?;
    let voting_round_id = parse_base("voting_round_id", voting_round_id)?;
    let transition = derive_vote_authority_transition(
        sk,
        address_index,
        total_note_value,
        gov_comm_rand,
        voting_round_id,
        proposal_id as u64,
        proposal_authority_old,
    )
    .map_err(|e| VotingError::InvalidInput {
        message: format!("cannot plan vote authority transition: {e}"),
    })?;

    Ok(VoteAuthorityTransitionPlan {
        vote_authority_note_old: transition.vote_authority_note_old.to_repr(),
        vote_authority_note_new: transition.vote_authority_note_new.to_repr(),
        proposal_authority_old: transition.proposal_authority_old,
        proposal_authority_new: transition.proposal_authority_new,
    })
}

/// Derives the chain-visible nullifier for one native VAN spend.
///
/// `voting-circuits` currently keeps this out-of-circuit helper private. Keep
/// this formula and its frozen vector in lockstep with vote-proof condition 5.
pub(crate) fn derive_van_nullifier_from_spending_key(
    sk: &orchard::keys::SpendingKey,
    voting_round_id: &[u8],
    vote_authority_note_old: &[u8],
) -> Result<[u8; 32], VotingError> {
    let voting_round_id = parse_base("voting_round_id", voting_round_id)?;
    let vote_authority_note_old = parse_base("vote_authority_note_old", vote_authority_note_old)?;
    let fvk = FullViewingKey::from(sk);
    Ok(van_nullifier_hash(fvk.nk().inner(), voting_round_id, vote_authority_note_old).to_repr())
}

fn van_nullifier_hash(
    nullifier_deriving_key: pallas::Base,
    voting_round_id: pallas::Base,
    vote_authority_note_old: pallas::Base,
) -> pallas::Base {
    let mut domain_bytes = [0u8; 32];
    domain_bytes[..b"vote authority spend".len()].copy_from_slice(b"vote authority spend");
    let domain = pallas::Base::from_repr(domain_bytes)
        .expect("the frozen short ASCII VAN-nullifier domain is canonical");
    poseidon::Hash::<_, P128Pow5T3, ConstantLength<4>, 3, 2>::init().hash([
        nullifier_deriving_key,
        domain,
        voting_round_id,
        vote_authority_note_old,
    ])
}

fn parse_base(name: &str, bytes: &[u8]) -> Result<pallas::Base, VotingError> {
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| VotingError::InvalidInput {
        message: format!("{name} must be 32 bytes, got {}", bytes.len()),
    })?;
    ct_option_to_result(
        pallas::Base::from_repr(bytes),
        &format!("{name} is not a canonical Pallas field element"),
    )
}

/// Build vote commitment + ZKP #2.
///
/// Generates a real Halo2 vote proof by calling `build_vote_proof_from_delegation`.
/// The builder handles share decomposition and El Gamal encryption internally,
/// ensuring the ciphertexts in the proof match those returned in `enc_shares`.
///
/// Share encryption randomness and blind factors are derived deterministically
/// from the spending key, round context, and VAN commitment, so the same call
/// with the same inputs always produces the same encrypted shares and commitments.
/// The VAN commitment binding prevents El Gamal nonce reuse when a user has
/// multiple VANs from separate delegation bundles.
///
/// # Arguments
///
/// * `hotkey_seed` - Seed bytes for the hotkey SpendingKey.
/// * `network` - Zcash network used to derive the hotkey spending key.
/// * `address_index` - Diversifier index used for the hotkey address during delegation.
/// * `total_note_value` - Sum of delegated note values.
/// * `gov_comm_rand` - 32-byte VAN blinding factor (from DB).
/// * `voting_round_id` - 32-byte voting round identifier (from DB, hex-decoded).
/// * `ea_pk` - 32-byte compressed election authority public key.
/// * `proposal_id` - Which proposal to vote on (1-15, 1-indexed to match on-chain
///   proposal IDs; bit 0 is the circuit's sentinel value and is always rejected).
/// * `choice` - Vote decision index (0-indexed into the proposal's options).
/// * `num_options` - Number of options declared for this proposal (2-8).
/// * `van_auth_path` - 24 siblings for the VAN Merkle path in the vote commitment tree.
/// * `van_position` - Leaf position of the VAN in the tree.
/// * `anchor_height` - Block height at which the tree was snapshotted.
/// * `progress` - Callback for proof generation progress.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_vote_commitment(
    hotkey_seed: &[u8],
    network: Network,
    address_index: u32,
    total_note_value: u64,
    gov_comm_rand: &[u8],
    voting_round_id: &[u8],
    ea_pk: &[u8],
    proposal_id: u32,
    choice: u32,
    num_options: u32,
    van_auth_path: &[[u8; 32]],
    van_position: u32,
    anchor_height: u32,
    proposal_authority: u64,
    single_share: bool,
    progress: &dyn ProgressReporter,
) -> Result<VoteCommitmentBundle, VotingError> {
    let sk = crate::hotkey::spending_key_from_hotkey_seed(
        hotkey_seed,
        network,
        VOTING_HOTKEY_ACCOUNT_INDEX,
    )?;
    build_vote_commitment_from_spending_key(
        &sk,
        address_index,
        total_note_value,
        gov_comm_rand,
        voting_round_id,
        ea_pk,
        proposal_id,
        choice,
        num_options,
        van_auth_path,
        van_position,
        anchor_height,
        proposal_authority,
        single_share,
        progress,
    )
}

/// Builds ZKP #2 from an already-derived Orchard voting key.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_vote_commitment_from_spending_key(
    sk: &orchard::keys::SpendingKey,
    address_index: u32,
    total_note_value: u64,
    gov_comm_rand: &[u8],
    voting_round_id: &[u8],
    ea_pk: &[u8],
    proposal_id: u32,
    choice: u32,
    num_options: u32,
    van_auth_path: &[[u8; 32]],
    van_position: u32,
    anchor_height: u32,
    proposal_authority: u64,
    single_share: bool,
    progress: &dyn ProgressReporter,
) -> Result<VoteCommitmentBundle, VotingError> {
    validate_vote_decision(choice, num_options)?;
    if !(MIN_PROPOSAL_ID..=MAX_PROPOSAL_ID).contains(&proposal_id) {
        return Err(VotingError::InvalidInput {
            message: format!(
                "proposal_id must be 1..15 (1-indexed, matching on-chain IDs; 0 is the circuit sentinel), got {}",
                proposal_id
            ),
        });
    }
    if van_auth_path.len() != VOTE_COMM_TREE_DEPTH {
        return Err(VotingError::InvalidInput {
            message: format!(
                "van_auth_path must have {} siblings, got {}",
                VOTE_COMM_TREE_DEPTH,
                van_auth_path.len()
            ),
        });
    }

    progress.on_progress(0.05);

    // Parse gov_comm_rand → pallas::Base
    let gcr = parse_base("gov_comm_rand", gov_comm_rand)?;

    // Parse voting_round_id → pallas::Base (canonical Fp).
    let vri = parse_base("voting_round_id", voting_round_id)?;

    // Parse ea_pk → pallas::Affine (compressed point)
    let ea_pk_bytes: [u8; 32] = ea_pk.try_into().map_err(|_| VotingError::InvalidInput {
        message: format!("ea_pk must be 32 bytes, got {}", ea_pk.len()),
    })?;
    let ea_pk_point: pallas::Point = Option::from(pallas::Point::from_bytes(&ea_pk_bytes))
        .ok_or_else(|| VotingError::InvalidInput {
            message: "ea_pk is not a valid compressed Pallas point".to_string(),
        })?;
    // Reject the identity point: with ea_pk = O the El Gamal ciphertexts become
    // C2 = v*G, exposing every share's plaintext. Pallas has cofactor 1, so the
    // identity is the only degenerate public key.
    if bool::from(ea_pk_point.is_identity()) {
        return Err(VotingError::InvalidInput {
            message: "ea_pk must not be the identity point".to_string(),
        });
    }
    let ea_pk_affine = ea_pk_point.to_affine();

    // Convert auth path from byte slices to pallas::Base field elements
    let mut auth_path = [pallas::Base::zero(); VOTE_COMM_TREE_DEPTH];
    for (i, sibling) in van_auth_path.iter().enumerate() {
        auth_path[i] = ct_option_to_result(
            pallas::Base::from_repr(*sibling),
            &format!("van_auth_path[{}] is not a valid Pallas field element", i),
        )?;
    }

    // Generate the real proof
    progress.on_progress(0.10);
    // Generate spend-auth randomizer for the voting key.
    // The caller will need alpha_v to sign the TX2 sighash with rsk_v = ask_v.randomize(&alpha_v).
    let alpha_v = pallas::Scalar::random(&mut voting_crypto_deps::rand::rngs::OsRng);
    let sk_for_proof = *sk;
    let vote_bundle = std::thread::Builder::new()
        .name("vote-proof-build".to_string())
        .stack_size(VOTE_PROOF_STACK_BYTES)
        .spawn(move || {
            build_vote_proof_from_delegation(
                &sk_for_proof,
                address_index,
                total_note_value,
                gcr,
                vri,
                auth_path,
                van_position,
                anchor_height,
                proposal_id as u64,
                choice as u64,
                ea_pk_affine,
                alpha_v,
                proposal_authority,
                single_share,
            )
        })
        .map_err(|e| VotingError::Internal {
            message: format!("failed to spawn vote proof builder thread: {e}"),
        })?
        .join()
        .map_err(|_| VotingError::Internal {
            message: "vote proof builder thread panicked".to_string(),
        })?
        .map_err(|e| VotingError::ProofFailed {
            message: format!("vote proof generation failed: {}", e),
        })?;
    progress.on_progress(1.0);

    // Convert Instance public inputs to byte vectors
    let van_nullifier = vote_bundle.instance.van_nullifier.to_repr().to_vec();
    let van_new = vote_bundle
        .instance
        .vote_authority_note_new
        .to_repr()
        .to_vec();
    let vote_commitment = vote_bundle.instance.vote_commitment.to_repr().to_vec();

    // Convert encrypted shares from builder output to zcash_voting EncryptedShare format
    let enc_shares: Vec<EncryptedShare> = vote_bundle
        .encrypted_shares
        .iter()
        .map(|es| EncryptedShare {
            c1: es.c1.to_vec(),
            c2: es.c2.to_vec(),
            share_index: es.share_index,
            plaintext_value: es.plaintext_value,
            randomness: es.randomness.to_vec(),
        })
        .collect();

    Ok(VoteCommitmentBundle {
        van_nullifier,
        vote_authority_note_new: van_new,
        vote_commitment,
        proposal_id,
        proof: vote_bundle.proof,
        enc_shares,
        anchor_height,
        vote_round_id: hex::encode(voting_round_id),
        shares_hash: vote_bundle.shares_hash.to_repr().to_vec(),
        share_blinds: vote_bundle
            .share_blinds
            .iter()
            .map(|b| b.to_repr().to_vec())
            .collect(),
        share_comms: vote_bundle
            .share_comms
            .iter()
            .map(|c| c.to_repr().to_vec())
            .collect(),
        r_vpk_bytes: vote_bundle.r_vpk_bytes.to_vec(),
        alpha_v: alpha_v.to_repr().to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestReporter;

    impl ProgressReporter for TestReporter {
        fn on_progress(&self, _progress: f64) {}
    }

    #[test]
    fn van_nullifier_formula_matches_the_voting_circuit_vector() {
        assert_eq!(
            van_nullifier_hash(
                pallas::Base::from(1u64),
                pallas::Base::from(42u64),
                pallas::Base::from(100u64),
            )
            .to_repr(),
            [
                114, 56, 62, 208, 155, 244, 76, 209, 125, 210, 149, 109, 176, 88, 34, 116, 123, 56,
                62, 216, 108, 204, 55, 120, 28, 155, 217, 186, 29, 159, 128, 2,
            ]
        );
    }

    #[test]
    fn hotkey_spending_key_helper_uses_zip32_account_index() {
        let seed = [0x42; 64];

        let default = crate::hotkey::spending_key_from_hotkey_seed(
            &seed,
            Network::Mainnet,
            VOTING_HOTKEY_ACCOUNT_INDEX,
        )
        .unwrap();
        let account_0 =
            crate::hotkey::spending_key_from_hotkey_seed(&seed, Network::Mainnet, 0).unwrap();
        let account_1 =
            crate::hotkey::spending_key_from_hotkey_seed(&seed, Network::Mainnet, 1).unwrap();

        assert_eq!(
            orchard::keys::FullViewingKey::from(&default).to_bytes(),
            orchard::keys::FullViewingKey::from(&account_0).to_bytes()
        );
        assert_ne!(
            orchard::keys::FullViewingKey::from(&account_0).to_bytes(),
            orchard::keys::FullViewingKey::from(&account_1).to_bytes()
        );
    }

    #[test]
    fn legacy_transition_wrapper_matches_direct_spending_key_path() {
        let seed = [0x42; 64];
        let network = Network::Mainnet;
        let spending_key = crate::hotkey::spending_key_from_hotkey_seed(
            &seed,
            network,
            VOTING_HOTKEY_ACCOUNT_INDEX,
        )
        .unwrap();
        let legacy = plan_vote_authority_transition(
            &seed, network, 0, 12_500_000, &[0; 32], &[1; 32], 1, 0xFFFF,
        )
        .unwrap();
        let direct = plan_vote_authority_transition_from_spending_key(
            &spending_key,
            0,
            12_500_000,
            &[0; 32],
            &[1; 32],
            1,
            0xFFFF,
        )
        .unwrap();

        assert_eq!(legacy, direct);
    }

    #[test]
    fn test_build_vote_commitment_bad_choice() {
        assert!(build_vote_commitment(
            &[0x42; 64],
            Network::Mainnet,
            0,
            1_000_000,
            &[0u8; 32],
            &[0u8; 32],
            &[0u8; 32],
            1,
            3, // invalid choice (num_options=2)
            2,
            &[[0u8; 32]; 24],
            0,
            1,
            65535,
            false,
            &TestReporter,
        )
        .is_err());
    }

    #[test]
    fn test_build_vote_commitment_invalid_option_count() {
        assert!(build_vote_commitment(
            &[0x42; 64],
            Network::Mainnet,
            0,
            1_000_000,
            &[0u8; 32],
            &[0u8; 32],
            &[0u8; 32],
            1,
            0,
            1, // fewer than two proposal options
            &[[0u8; 32]; 24],
            0,
            1,
            65535,
            false,
            &TestReporter,
        )
        .is_err());

        assert!(build_vote_commitment(
            &[0x42; 64],
            Network::Mainnet,
            0,
            1_000_000,
            &[0u8; 32],
            &[0u8; 32],
            &[0u8; 32],
            1,
            0,
            9, // more than eight proposal options
            &[[0u8; 32]; 24],
            0,
            1,
            65535,
            false,
            &TestReporter,
        )
        .is_err());
    }

    #[test]
    fn test_build_vote_commitment_proposal_id_zero_rejected() {
        assert!(build_vote_commitment(
            &[0x42; 64],
            Network::Mainnet,
            0,
            1_000_000,
            &[0u8; 32],
            &[0u8; 32],
            &[0u8; 32],
            0, // sentinel value; circuit rejects via non-zero gate
            0,
            2,
            &[[0u8; 32]; 24],
            0,
            1,
            65535,
            false,
            &TestReporter,
        )
        .is_err());
    }

    #[test]
    fn test_build_vote_commitment_proposal_id_too_large() {
        assert!(build_vote_commitment(
            &[0x42; 64],
            Network::Mainnet,
            0,
            1_000_000,
            &[0u8; 32],
            &[0u8; 32],
            &[0u8; 32],
            16, // exceeds MAX_PROPOSAL_ID-1
            0,
            2,
            &[[0u8; 32]; 24],
            0,
            1,
            65535,
            false,
            &TestReporter,
        )
        .is_err());
    }

    #[test]
    fn test_build_vote_commitment_wrong_auth_path_len() {
        assert!(build_vote_commitment(
            &[0x42; 64],
            Network::Mainnet,
            0,
            1_000_000,
            &[0u8; 32],
            &[0u8; 32],
            &[0u8; 32],
            1,
            0,
            2,
            &[[0u8; 32]; 10], // wrong length
            0,
            1,
            65535,
            false,
            &TestReporter,
        )
        .is_err());
    }
}
