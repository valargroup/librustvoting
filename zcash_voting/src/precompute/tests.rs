//! Vote-tree client caching: which transport a wallet's sync actually uses,
//! and which client an unrouted call lands on.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use vote_commitment_tree_client::transport::{Transport, TransportError, TransportResponse};

use super::{
    cached_vote_tree_rounds, reset_vote_tree, sync_vote_tree, sync_vote_tree_with, vote_tree_for,
    vote_tree_registry::vote_tree_for_round,
};
use crate::round::VotingDb;

const ROUND_ID: &str = "0101010101010101010101010101010101010101010101010101010101010101";
const NODE_URL: &str = "http://node.invalid";

/// Counts the requests routed through it and then fails, so a sync reaches the
/// transport without needing a live node.
struct CountingTransport {
    requests: AtomicUsize,
}

impl CountingTransport {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: AtomicUsize::new(0),
        })
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

impl Transport for CountingTransport {
    fn get(&self, _url: &str) -> Result<TransportResponse, TransportError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        Err(TransportError::Request("counted".to_string()))
    }
}

/// The client registry is keyed by wallet id and lives for the process, so
/// every test needs its own wallet and has to leave the registry clean.
fn db(wallet_id: &str) -> VotingDb {
    let db = VotingDb::open_in_memory().unwrap();
    db.set_wallet_id(wallet_id);
    db
}

#[test]
fn a_routed_sync_is_not_served_the_direct_client() {
    let db = db("wallet-routed-after-direct");
    // Binds the wallet's client to the SDK's direct transport.
    sync_vote_tree(&db, ROUND_ID, NODE_URL).unwrap_err();

    let routed = CountingTransport::new();
    sync_vote_tree_with(&db, ROUND_ID, NODE_URL, routed.clone()).unwrap_err();

    assert_eq!(
        routed.requests(),
        1,
        "a routed sync must not be served the cached direct client"
    );
    reset_vote_tree(&db, "").unwrap();
}

#[test]
fn a_second_transport_gets_its_own_client() {
    let db = db("wallet-second-transport");
    let first = CountingTransport::new();
    sync_vote_tree_with(&db, ROUND_ID, NODE_URL, first.clone()).unwrap_err();

    let second = CountingTransport::new();
    sync_vote_tree_with(&db, ROUND_ID, NODE_URL, second.clone()).unwrap_err();

    assert_eq!(first.requests(), 1);
    assert_eq!(
        second.requests(),
        1,
        "the second route must carry its own sync"
    );
    reset_vote_tree(&db, "").unwrap();
}

#[test]
fn an_unrouted_sync_keeps_the_wallet_on_its_routed_client() {
    let db = db("wallet-no-downgrade");
    let routed = CountingTransport::new();
    sync_vote_tree_with(&db, ROUND_ID, NODE_URL, routed.clone()).unwrap_err();

    // Asking for no particular route must not silently move an already routed
    // wallet back onto the direct transport.
    sync_vote_tree(&db, ROUND_ID, NODE_URL).unwrap_err();

    assert_eq!(routed.requests(), 2);
    reset_vote_tree(&db, "").unwrap();
}

#[test]
fn the_same_transport_keeps_the_cached_client() {
    let db = db("wallet-same-transport");
    let transport = CountingTransport::new();

    let first = vote_tree_for(&db, Some(transport.clone())).unwrap();
    let second = vote_tree_for(&db, Some(transport.clone())).unwrap();
    // Rebinding here would throw away the synced tree state that
    // `generate_van_witness` depends on.
    assert!(Arc::ptr_eq(&first, &second));

    let unrouted = vote_tree_for(&db, None).unwrap();
    assert!(Arc::ptr_eq(&first, &unrouted));

    reset_vote_tree(&db, "").unwrap();
}

#[test]
fn an_account_wide_reset_lets_the_next_sync_bind_a_new_transport() {
    let db = db("wallet-reset-rebinds");
    let first = CountingTransport::new();
    sync_vote_tree_with(&db, ROUND_ID, NODE_URL, first.clone()).unwrap_err();
    reset_vote_tree(&db, "").unwrap();

    sync_vote_tree(&db, ROUND_ID, NODE_URL).unwrap_err();
    assert_eq!(
        first.requests(),
        1,
        "a reset forgets the transport the client was created with"
    );
    reset_vote_tree(&db, "").unwrap();
}

#[test]
fn a_retained_tree_handle_keeps_its_round_state_when_the_wallet_rebinds() {
    let db = db("wallet-retained-handle");
    let first = CountingTransport::new();
    let tree = vote_tree_for(&db, Some(first.clone())).unwrap();
    // The sync fails at the transport but has already created the round's
    // client on this handle, which is the state a witness needs.
    tree.sync(&db, ROUND_ID, NODE_URL).unwrap_err();
    assert_eq!(tree.cached_rounds(), vec![ROUND_ID.to_string()]);

    // Another executor for the same wallet binds a different transport.
    let second = CountingTransport::new();
    let other = vote_tree_for(&db, Some(second.clone())).unwrap();
    assert!(!Arc::ptr_eq(&tree, &other));
    assert!(other.cached_rounds().is_empty());

    // The retained handle is unaffected, and the registry still hands the
    // first transport its own client rather than a fresh one.
    assert_eq!(tree.cached_rounds(), vec![ROUND_ID.to_string()]);
    let again = vote_tree_for(&db, Some(first.clone())).unwrap();
    assert!(
        Arc::ptr_eq(&tree, &again),
        "a second transport must not evict the first transport's client"
    );
    reset_vote_tree(&db, "").unwrap();
}

