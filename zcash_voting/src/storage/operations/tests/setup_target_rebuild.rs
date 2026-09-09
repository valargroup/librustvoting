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

/// Every setup call joins the same exclusion as a rebuild, including the
/// first write. Otherwise a caller can observe the binding after the rebuild
/// cleared it and write a competing setup while the rebuilder still holds the
/// round gate.
#[test]
fn first_setup_write_refuses_the_cleared_binding_window() {
    let (db, note, fvk_bytes) = ironwood_setup_fixture();
    let keys = keys_for_hotkey_byte(&fvk_bytes, 0x43);
    let _rebuild_lease = db
        .acquire_round_exclusive_lease(ROUND_ID, W)
        .unwrap()
        .expect("a current round has a submission identity");

    let error = db
        .build_governance_pczt(ROUND_ID, 0, &[note], &keys, nu6_3_branch_id())
        .unwrap_err();

    assert!(matches!(error, VotingError::Busy { .. }), "{error:?}");
    assert_eq!(
        van_comm_rand_of(&db, 0),
        None,
        "a setup writer must not enter while the rebuild gate is held"
    );
}

/// Reusing an existing setup is serialized too. If only mismatch-triggered
/// rebuilds took the gate, a same-hotkey caller could begin from a generation
/// that another caller was about to discard.
#[test]
fn existing_setup_reuse_refuses_an_active_rebuild() {
    let (db, note, fvk_bytes) = ironwood_setup_fixture();
    let keys = keys_for_hotkey_byte(&fvk_bytes, 0x43);
    db.build_governance_pczt(ROUND_ID, 0, &[note.clone()], &keys, nu6_3_branch_id())
        .unwrap();
    let stored_setup = van_comm_rand_of(&db, 0);
    let _rebuild_lease = db
        .acquire_round_exclusive_lease(ROUND_ID, W)
        .unwrap()
        .expect("a current round has a submission identity");

    let error = db
        .build_governance_pczt(ROUND_ID, 0, &[note], &keys, nu6_3_branch_id())
        .unwrap_err();

    assert!(matches!(error, VotingError::Busy { .. }), "{error:?}");
    assert_eq!(
        van_comm_rand_of(&db, 0),
        stored_setup,
        "a refused reuse must leave the durable setup unchanged"
    );
}

