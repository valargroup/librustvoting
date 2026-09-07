//! Completion of a vote that reached the chain, as the storage queries see it.
//!
//! A vote is on chain when either witness of its confirmation is present:
//! hash confirmation writes `votes.tx_hash`, tree confirmation writes
//! `votes.vc_tree_position` and no hash, which the schema requires of it. Each
//! case here reads a query that once accepted the hash alone, and pins the
//! answer a tree-confirmed vote must get.

use super::*;

/// A bundle whose vote POST was dispatched must not still expect its
/// delegation VAN in the tree.
///
/// Casting spends the delegation VAN and replaces it with a successor, so
/// tree sync stops checking `gov_comm` once a bundle has voted. It decided
/// that from `votes.tx_hash`, which a dispatched-but-unclassified vote does
/// not yet have — and the omission was self-sustaining: the stale
/// expectation failed tree sync, the failure aborted the cast that would
/// have classified the vote, so no hash was ever written and the next pass
/// failed identically. A committed `chain_submissions` row is the
/// authoritative fact, because it means a POST was released.
#[test]
fn a_dispatched_vote_retires_its_bundles_van_expectation() {
    let db = test_db();
    let conn = db.conn();
    queries::insert_round(&conn, W, Network::Testnet, &test_params(), None).unwrap();
    queries::insert_bundle(&conn, "test-round-1", W, 0, &[]).unwrap();
    conn.execute(
        "UPDATE bundles SET van_leaf_position = 3, gov_comm = ?3
         WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = 0",
        rusqlite::params!["test-round-1", W, vec![0u8; 32]],
    )
    .unwrap();
    queries::store_vote(&conn, "test-round-1", W, 0, 1, 0, &[0xCC; 32]).unwrap();

    // No POST yet: the delegation VAN is still the bundle's current VAN.
    let entries = queries::load_van_tree_entries(&conn, "test-round-1", W).unwrap();
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0].expected_delegation_van.is_some(),
        "a bundle that has not voted still expects its delegation VAN"
    );

    // A dispatched vote with no hash yet — exactly what a crash between
    // POST and classification leaves.
    conn.execute(
        "INSERT INTO chain_submissions
         (identity_key, round_id, wallet_id, network, bundle_index, kind,
          proposal_id, generation_digest, state, committed_post_reservations,
          created_at, updated_at)
         VALUES (?1, ?2, ?3, 'testnet', 0, 'vote', 1, ?4, 'submitting', 1, 9, 9)",
        rusqlite::params![vec![0xABu8; 32], "test-round-1", W, vec![0xCDu8; 32]],
    )
    .unwrap();

    let entries = queries::load_van_tree_entries(&conn, "test-round-1", W).unwrap();
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0].expected_delegation_van.is_none(),
        "a dispatched vote spends the delegation VAN, so it must not be expected"
    );
}

/// A terminally rejected vote still holds its bundle's delegation VAN.
///
/// What retires the expectation is a POST that may have spent the VAN, and a
/// definite rejection spent nothing. `rejected` is terminal — no observation
/// transitions out of it and `reserve_recovery_retry` accepts only
/// `Recovering` — so the row can never acquire an outstanding POST later, and
/// it is never deleted. Counting it would retire the expectation for the rest
/// of the round and skip the delegation-leaf check silently.
#[test]
fn a_terminally_rejected_vote_keeps_its_bundles_van_expectation() {
    let db = test_db();
    let conn = db.conn();
    seed_bundle_with_delegation_van(&conn);
    insert_vote_submission(&conn, 0xAB, "rejected", Some("chain_rejected"));

    let entries = queries::load_van_tree_entries(&conn, "test-round-1", W).unwrap();
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0].expected_delegation_van.is_some(),
        "a rejected vote spent nothing, so its delegation VAN must still be expected"
    );
}

/// A rejection that is still in recovery retires the expectation anyway.
///
/// Its last classified outcome spent nothing, but the row is not terminal.
/// Exact-tree recovery reaches any `Recovering` row whatever its diagnostic,
/// and a no-match reserves a retry and releases a POST while leaving `state`
/// and `diagnostic_kind` exactly as they are. Between that reservation and its
/// classification the row is indistinguishable from a quiescent rejection, so
/// it must be read as possibly-spent: restoring the expectation instead would
/// fail tree sync against the successor leaf, aborting the cast that would
/// classify the retry — the deadlock this query exists to break.
#[test]
fn a_rejection_still_in_recovery_retires_the_van_expectation() {
    let db = test_db();
    let conn = db.conn();
    seed_bundle_with_delegation_van(&conn);
    insert_vote_submission(&conn, 0xAB, "recovering", Some("chain_rejected"));

    let entries = queries::load_van_tree_entries(&conn, "test-round-1", W).unwrap();
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0].expected_delegation_van.is_none(),
        "a retry may already be in flight, so the VAN must be treated as spent"
    );
}

/// A live submission on the bundle outweighs a rejected one beside it.
///
/// One proposal's vote was rejected and spent nothing; another's was released
/// and may have spent the VAN. Both are `vote` rows on the same bundle, which
/// is the granularity the VAN lives at, so the exclusion must not let the dead
/// row hide the live one.
#[test]
fn a_live_submission_beside_a_rejected_one_retires_the_expectation() {
    let db = test_db();
    let conn = db.conn();
    seed_bundle_with_delegation_van(&conn);
    insert_vote_submission_for(&conn, 0xAB, 1, "rejected", Some("chain_rejected"));
    insert_vote_submission_for(&conn, 0xBC, 2, "submitting", None);

    let entries = queries::load_van_tree_entries(&conn, "test-round-1", W).unwrap();
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0].expected_delegation_van.is_none(),
        "the live generation may have spent the VAN, whatever became of the dead one"
    );
}

