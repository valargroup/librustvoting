//! Vote-tree client caching: which transport a wallet's sync actually uses.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use vote_commitment_tree_client::transport::{Transport, TransportError, TransportResponse};

use super::{reset_vote_tree, sync_vote_tree, sync_vote_tree_with, vote_tree_sync_for};
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
fn a_routed_sync_replaces_a_cached_direct_client() {
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
fn a_second_transport_rebinds_the_wallets_client() {
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

    let first = vote_tree_sync_for(&db, Some(transport.clone())).unwrap();
    let second = vote_tree_sync_for(&db, Some(transport.clone())).unwrap();
    // Rebinding here would throw away the synced tree state that
    // `generate_van_witness` depends on.
    assert!(Arc::ptr_eq(&first, &second));

    let unrouted = vote_tree_sync_for(&db, None).unwrap();
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
