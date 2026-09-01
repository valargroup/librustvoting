use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use sha2::{Digest, Sha256};

use super::*;
use crate::{
    chain::{
        transport::{ChainFuture, ChainResponse, ChainTransport, ChainTransportError},
        ChainClientConfig, ChainEndpointSet, ChainTxConfirmation,
    },
    round::RoundParams,
    storage::queries,
    Network,
};

mod cancellation_concurrency;
mod dispatch_reservation;
mod public_contract;
mod reconciliation;
mod recovery_coverage;

const ROUND_ID: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const TX_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const WALLET: &str = "wallet-1";
const TX_HASH_2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const TX_HASH_3: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

#[derive(Default)]
struct MockTransport {
    responses: Mutex<VecDeque<Result<ChainResponse, ChainTransportError>>>,
    posts: Mutex<usize>,
    gets: Mutex<usize>,
}

impl ChainTransport for MockTransport {
    fn get<'a>(&'a self, _url: &'a str, _timeout: Duration) -> ChainFuture<'a> {
        Box::pin(async move {
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("mock GET response");
            *self.gets.lock().unwrap() += 1;
            response
        })
    }

    fn post_json<'a>(
        &'a self,
        _url: &'a str,
        _body: Vec<u8>,
        _timeout: Duration,
    ) -> ChainFuture<'a> {
        Box::pin(async move {
            *self.posts.lock().unwrap() += 1;
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("mock POST response")
        })
    }
}

fn response(status: u16, body: &str) -> ChainResponse {
    ChainResponse::json(status, body.as_bytes().to_vec())
}

/// Rebuild for tests that post a fixed body rather than a real payload.
fn echo_rebuild(_conn: &rusqlite::Connection) -> Result<Vec<u8>, VotingError> {
    Ok(b"{}".to_vec())
}

fn file_test_db(path: &str) -> VotingDb {
    let db = VotingDb::open(path).unwrap();
    init_test_db(db)
}

fn test_db() -> VotingDb {
    init_test_db(VotingDb::open_in_memory().unwrap())
}

fn init_test_db(db: VotingDb) -> VotingDb {
    db.set_wallet_id("wallet-1");
    db.create_round(
        Network::Testnet,
        &RoundParams {
            vote_round_id: ROUND_ID.to_string(),
            snapshot_height: 100,
            ea_pk: vec![0xEA; 32],
            nc_root: vec![0xAA; 32],
            nullifier_imt_root: vec![0xBB; 32],
        },
        None,
    )
    .unwrap();
    db.conn()
        .execute(
            "INSERT INTO bundles (round_id, wallet_id, bundle_index)
                 VALUES (?1, ?2, 0)",
            rusqlite::params![ROUND_ID, db.wallet_id()],
        )
        .unwrap();
    db
}

/// Rebuild that reads a durable column, standing in for a real payload
/// reconstruction: the bytes it returns change when storage changes.
fn durable_rebuild(conn: &rusqlite::Connection) -> Result<Vec<u8>, VotingError> {
    conn.query_row(
        "SELECT COALESCE(gov_comm, X'') FROM bundles WHERE round_id=?1 AND bundle_index=0",
        rusqlite::params![ROUND_ID],
        |row| row.get::<_, Vec<u8>>(0),
    )
    .map_err(|error| VotingError::Internal {
        message: format!("test rebuild failed: {error}"),
    })
}

fn set_generation(db: &VotingDb, generation: &[u8]) {
    db.conn()
        .execute(
            "UPDATE bundles SET gov_comm=?1 WHERE round_id=?2 AND bundle_index=0",
            rusqlite::params![generation, ROUND_ID],
        )
        .unwrap();
}

fn accepted_client(transport: Arc<MockTransport>) -> ChainClient {
    ChainClient::new(
        transport,
        ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
    )
}

/// Stores a vote row whose columns agree with the recovery it carries, the
/// way the vote lifecycle writes them.
fn store_vote_with_recovery(
    db: &VotingDb,
    proposal_id: u32,
    recovery: &crate::vote::VoteRecoveryBundle,
) {
    let commitment = crate::vote::stored_vote_commitment_bytes(recovery).unwrap();
    queries::store_vote(
        &db.conn(),
        ROUND_ID,
        WALLET,
        0,
        proposal_id,
        recovery.vote_decision,
        &commitment,
    )
    .unwrap();
    db.conn()
        .execute(
            "UPDATE votes SET commitment_bundle_json=?1
                  WHERE round_id=?2 AND wallet_id=?3 AND bundle_index=0 AND proposal_id=?4",
            rusqlite::params![
                crate::vote::serialize_recovery(recovery).unwrap(),
                ROUND_ID,
                WALLET,
                proposal_id as i64
            ],
        )
        .unwrap();
}

