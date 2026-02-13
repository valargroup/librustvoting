use ff::{Field, PrimeField};
use pasta_curves::pallas;

use crate::governance;
use crate::types::{
    validate_hotkey, validate_notes, validate_round_params, DelegationAction, NoteInfo,
    VotingError, VotingHotkey, VotingRoundParams,
};

/// Construct the delegation action for Keystone signing.
///
/// Computes real governance nullifiers (padded to 4) and VAN.
/// action_bytes/rk/sighash remain stubs (they need rho, which depends on VAN — next step).
///
/// - `nk`: 32-byte nullifier deriving key
/// - `g_d_new_x`: 32-byte x-coordinate of hotkey diversified generator
/// - `pk_d_new_x`: 32-byte x-coordinate of hotkey transmission key
pub fn construct_delegation_action(
    hotkey: &VotingHotkey,
    notes: &[NoteInfo],
    params: &VotingRoundParams,
    nk: &[u8],
    g_d_new_x: &[u8],
    pk_d_new_x: &[u8],
) -> Result<DelegationAction, VotingError> {
    validate_hotkey(hotkey)?;
    validate_notes(notes)?;
    validate_round_params(params)?;
    crate::types::validate_32_bytes(nk, "nk")?;
    crate::types::validate_32_bytes(g_d_new_x, "g_d_new_x")?;
    crate::types::validate_32_bytes(pk_d_new_x, "pk_d_new_x")?;

    // Convert vote_round_id from hex string to exactly 32 bytes.
    // Rejecting non-32-byte values prevents silent truncation/padding collisions.
    let vote_round_id_bytes = hex::decode(&params.vote_round_id).map_err(|e| {
        VotingError::InvalidInput {
            message: format!("vote_round_id is not valid hex: {}", e),
        }
    })?;
    crate::types::validate_32_bytes(&vote_round_id_bytes, "vote_round_id (decoded hex)")?;
    // Safe: validate_32_bytes already ensures exactly 32 bytes.
    let vri_32: [u8; 32] = vote_round_id_bytes.try_into().unwrap();

    // Compute real gov nullifiers for each input note
    let mut gov_nullifiers: Vec<Vec<u8>> = Vec::with_capacity(4);
    for note in notes {
        let gov_null = governance::derive_gov_nullifier(nk, &vri_32, &note.nullifier)?;
        gov_nullifiers.push(gov_null);
    }

    // Pad to 4 with random v=0 dummy notes; each gets a real gov nullifier
    let mut rng = rand::thread_rng();
    let mut dummy_nullifiers: Vec<Vec<u8>> = Vec::new();
    while gov_nullifiers.len() < 4 {
        // Random dummy note nullifier (valid field element for Poseidon)
        let dummy_nf_fp = pallas::Base::random(&mut rng);
        let dummy_nf: [u8; 32] = dummy_nf_fp.to_repr();
        let gov_null = governance::derive_gov_nullifier(nk, &vri_32, &dummy_nf)?;
        gov_nullifiers.push(gov_null);
        dummy_nullifiers.push(dummy_nf.to_vec());
    }

    // Compute total weight from note values (checked to prevent silent overflow)
    let total_weight: u64 = notes
        .iter()
        .try_fold(0u64, |acc, n| acc.checked_add(n.value))
        .ok_or_else(|| VotingError::InvalidInput {
            message: "total note weight overflows u64".to_string(),
        })?;

    // Sample gov_comm_rand as a proper random field element
    let gov_comm_rand_fp = pallas::Base::random(&mut rng);
    let gov_comm_rand: [u8; 32] = gov_comm_rand_fp.to_repr();

    // Compute real VAN
    let van = governance::construct_van(
        g_d_new_x,
        pk_d_new_x,
        total_weight,
        &vri_32,
        &gov_comm_rand,
    )?;

    Ok(DelegationAction {
        // Stubs for now — these need rho (depends on VAN + note commitments, next step)
        action_bytes: vec![0xDA; 128],
        rk: vec![0xDE; 32],
        sighash: vec![0x5A; 32],
        gov_nullifiers,
        van,
        gov_comm_rand: gov_comm_rand.to_vec(),
        dummy_nullifiers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_note() -> NoteInfo {
        NoteInfo {
            commitment: vec![0x01; 32],
            nullifier: vec![0x02; 32],
            value: 1_000_000,
            position: 42,
        }
    }

    fn mock_hotkey() -> VotingHotkey {
        VotingHotkey {
            secret_key: vec![0x42; 32],
            public_key: vec![0x43; 32],
            address: "zvote1test".to_string(),
        }
    }

    fn mock_params() -> VotingRoundParams {
        VotingRoundParams {
            // Hex string representing 32 bytes
            vote_round_id: "0101010101010101010101010101010101010101010101010101010101010101"
                .to_string(),
            snapshot_height: 100_000,
            ea_pk: vec![0xEA; 32],
            nc_root: vec![0xCC; 32],
            nullifier_imt_root: vec![0xDD; 32],
        }
    }

    fn mock_nk() -> Vec<u8> {
        vec![0x11; 32]
    }

    fn mock_g_d() -> Vec<u8> {
        vec![0x22; 32]
    }

    fn mock_pk_d() -> Vec<u8> {
        vec![0x33; 32]
    }

    #[test]
    fn test_construct_delegation_action_one_note() {
        let result = construct_delegation_action(
            &mock_hotkey(),
            &[mock_note()],
            &mock_params(),
            &mock_nk(),
            &mock_g_d(),
            &mock_pk_d(),
        )
        .unwrap();

        assert_eq!(result.rk.len(), 32);
        assert_eq!(result.sighash.len(), 32);
        assert!(!result.action_bytes.is_empty());

        // Gov nullifiers always padded to 4
        assert_eq!(result.gov_nullifiers.len(), 4);
        for gnull in &result.gov_nullifiers {
            assert_eq!(gnull.len(), 32);
        }

        // VAN is 32 bytes
        assert_eq!(result.van.len(), 32);
        assert_ne!(result.van, vec![0xBB; 32]); // not the old mock

        // gov_comm_rand is 32 bytes
        assert_eq!(result.gov_comm_rand.len(), 32);

        // First gov nullifier is real (deterministic for same inputs)
        assert_ne!(result.gov_nullifiers[0], vec![0xAA; 32]); // not the old mock
    }

    #[test]
    fn test_construct_delegation_action_four_notes() {
        let notes: Vec<NoteInfo> = (0..4)
            .map(|i| NoteInfo {
                commitment: vec![i as u8 + 1; 32],
                nullifier: vec![i as u8 + 0x10; 32],
                value: 250_000,
                position: i as u64,
            })
            .collect();

        let result = construct_delegation_action(
            &mock_hotkey(),
            &notes,
            &mock_params(),
            &mock_nk(),
            &mock_g_d(),
            &mock_pk_d(),
        )
        .unwrap();

        assert_eq!(result.gov_nullifiers.len(), 4);
        // All 4 should be real (no padding needed)
        // They should all be different since inputs differ
        for i in 0..4 {
            for j in (i + 1)..4 {
                assert_ne!(
                    result.gov_nullifiers[i], result.gov_nullifiers[j],
                    "gov nullifiers {} and {} should differ",
                    i, j
                );
            }
        }
    }

    #[test]
    fn test_construct_delegation_action_deterministic_gov_nullifiers() {
        let result1 = construct_delegation_action(
            &mock_hotkey(),
            &[mock_note()],
            &mock_params(),
            &mock_nk(),
            &mock_g_d(),
            &mock_pk_d(),
        )
        .unwrap();

        let result2 = construct_delegation_action(
            &mock_hotkey(),
            &[mock_note()],
            &mock_params(),
            &mock_nk(),
            &mock_g_d(),
            &mock_pk_d(),
        )
        .unwrap();

        // First gov nullifier (real) should be deterministic
        assert_eq!(result1.gov_nullifiers[0], result2.gov_nullifiers[0]);

        // VAN will differ because gov_comm_rand is randomly sampled each time
        // (this is expected)
    }

    #[test]
    fn test_construct_delegation_action_no_notes() {
        let result = construct_delegation_action(
            &mock_hotkey(),
            &[],
            &mock_params(),
            &mock_nk(),
            &mock_g_d(),
            &mock_pk_d(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_construct_delegation_action_too_many_notes() {
        let notes: Vec<NoteInfo> = (0..5).map(|_| mock_note()).collect();
        let result = construct_delegation_action(
            &mock_hotkey(),
            &notes,
            &mock_params(),
            &mock_nk(),
            &mock_g_d(),
            &mock_pk_d(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_construct_delegation_action_rejects_short_vote_round_id() {
        let mut params = mock_params();
        // 31 bytes (62 hex chars)
        params.vote_round_id = "01".repeat(31);

        let result = construct_delegation_action(
            &mock_hotkey(),
            &[mock_note()],
            &params,
            &mock_nk(),
            &mock_g_d(),
            &mock_pk_d(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_construct_delegation_action_rejects_long_vote_round_id() {
        let mut params = mock_params();
        // 33 bytes (66 hex chars)
        params.vote_round_id = "01".repeat(33);

        let result = construct_delegation_action(
            &mock_hotkey(),
            &[mock_note()],
            &params,
            &mock_nk(),
            &mock_g_d(),
            &mock_pk_d(),
        );
        assert!(result.is_err());
    }
}