/// A bundle holding its delegation VAN at leaf 3, with a vote drafted but no
/// submission yet.
fn seed_bundle_with_delegation_van(conn: &Connection) {
    queries::insert_round(conn, W, Network::Testnet, &test_params(), None).unwrap();
    queries::insert_bundle(conn, "test-round-1", W, 0, &[]).unwrap();
    conn.execute(
        "UPDATE bundles SET van_leaf_position = 3, gov_comm = ?3
         WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = 0",
        rusqlite::params!["test-round-1", W, vec![0u8; 32]],
    )
    .unwrap();
    queries::store_vote(conn, "test-round-1", W, 0, 1, 0, &[0xCC; 32]).unwrap();
}

/// One `chain_submissions` row for the bundle's vote on proposal 1.
fn insert_vote_submission(conn: &Connection, identity: u8, state: &str, diagnostic: Option<&str>) {
    insert_vote_submission_for(conn, identity, 1, state, diagnostic);
}

/// One `chain_submissions` row for the bundle's vote on `proposal_id`, in the
/// given durable state. `chain_submissions_identity` is unique per proposal,
/// so two rows on one bundle must name different ones.
fn insert_vote_submission_for(
    conn: &Connection,
    identity: u8,
    proposal_id: i64,
    state: &str,
    diagnostic_kind: Option<&str>,
) {
    conn.execute(
        "INSERT INTO chain_submissions
         (identity_key, round_id, wallet_id, network, bundle_index, kind,
          proposal_id, generation_digest, state, committed_post_reservations,
          diagnostic_kind, diagnostic, created_at, updated_at)
         VALUES (?1, ?2, ?3, 'testnet', 0, 'vote', ?8, ?4, ?5, 1, ?6, ?7, 9, 9)",
        rusqlite::params![
            vec![identity; 32],
            "test-round-1",
            W,
            vec![identity.wrapping_add(1); 32],
            state,
            diagnostic_kind,
            diagnostic_kind.map(|_| "vote chain rejected the transaction"),
            proposal_id,
        ],
    )
    .unwrap();
}

/// A vote confirmed by an exact-tree scan must clear its proposal's
/// authority bit.
///
/// The bit says the bundle may still vote that proposal, so it must clear
/// once the vote reaches the chain; otherwise the next vote on the bundle
/// rebuilds its vote-authority note as though that proposal had never
/// voted, and the chain rejects the stale nullifier as already spent. The
/// bit used to clear on `votes.tx_hash` alone, which a tree confirmation
/// never writes — permanently, since no later pass can supply a hash that
/// does not exist.
#[test]
fn a_tree_confirmed_vote_clears_its_proposal_authority_bit() {
    let db = test_db();
    let conn = db.conn();
    queries::insert_round(&conn, W, Network::Testnet, &test_params(), None).unwrap();
    queries::insert_bundle(&conn, "test-round-1", W, 0, &[]).unwrap();
    conn.execute(
        "UPDATE bundles SET van_comm_rand = ?3, total_note_value = 125000000,
                            address_index = 0
         WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = 0",
        rusqlite::params!["test-round-1", W, vec![0u8; 32]],
    )
    .unwrap();
    queries::store_vote(&conn, "test-round-1", W, 0, 1, 0, &[0xCC; 32]).unwrap();

    let all_open = queries::load_zkp2_inputs(&conn, "test-round-1", W, 0).unwrap();
    assert_eq!(
        all_open.proposal_authority,
        voting_circuits::MAX_PROPOSAL_AUTHORITY,
        "a vote that has not reached the chain leaves its bit set"
    );

    // Exactly what an exact-tree confirmation leaves: a position, no hash.
    conn.execute(
        "UPDATE votes SET tx_hash = NULL, vc_tree_position = 7
         WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = 0 AND proposal_id = 1",
        rusqlite::params!["test-round-1", W],
    )
    .unwrap();

    let after = queries::load_zkp2_inputs(&conn, "test-round-1", W, 0).unwrap();
    assert_eq!(
        after.proposal_authority,
        voting_circuits::MAX_PROPOSAL_AUTHORITY & !(1u64 << 1),
        "a tree-confirmed vote must clear its proposal's bit"
    );
}

/// A vote confirmed by an exact-tree scan must not be replaceable.
///
/// `store_vote` refuses to change the choice or commitment of a vote that
/// reached the chain, because the proposal's authority has already moved
/// and a replacement would describe a vote that cannot be cast. It used to
/// decide that by reading the transaction hash alone, which an exact-tree
/// confirmation leaves absent by construction — so the refusal never fired
/// and the replacement silently overwrote an on-chain vote.
#[test]
fn a_tree_confirmed_vote_cannot_be_replaced() {
    let db = test_db();
    let conn = db.conn();
    queries::insert_round(&conn, W, Network::Testnet, &test_params(), None).unwrap();
    queries::insert_bundle(&conn, "test-round-1", W, 0, &[]).unwrap();

    let commitment = vec![0xCC; 128];
    queries::store_vote(&conn, "test-round-1", W, 0, 0, 0, &commitment).unwrap();
    // Exactly what an exact-tree confirmation leaves: a commitment-tree
    // position and no hash, which the schema requires of it.
    conn.execute(
        "UPDATE votes SET tx_hash = NULL, vc_tree_position = 7
         WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = 0 AND proposal_id = 0",
        rusqlite::params!["test-round-1", W],
    )
    .unwrap();

    let replace_err =
        queries::store_vote(&conn, "test-round-1", W, 0, 0, 1, &commitment).unwrap_err();
    assert!(
        replace_err
            .to_string()
            .contains("cannot replace submitted vote"),
        "{replace_err}"
    );
}