fn journal_attempt(db: &VotingDb, state: &str, tx_hash: Option<&str>) -> i64 {
    let conn = db.conn();
    conn.execute(
        "INSERT INTO chain_submission_attempts
             (round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
              payload_digest, chain_tx_hash, state, created_at, updated_at)
             VALUES (?1, ?2, 'delegation', 0, -1, X'', ?3, ?4, ?5, 1, 1)",
        rusqlite::params![ROUND_ID, WALLET, vec![0xCC_u8; 32], tx_hash, state],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn journal_vote_attempt(db: &VotingDb, state: &str, tx_hash: Option<&str>) {
    db.conn()
        .execute(
            "INSERT INTO chain_submission_attempts
                 (round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
                  payload_digest, chain_tx_hash, state, created_at, updated_at)
                 VALUES (?1, ?2, 'vote', 0, 3, X'', ?3, ?4, ?5, 1, 1)",
            rusqlite::params![ROUND_ID, WALLET, vec![0xCC_u8; 32], tx_hash, state],
        )
        .unwrap();
}

fn attempt_states(db: &VotingDb) -> Vec<String> {
    db.conn()
        .prepare("SELECT state FROM chain_submission_attempts ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn delegate_vote_events(leaf_index: &str) -> Vec<crate::confirmation::TxEvent> {
    vec![crate::confirmation::TxEvent {
        event_type: "delegate_vote".to_string(),
        attributes: vec![
            crate::confirmation::TxEventAttribute {
                key: "vote_round_id".to_string(),
                value: ROUND_ID.to_string(),
            },
            crate::confirmation::TxEventAttribute {
                key: "leaf_index".to_string(),
                value: leaf_index.to_string(),
            },
        ],
    }]
}

/// A transport that lets another writer record a candidate between this
/// call's preflight and its response, the way a second operation or a
/// legacy recording call would.
struct RacingTransport {
    inner: MockTransport,
    db_path: String,
    raced: Mutex<bool>,
}

impl ChainTransport for RacingTransport {
    fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> ChainFuture<'a> {
        self.inner.get(url, timeout)
    }

    fn post_json<'a>(&'a self, url: &'a str, body: Vec<u8>, timeout: Duration) -> ChainFuture<'a> {
        {
            let mut raced = self.raced.lock().unwrap();
            if !*raced {
                *raced = true;
                let conn = rusqlite::Connection::open(&self.db_path).unwrap();
                conn.execute(
                    "INSERT INTO chain_submission_attempts
                         (round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
                          payload_digest, chain_tx_hash, state, created_at, updated_at)
                         VALUES (?1, ?2, 'vote', 0, 3, X'', ?3, ?4, 'accepted', 1, 1)",
                    rusqlite::params![ROUND_ID, WALLET, vec![0xEE_u8; 32], TX_HASH_2],
                )
                .unwrap();
            }
        }
        self.inner.post_json(url, body, timeout)
    }
}

/// Flips a cancellation flag once the status request has actually been
/// issued, so cancellation lands after the lookup rather than before it.
struct CancelOnLookupTransport {
    inner: MockTransport,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl ChainTransport for CancelOnLookupTransport {
    fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> ChainFuture<'a> {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.inner.get(url, timeout)
    }

    fn post_json<'a>(&'a self, url: &'a str, body: Vec<u8>, timeout: Duration) -> ChainFuture<'a> {
        self.inner.post_json(url, body, timeout)
    }
}

/// Records a racing accepted candidate during the POST, then cancels, so
/// the rejection branch's reconciliation reaches no conclusion.
struct RacingThenCancelTransport {
    inner: MockTransport,
    db_path: String,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl ChainTransport for RacingThenCancelTransport {
    fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> ChainFuture<'a> {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.inner.get(url, timeout)
    }

    fn post_json<'a>(&'a self, url: &'a str, body: Vec<u8>, timeout: Duration) -> ChainFuture<'a> {
        let conn = rusqlite::Connection::open(&self.db_path).unwrap();
        conn.execute(
            "INSERT INTO chain_submission_attempts
                 (round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
                  payload_digest, chain_tx_hash, state, created_at, updated_at)
                 VALUES (?1, ?2, 'delegation', 0, -1, X'', ?3, ?4, 'accepted', 1, 1)",
            rusqlite::params![ROUND_ID, WALLET, vec![0xEE_u8; 32], TX_HASH_2],
        )
        .unwrap();
        self.inner.post_json(url, body, timeout)
    }
}

/// Times out the POST after recording a racing candidate, then cancels once
/// the between-retry reconciliation looks that candidate up.
struct AmbiguousThenCancelTransport {
    inner: MockTransport,
    db_path: String,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl ChainTransport for AmbiguousThenCancelTransport {
    fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> ChainFuture<'a> {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.inner.get(url, timeout)
    }

    fn post_json<'a>(&'a self, url: &'a str, body: Vec<u8>, timeout: Duration) -> ChainFuture<'a> {
        let conn = rusqlite::Connection::open(&self.db_path).unwrap();
        conn.execute(
            "INSERT INTO chain_submission_attempts
                 (round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
                  payload_digest, chain_tx_hash, state, created_at, updated_at)
                 VALUES (?1, ?2, 'delegation', 0, -1, X'', ?3, ?4, 'accepted', 1, 1)",
            rusqlite::params![ROUND_ID, WALLET, vec![0xEE_u8; 32], TX_HASH_2],
        )
        .unwrap();
        self.inner.post_json(url, body, timeout)
    }
}

/// Applies a durable confirmation through its own connection while the
/// lookup is in flight, the way another process would.
struct ConfirmDuringLookupTransport {
    inner: MockTransport,
    db_path: String,
    applied: Mutex<bool>,
}

impl ChainTransport for ConfirmDuringLookupTransport {
    fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> ChainFuture<'a> {
        {
            let mut applied = self.applied.lock().unwrap();
            if !*applied {
                *applied = true;
                let conn = rusqlite::Connection::open(&self.db_path).unwrap();
                conn.execute(
                    "UPDATE bundles SET delegation_tx_hash=?3, van_leaf_position=5
                          WHERE round_id=?1 AND wallet_id=?2 AND bundle_index=0",
                    rusqlite::params![ROUND_ID, WALLET, TX_HASH],
                )
                .unwrap();
            }
        }
        self.inner.get(url, timeout)
    }

    fn post_json<'a>(&'a self, url: &'a str, body: Vec<u8>, timeout: Duration) -> ChainFuture<'a> {
        self.inner.post_json(url, body, timeout)
    }
}

/// Records a racing candidate on the *second* POST only, so the rejection
/// branch is the first place that candidate is reconciled.
struct CandidateOnSecondPostTransport {
    inner: MockTransport,
    db_path: String,
    posts: Mutex<usize>,
}

impl ChainTransport for CandidateOnSecondPostTransport {
    fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> ChainFuture<'a> {
        self.inner.get(url, timeout)
    }

    fn post_json<'a>(&'a self, url: &'a str, body: Vec<u8>, timeout: Duration) -> ChainFuture<'a> {
        {
            let mut posts = self.posts.lock().unwrap();
            *posts += 1;
            if *posts == 2 {
                let conn = rusqlite::Connection::open(&self.db_path).unwrap();
                conn.execute(
                    "INSERT INTO chain_submission_attempts
                         (round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
                          payload_digest, chain_tx_hash, state, created_at, updated_at)
                         VALUES (?1, ?2, 'delegation', 0, -1, X'', ?3, ?4, 'accepted', 1, 1)",
                    rusqlite::params![ROUND_ID, WALLET, vec![0xEE_u8; 32], TX_HASH_2],
                )
                .unwrap();
            }
        }
        self.inner.post_json(url, body, timeout)
    }
}