/// Setup exclusion follows the bundle scope used by delegation work. A
/// five-note bundle being prepared must not make a concurrent one-note tail
/// fail merely because both belong to the same round.
#[test]
fn another_bundle_builds_while_delegation_setup_is_active() {
    let (db, notes, fvk_bytes) = ironwood_setup_fixture_with_note_count(6);
    let keys = keys_for_hotkey_byte(&fvk_bytes, 0x43);
    let _first_bundle_setup = db
        .acquire_delegation_setup_lease(ROUND_ID, W, 0)
        .unwrap()
        .expect("a current round has a submission identity");

    db.build_governance_pczt(ROUND_ID, 1, &notes[5..], &keys, nu6_3_branch_id())
        .expect("an unrelated bundle must remain independent");

    assert!(
        van_comm_rand_of(&db, 1).is_some(),
        "the tail bundle setup must be persisted"
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
    store_setup(db, van_comm_rand, &[0xAA; 32])
}

/// The same write with the governance commitment moved too, which is the shape
/// of a caller that keeps the stored blinding factor and changes what it
/// commits to.
fn store_binding_with_gov_comm(
    db: &VotingDb,
    van_comm_rand: &[u8; 32],
    gov_comm: &[u8; 32],
) -> Result<(), VotingError> {
    store_setup(db, van_comm_rand, gov_comm)
}

fn store_setup(
    db: &VotingDb,
    van_comm_rand: &[u8; 32],
    gov_comm: &[u8; 32],
) -> Result<(), VotingError> {
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
        gov_comm,
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

/// The rebuild clears the binding before it spends time building a
/// replacement, so an absent binding is exactly the window in which another
/// process may have dispatched the old payload. The write refuses there too,
/// rather than reading `NULL` as proof that this is a first write.
#[test]
fn a_write_over_a_cleared_binding_is_refused_once_the_bundle_is_broadcast() {
    let db = db_with_delegation_setup(1);
    db.discard_unbroadcast_delegation(ROUND_ID, Some(0))
        .unwrap();
    assert_eq!(van_comm_rand_of(&db, 0), None, "the discard clears it");
    // A bundle can sit with no binding after an explicit host-driven discard,
    // and a later direct write must still not walk over broadcast evidence.
    insert_chain_submission(&db, 0);

    let error = store_binding(&db, &[0x99; 32]).unwrap_err();

    assert!(
        matches!(error, VotingError::DelegationAlreadyBroadcast { .. }),
        "{error:?}"
    );
    assert_eq!(van_comm_rand_of(&db, 0), None);
}

/// The rebuild's exchange is one transaction, so evidence that arrives while
/// the replacement is being built refuses the whole thing and the setup it
/// would have discarded is still there. When the discard committed on its own,
/// a refusal at the write left the bundle with neither.
///
/// Exercised at the query, because the race it closes is between processes:
/// the round gate is process-local and cannot exclude the other party.
#[test]
fn a_rebuild_refused_at_the_write_keeps_the_old_setup_and_its_proof() {
    let db = db_with_delegation_setup(1);
    queries::store_proof(&db.conn(), ROUND_ID, W, 0, &[0x01; 8]).unwrap();
    // What another process dispatches while the replacement is being built.
    insert_chain_submission(&db, 0);

    let error = replace_setup(&db, &[0x99; 32]).unwrap_err();

    assert!(
        matches!(error, VotingError::DelegationAlreadyBroadcast { .. }),
        "{error:?}"
    );
    assert_eq!(
        van_comm_rand_of(&db, 0),
        Some(vec![0x11; 32]),
        "the setup that recovers the round must survive a refused rebuild"
    );
    let proofs: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM proofs
              WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = 0",
            rusqlite::params![ROUND_ID, W],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(proofs, 1, "nor may its proof be discarded");
}

/// The same exchange with nothing dispatched into it: the replacement lands
/// and takes the old setup's derived rows with it.
#[test]
fn a_rebuild_that_is_not_refused_replaces_the_setup_and_its_proof() {
    let db = db_with_delegation_setup(1);
    queries::store_proof(&db.conn(), ROUND_ID, W, 0, &[0x01; 8]).unwrap();

    replace_setup(&db, &[0x99; 32]).expect("an unbroadcast bundle rebuilds");

    assert_eq!(van_comm_rand_of(&db, 0), Some(vec![0x99; 32]));
    let proofs: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM proofs
              WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = 0",
            rusqlite::params![ROUND_ID, W],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        proofs, 0,
        "the stale proof goes with the setup it was made for"
    );
}

/// A recorded rejection streak for bundle 0, if any.
fn rejection_streak(db: &VotingDb) -> Option<u32> {
    db.conn()
        .query_row(
            "SELECT consecutive_rejections FROM combined_cast_rejections
              WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = 0",
            rusqlite::params![ROUND_ID, W],
            |row| row.get::<_, i64>(0).map(|streak| streak as u32),
        )
        .ok()
}

/// Records one combined rejection against bundle 0's delegation generation.
fn record_rejection_streak(db: &VotingDb, streak: u32) {
    db.conn()
        .execute(
            "INSERT INTO combined_cast_rejections
                (round_id, wallet_id, bundle_index, delegation_generation_digest,
                 last_batch_digest, consecutive_rejections, last_diagnostic_kind,
                 last_diagnostic, first_rejected_at, last_rejected_at)
             VALUES (?1, ?2, 0, ?3, ?4, ?5, 'chain_rejected', 'code 7', 100, 100)",
            rusqlite::params![ROUND_ID, W, [0x01u8; 32], [0x02u8; 32], streak as i64],
        )
        .unwrap();
}

#[test]
fn a_rebuild_clears_the_rejection_streak_the_old_delegation_earned() {
    // The streak is counted against a delegation generation. A rebuild ends
    // that generation, so the count no longer describes anything that can be
    // sent and must not block the replacement's first cast. The discard's own
    // `DELETE` cannot do this: retiring the rejected batch keeps the bundle's
    // `votes` rows, which the shared broadcast guard requires to be absent.
    let db = db_with_delegation_setup(1);
    record_rejection_streak(&db, 2);

    replace_setup(&db, &[0x99; 32]).expect("an unbroadcast bundle rebuilds");

    assert_eq!(
        rejection_streak(&db),
        None,
        "the streak goes with the delegation it was counted against"
    );
}

#[test]
fn an_idempotent_setup_rewrite_keeps_the_rejection_streak() {
    // Resuming an interrupted setup rewrites the same columns with the same
    // values. That is the very generation the chain refused, so the streak
    // still describes what would be sent and must survive.
    let db = db_with_delegation_setup(1);
    let stored = van_comm_rand_of(&db, 0).expect("the fixture bound the setup");
    record_rejection_streak(&db, 2);

    store_binding(&db, &stored.clone().try_into().unwrap())
        .expect("rewriting the same binding is allowed");

    assert_eq!(
        rejection_streak(&db),
        Some(2),
        "an unchanged setup is the same delegation generation"
    );
}

/// Exchanges bundle 0's setup for one bound to `van_comm_rand`.
fn replace_setup(db: &VotingDb, van_comm_rand: &[u8; 32]) -> Result<(), VotingError> {
    queries::replace_unbroadcast_delegation_setup(
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
        &[],
        &[0xEE; 32],
        &[vec![0xFF; 32]],
    )
}

/// The same window with nothing dispatched into it: the rebuild completes,
/// because the guard is on the evidence and not on the absent binding.
#[test]
fn a_write_over_a_cleared_binding_succeeds_when_nothing_was_broadcast() {
    let db = db_with_delegation_setup(1);
    db.discard_unbroadcast_delegation(ROUND_ID, Some(0))
        .unwrap();

    store_binding(&db, &[0x99; 32]).expect("an unbroadcast bundle rebuilds");

    assert_eq!(van_comm_rand_of(&db, 0), Some(vec![0x99; 32]));
}

/// The stored blinding factor is not a licence to rewrite what it commits to.
/// Every column the write can change is compared, so a caller that keeps
/// `van_comm_rand` and moves the governance commitment is still refused.
#[test]
fn keeping_the_binding_while_changing_derived_setup_is_still_a_replacement() {
    let db = db_with_delegation_setup(1);
    insert_chain_submission(&db, 0);

    let error = store_binding_with_gov_comm(&db, &[0x11; 32], &[0xEE; 32]).unwrap_err();

    assert!(
        matches!(error, VotingError::DelegationAlreadyBroadcast { .. }),
        "{error:?}"
    );
    let gov_comm: Option<Vec<u8>> = db
        .conn()
        .query_row(
            "SELECT gov_comm FROM bundles
              WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = 0",
            rusqlite::params![ROUND_ID, W],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        gov_comm,
        Some(vec![0xAA; 32]),
        "nothing may have been written"
    );
}
