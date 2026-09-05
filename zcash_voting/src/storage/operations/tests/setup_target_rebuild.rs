//! Delegation setup the wallet's current voting hotkey can no longer
//! reproduce: when setup rebuilds it, and when the rebuild is refused.

use super::fixtures::*;
use super::*;

/// A wallet whose voting hotkey changed can still set up its bundle: the
/// SDK notices the stored target no longer reproduces, and rebuilds it.
///
/// Nothing about that decision reaches the host. It turns entirely on
/// whether the delegation may already be on chain, which is the judgement
/// a host cannot make, so the crate makes it and the wallet just retries.
#[test]
fn setup_rebuilds_a_bundle_the_current_hotkey_cannot_use() {
    let (db, note, fvk_bytes) = ironwood_setup_fixture();

    let first = keys_for_hotkey_byte(&fvk_bytes, 0x43);
    db.build_governance_pczt(ROUND_ID, 0, &[note.clone()], &first, nu6_3_branch_id())
        .unwrap();
    let first_setup = van_comm_rand_of(&db, 0);
    assert!(first_setup.is_some());
    // The setup the builder just wrote must reproduce from the key that
    // wrote it. This is the check every later step relies on, and for a
    // long time it could not be satisfied by any real bundle: the
    // reconstruction used the spend's rho where the output note's rho —
    // the spend's nullifier — belongs, so a correct hotkey was reported as
    // a mismatch and bundles were rebuilt for no reason.
    {
        let identity = DelegationProofIdentity::new(db.sidecar_id(), W.to_string(), ROUND_ID, 0);
        db.validate_delegation_proof_target(&identity, &first)
            .expect("fresh setup must validate against the key that built it");
    }
    queries::store_proof(&db.conn(), ROUND_ID, W, 0, &[0x01; 8]).unwrap();

    // The wallet comes back with a different hotkey for the same round.
    // Without the rebuild this is where the round dies: the write-once
    // setup guard refuses the new bundle and every later step refuses the
    // stored one.
    let second = keys_for_hotkey_byte(&fvk_bytes, 0x44);
    db.build_governance_pczt(ROUND_ID, 0, &[note], &second, nu6_3_branch_id())
        .expect("setup must rebuild a bundle the current hotkey cannot use");

    assert_ne!(
        van_comm_rand_of(&db, 0),
        first_setup,
        "the bundle must be rebuilt, not left bound to the old hotkey"
    );
    let proofs: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM proofs WHERE round_id = ?1 AND wallet_id = ?2",
            rusqlite::params![ROUND_ID, W],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        proofs, 0,
        "the stale proof must go with the setup it was made for"
    );
}

/// The Keystone shape validates too: a bundle set up against a round-bound
/// target reproduces from the hotkey behind that target.
///
/// Keystone reaches this through `keystone_request`, which runs setup
/// before redacting the PCZT for the device. While the reconstruction used
/// the spend's rho, that path saw a mismatch on every bundle it had just
/// written — and a hardware voter's recovery is worse than a software
/// one's, because the rebuild discards a signature the device already made.
#[test]
fn a_round_bound_keystone_target_validates_against_its_own_setup() {
    let (db, note, fvk_bytes) = ironwood_setup_fixture();
    let round_bytes: [u8; 32] = hex::decode(ROUND_ID).unwrap().try_into().unwrap();
    let keys = {
        let voting_hotkey =
            VotingHotkey::from_stored_secret(&[0x51; 64], Network::Regtest).unwrap();
        let target = RoundBoundVotingHotkeyTarget::from_validated_parts(
            voting_hotkey.delegation_target(),
            "vote-chain-1".to_string(),
            round_bytes,
        );
        DelegationKeys::with_round_bound_voting_target(
            fvk_bytes.clone(),
            &target,
            [0x42; 32],
            0,
            "keystone round".to_string(),
        )
        .unwrap()
    };

    db.build_governance_pczt(ROUND_ID, 0, &[note], &keys, nu6_3_branch_id())
        .expect("a round-bound target must set up");

    let identity = DelegationProofIdentity::new(db.sidecar_id(), W.to_string(), ROUND_ID, 0);
    db.validate_delegation_proof_target(&identity, &keys)
        .expect("the bundle must reproduce from the round-bound target that built it");
}