/// Journals a second candidate while the first one is being looked up.
struct JournalDuringLookup {
    inner: MockTransport,
    db_path: String,
    done: Mutex<bool>,
}
impl ChainTransport for JournalDuringLookup {
    fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> ChainFuture<'a> {
        {
            let mut done = self.done.lock().unwrap();
            if !*done {
                *done = true;
                let conn = rusqlite::Connection::open(&self.db_path).unwrap();
                conn.execute(
                    "INSERT INTO chain_submission_attempts
                         (round_id, wallet_id, kind, bundle_index, proposal_id, batch_digest,
                          payload_digest, chain_tx_hash, state, created_at, updated_at)
                         VALUES (?1, ?2, 'delegation', 0, -1, X'', ?3, ?4, 'accepted', 1, 1)",
                    rusqlite::params![ROUND_ID, WALLET, vec![0xEE_u8; 32], TX_HASH_2],
                )
                .unwrap();
            }
        }
        self.inner.get(url, timeout)
    }
    fn post_json<'a>(&'a self, url: &'a str, body: Vec<u8>, timeout: Duration) -> ChainFuture<'a> {
        self.inner.post_json(url, body, timeout)
    }
}

/// Applies a durable confirmation while the final POST is in flight.
struct ConfirmDuringPostTransport {
    inner: MockTransport,
    db_path: String,
    done: Mutex<bool>,
}