#[test]
fn an_unrouted_round_lookup_prefers_the_client_holding_that_round() {
    const OTHER_ROUND: &str = "0202020202020202020202020202020202020202020202020202020202020202";
    let db = db("wallet-round-holder");
    let first = CountingTransport::new();
    let holds_round = vote_tree_for(&db, Some(first.clone())).unwrap();
    holds_round.sync(&db, ROUND_ID, NODE_URL).unwrap_err();

    // A later executor syncs a different round over its own transport; it is
    // now the most recently used client.
    let second = CountingTransport::new();
    let holds_other = vote_tree_for(&db, Some(second.clone())).unwrap();
    holds_other.sync(&db, OTHER_ROUND, NODE_URL).unwrap_err();

    // The standalone sync -> witness path names no transport and must land on
    // the client that holds the round it asks about.
    assert!(Arc::ptr_eq(
        &vote_tree_for_round(&db, ROUND_ID).unwrap(),
        &holds_round
    ));
    assert!(Arc::ptr_eq(
        &vote_tree_for_round(&db, OTHER_ROUND).unwrap(),
        &holds_other
    ));
    // An unrouted sync of the first round continues on its client too.
    sync_vote_tree(&db, ROUND_ID, NODE_URL).unwrap_err();
    assert_eq!(first.requests(), 2);
    assert_eq!(second.requests(), 1);
    reset_vote_tree(&db, "").unwrap();
}

#[test]
fn a_round_reset_clears_the_round_on_every_client() {
    let db = db("wallet-reset-everywhere");
    let first = CountingTransport::new();
    let second = CountingTransport::new();
    vote_tree_for(&db, Some(first.clone()))
        .unwrap()
        .sync(&db, ROUND_ID, NODE_URL)
        .unwrap_err();
    vote_tree_for(&db, Some(second.clone()))
        .unwrap()
        .sync(&db, ROUND_ID, NODE_URL)
        .unwrap_err();
    assert_eq!(cached_vote_tree_rounds(&db), vec![ROUND_ID.to_string()]);

    reset_vote_tree(&db, ROUND_ID).unwrap();

    assert!(vote_tree_for(&db, Some(first.clone()))
        .unwrap()
        .cached_rounds()
        .is_empty());
    assert!(vote_tree_for(&db, Some(second.clone()))
        .unwrap()
        .cached_rounds()
        .is_empty());
    reset_vote_tree(&db, "").unwrap();
}

#[test]
fn a_transport_nobody_holds_releases_its_client() {
    let db = db("wallet-dropped-transport");
    let routed = CountingTransport::new();
    sync_vote_tree_with(&db, ROUND_ID, NODE_URL, routed.clone()).unwrap_err();
    assert_eq!(cached_vote_tree_rounds(&db), vec![ROUND_ID.to_string()]);

    drop(routed);

    assert!(
        cached_vote_tree_rounds(&db).is_empty(),
        "a client over a transport no caller can name again is pruned"
    );
    reset_vote_tree(&db, "").unwrap();
}

#[test]
fn two_sidecars_with_one_wallet_id_keep_separate_tree_state_and_routes() {
    let first_sidecar = db("wallet-shared-name");
    let second_sidecar = db("wallet-shared-name");
    let routed = CountingTransport::new();
    // The first sidecar binds a routed client and creates the round's state.
    sync_vote_tree_with(&first_sidecar, ROUND_ID, NODE_URL, routed.clone()).unwrap_err();
    assert_eq!(routed.requests(), 1);

    // An unrouted sync on the other sidecar must neither reuse that route nor
    // see that round state.
    assert!(cached_vote_tree_rounds(&second_sidecar).is_empty());
    sync_vote_tree(&second_sidecar, ROUND_ID, NODE_URL).unwrap_err();
    assert_eq!(
        routed.requests(),
        1,
        "the second sidecar must not travel the first sidecar's route"
    );
    assert!(!std::sync::Arc::ptr_eq(
        &vote_tree_for(&first_sidecar, None).unwrap(),
        &vote_tree_for(&second_sidecar, None).unwrap()
    ));

    reset_vote_tree(&first_sidecar, "").unwrap();
    reset_vote_tree(&second_sidecar, "").unwrap();
}

#[test]
fn a_dropped_sidecar_connection_no_longer_retains_its_tree_cache() {
    let client = {
        let short_lived = db("wallet-short-lived");
        sync_vote_tree(&short_lived, ROUND_ID, NODE_URL).unwrap_err();
        let client = Arc::downgrade(&vote_tree_for(&short_lived, None).unwrap());
        assert!(client.upgrade().is_some(), "the registry holds the client");
        client
    };
    // Every handle on the sidecar is gone; the next registry access prunes
    // the entry and with it the last reference to the client.
    let other = db("wallet-prunes-neighbours");
    assert!(cached_vote_tree_rounds(&other).is_empty());
    assert!(client.upgrade().is_none());
}
