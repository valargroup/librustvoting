//! Guards that must refuse an act on a vote that already reached the chain.
//!
//! Each of these once read `votes.tx_hash` alone. A hash exists only for a
//! hash-confirmed submission — the schema requires `confirmation_source =
//! 'tree'` to carry none — so a vote confirmed by an exact-tree scan answered
//! every one of them wrongly, and permanently. Two of these guards failed open,
//! which is why they are tested rather than assumed.

use super::*;

/// A tree-confirmed vote must not block its own bundle as a competing
/// pending vote chain.
///
/// The guard refuses to prepare a second vote chain on a bundle that
/// already has an unresolved one, so it must ask whether the existing vote
/// is *unresolved*. It used to accept a transaction hash as resolution too,
/// which a tree confirmation never writes — so a vote confirmed by an
/// exact-tree scan looked permanently pending and locked its bundle out of
/// every later proposal, asking the caller to confirm a vote already
/// confirmed. The commitment-tree position is the resolution both routes
/// write, so it alone answers the question.
#[test]
fn a_tree_confirmed_vote_is_not_a_competing_pending_chain() {
    let db = db_with_vote();
    db.conn()
        .execute(
            "UPDATE votes SET commitment_bundle_json = '{}', tx_hash = NULL,
                              vc_tree_position = 7
             WHERE round_id = ?1 AND bundle_index = 0 AND proposal_id = 1",
            rusqlite::params![ROUND_ID],
        )
        .unwrap();

    // Proposal 1 is resolved, so preparing proposal 2 on the same bundle
    // must be allowed.
    ensure_no_competing_pending_vote_chain_with_conn(&db.conn(), WALLET_ID, ROUND_ID, 0, &[2])
        .expect("a tree-confirmed vote does not block a later proposal");

    // A vote that really is unresolved still blocks it.
    db.conn()
        .execute(
            "UPDATE votes SET vc_tree_position = NULL
             WHERE round_id = ?1 AND bundle_index = 0 AND proposal_id = 1",
            rusqlite::params![ROUND_ID],
        )
        .unwrap();
    let error =
        ensure_no_competing_pending_vote_chain_with_conn(&db.conn(), WALLET_ID, ROUND_ID, 0, &[2])
            .expect_err("an unresolved vote chain must still block a later proposal");
    assert!(error.to_string().contains("pending vote chain"), "{error}");
}

/// A ballot intent that conflicts with a tree-confirmed vote must be
/// refused.
///
/// Once a vote is on chain its proposal's authority has moved, so a
/// different choice for it can no longer be honoured. The guard used to
/// find the conflicting bundle by reading the transaction hash alone, which
/// an exact-tree confirmation leaves absent by construction. Like the
/// rebuild guard it failed open: the conflicting intent was accepted and
/// the disagreement surfaced later, if at all.
#[test]
fn an_intent_conflicting_with_a_tree_confirmed_vote_is_refused() {
    let db = db_with_vote();
    db.conn()
        .execute(
            "UPDATE votes SET tx_hash = NULL, vc_tree_position = 7, choice = 2
             WHERE round_id = ?1 AND bundle_index = 0 AND proposal_id = 1",
            rusqlite::params![ROUND_ID],
        )
        .unwrap();

    // A different choice for a proposal already voted on chain.
    let error = queries::ensure_no_submitted_vote_conflict_for_intent(
        &db.conn(),
        ROUND_ID,
        WALLET_ID,
        1,
        false,
        Some(0),
    )
    .expect_err("a choice conflicting with an on-chain vote must be refused");
    assert!(error.to_string().contains("submitted"), "{error}");
}

/// A vote confirmed by an exact-tree scan must not be rebuildable.
///
/// Rebuilding a vote already on chain would produce a competing generation
/// for a proposal whose authority has already moved. The guard used to ask
/// only whether a transaction hash was recorded — but a hash exists only
/// for hash-confirmed submissions, so an exact-tree confirmation left none
/// and the refusal had nothing to fire on. That failed open, which is why
/// it is tested rather than assumed: a stuck round announces itself, a
/// permitted rebuild does not.
#[test]
fn a_tree_confirmed_vote_cannot_be_rebuilt() {
    let db = db_with_vote();
    // Exactly what an exact-tree confirmation leaves: a commitment-tree
    // position and no transaction hash, which the schema requires of it.
    db.conn()
        .execute(
            "UPDATE votes SET tx_hash = NULL, vc_tree_position = 7
             WHERE round_id = ?1 AND bundle_index = 0 AND proposal_id = 1",
            rusqlite::params![ROUND_ID],
        )
        .unwrap();

    let error = ensure_vote_rebuild_allowed(&db, ROUND_ID, 0, 1)
        .expect_err("a vote already on chain must not be rebuilt");
    assert!(
        error.to_string().contains("submitted") || error.to_string().contains("rebuild"),
        "{error}"
    );
}