impl ChainTransport for ConfirmDuringPostTransport {
    fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> ChainFuture<'a> {
        self.inner.get(url, timeout)
    }

    fn post_json<'a>(&'a self, url: &'a str, body: Vec<u8>, timeout: Duration) -> ChainFuture<'a> {
        {
            let mut done = self.done.lock().unwrap();
            if !*done {
                *done = true;
                let conn = rusqlite::Connection::open(&self.db_path).unwrap();
                conn.execute(
                    "UPDATE bundles SET delegation_tx_hash=?3, van_leaf_position=5
                          WHERE round_id=?1 AND wallet_id=?2 AND bundle_index=0",
                    rusqlite::params![ROUND_ID, WALLET, TX_HASH],
                )
                .unwrap();
            }
        }
        self.inner.post_json(url, body, timeout)
    }
}

#[tokio::test(start_paused = true)]
async fn the_in_flight_guard_is_released_before_the_backoff() {
    let db = test_db();
    // A bundle index no other test submits for: the in-flight registry is
    // process-global, so a shared identity would count another test's POST.
    let identity = ChainSubmissionIdentity::delegation(ROUND_ID, 7788);
    let transport = Arc::new(MockTransport::default());
    // Retryable, and journaled as rejected: once classified, nothing of
    // ours is outstanding.
    transport.responses.lock().unwrap().extend([
        Ok(response(429, r#"{"message":"slow down"}"#)),
        Ok(response(429, r#"{"message":"slow down"}"#)),
    ]);
    let client = ChainClient::with_config(
        transport,
        ChainEndpointSet::new(&["https://vote.example".to_string()]).unwrap(),
        ChainClientConfig::default()
            .with_retry_delays(vec![Duration::from_secs(30)])
            .unwrap(),
    );
    let lifecycle = ChainSubmissionLifecycle::new(&db, &client);

    // Sampled while the backoff is sleeping: the first POST completes at
    // once, so by the time this fires the call is waiting to retry.
    let sampled = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        in_flight_count(WALLET, &ChainSubmissionIdentity::delegation(ROUND_ID, 7788))
    };
    let submit = lifecycle.submit_canonical_payload_locked(
        WALLET,
        identity,
        b"{}".to_vec(),
        &echo_rebuild,
        &|| false,
    );
    let (during_backoff, _) = tokio::join!(sampled, submit);

    // Held across the backoff, this reads 1, and cleanup, ballot-intent
    // changes, and bundle pruning would all defer to a POST that has already
    // been answered — for a delay the host chooses.
    assert_eq!(during_backoff, 0);
}

/// Makes the durable-state read fail, by removing the table it reads,
/// once the POST has been answered.
struct BreakDurableReadTransport {
    inner: MockTransport,
    db_path: String,
    done: Mutex<bool>,
}

impl ChainTransport for BreakDurableReadTransport {
    fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> ChainFuture<'a> {
        self.inner.get(url, timeout)
    }

    fn post_json<'a>(&'a self, url: &'a str, body: Vec<u8>, timeout: Duration) -> ChainFuture<'a> {
        {
            let mut done = self.done.lock().unwrap();
            if !*done {
                *done = true;
                let conn = rusqlite::Connection::open(&self.db_path).unwrap();
                conn.execute_batch("DROP TABLE bundles").unwrap();
            }
        }
        self.inner.post_json(url, body, timeout)
    }
}