/// The same wallet, once its delegation may be on chain, is refused instead
/// — with every byte of the setup it will need kept.
#[test]
fn setup_refuses_to_rebuild_a_bundle_that_may_be_on_chain() {
    let (db, note, fvk_bytes) = ironwood_setup_fixture();

    let first = keys_for_hotkey_byte(&fvk_bytes, 0x43);
    db.build_governance_pczt(ROUND_ID, 0, &[note.clone()], &first, nu6_3_branch_id())
        .unwrap();
    let committed_setup = van_comm_rand_of(&db, 0);
    queries::store_delegation_tx_hash(&db.conn(), ROUND_ID, W, 0, "ab".repeat(32).as_str())
        .unwrap();

    let second = keys_for_hotkey_byte(&fvk_bytes, 0x44);
    let error = db
        .build_governance_pczt(ROUND_ID, 0, &[note], &second, nu6_3_branch_id())
        .unwrap_err();

    assert!(
        matches!(
            error,
            VotingError::DelegationAlreadyBroadcast {
                bundle_index: 0,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(
        van_comm_rand_of(&db, 0),
        committed_setup,
        "a refused rebuild must keep the setup the round's weight depends on"
    );
}

/// The exclusion the rebuild holds is not the only guard: the write that
/// replaces a binding checks the evidence itself, in the transaction that
/// writes, so a submission that commits while a replacement is being built
/// still keeps its setup.
#[test]
fn replacing_a_stored_binding_is_refused_once_the_bundle_is_broadcast() {
    let db = db_with_delegation_setup(1);
    let committed_setup = van_comm_rand_of(&db, 0);
    insert_chain_submission(&db, 0);

    let error = store_binding(&db, &[0x99; 32]).unwrap_err();

    assert!(
        matches!(error, VotingError::DelegationAlreadyBroadcast { .. }),
        "{error:?}"
    );
    assert_eq!(
        van_comm_rand_of(&db, 0),
        committed_setup,
        "the refused replacement must not have written anything"
    );
}

/// The guard is on replacement, not on writing: rewriting the same binding is
/// how an interrupted setup resumes, and it stays allowed.
#[test]
fn rewriting_the_same_binding_is_allowed_after_broadcast() {
    let db = db_with_delegation_setup(1);
    insert_chain_submission(&db, 0);

    store_binding(&db, &[0x11; 32]).expect("an idempotent rewrite is not a replacement");

    assert_eq!(van_comm_rand_of(&db, 0), Some(vec![0x11; 32]));
}

/// Writes bundle 0's delegation setup with `van_comm_rand`, leaving every
/// other field as `db_with_delegation_setup` left it.
fn store_binding(db: &VotingDb, van_comm_rand: &[u8; 32]) -> Result<(), VotingError> {
    queries::store_delegation_data(
        &db.conn(),
        ROUND_ID,
        W,
        0,
        van_comm_rand,
        &[vec![0x22; 32]],
        &[0x33; 32],
        &[vec![0x44; 32]],
        &[0x55; 32],
        &[0x66; 32],
        &[0x77; 32],
        &[0x88; 32],
        &[0x99; 32],
        &[0xAA; 32],
        1_000,
        0,
        &[(vec![0xBB; 32], vec![0xCC; 32])],
        &[0xDD; 32],
        &crate::tx1::placeholder_tx1_effects(),
    )
}

/// The mismatch kind reports the disagreement and nothing else. A bundle
/// already on chain fails the comparison identically to one that never left
/// the device, so a host must not read the kind as permission to discard —
/// the discard decides that itself, and refuses.
#[test]
fn the_mismatch_kind_says_nothing_about_whether_the_bundle_was_broadcast() {
    let (db, note, fvk_bytes) = ironwood_setup_fixture();
    let first = keys_for_hotkey_byte(&fvk_bytes, 0x43);
    db.build_governance_pczt(ROUND_ID, 0, &[note], &first, nu6_3_branch_id())
        .unwrap();
    insert_chain_submission(&db, 0);

    let identity = DelegationProofIdentity::new(db.sidecar_id(), W.to_string(), ROUND_ID, 0);
    let second = keys_for_hotkey_byte(&fvk_bytes, 0x44);
    let error = db
        .validate_delegation_proof_target(&identity, &second)
        .unwrap_err();

    assert_eq!(
        error.kind(),
        crate::VotingErrorKind::DelegationTargetMismatch
    );
    // And the recovery that kind might suggest is refused, which is where the
    // unbroadcast guarantee actually lives.
    let refusal = db
        .discard_unbroadcast_delegation(ROUND_ID, Some(0))
        .unwrap_err();
    assert!(
        matches!(refusal, VotingError::DelegationAlreadyBroadcast { .. }),
        "{refusal:?}"
    );
}
