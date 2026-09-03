mod migrations;
pub mod operations;
pub mod queries;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use rusqlite::{Connection, TransactionBehavior};

use crate::types::{Network, VotingError};

const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Current phase of a voting round.
///
/// Discriminants are ordered lifecycle ranks; `advance_round_phase` compares
/// them to enforce forward-only progression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum RoundPhase {
    Initialized = 0,
    HotkeyGenerated = 1,
    DelegationConstructed = 2,
    DelegationProved = 3,
    VoteReady = 4,
}

impl RoundPhase {
    pub fn from_i32(v: i32) -> Self {
        match v {
            0 => Self::Initialized,
            1 => Self::HotkeyGenerated,
            2 => Self::DelegationConstructed,
            3 => Self::DelegationProved,
            4 => Self::VoteReady,
            _ => Self::Initialized,
        }
    }
}

/// Summary state of a voting round (for UI / SDK queries).
#[derive(Clone, Debug)]
pub struct RoundState {
    pub round_id: String,
    pub phase: RoundPhase,
    pub network: Network,
    pub snapshot_height: u64,
    pub hotkey_address: Option<String>,
    pub delegated_weight: Option<u64>,
    pub proof_generated: bool,
}

/// A vote record from the votes table.
pub use crate::wire::VoteRecord;

/// Compact round info for list_rounds().
#[derive(Clone, Debug)]
pub struct RoundSummary {
    pub round_id: String,
    pub wallet_id: String,
    pub phase: RoundPhase,
    pub network: Network,
    pub snapshot_height: u64,
    pub created_at: u64,
}

/// A Keystone bundle signature stored in the DB.
pub use crate::wire::KeystoneSignatureRecord;

/// One Keystone signature tuple to store as part of an atomic batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeystoneSignatureInput {
    pub bundle_index: u32,
    pub sig: Vec<u8>,
    pub sighash: Vec<u8>,
    pub rk: Vec<u8>,
}

/// Counts from an idempotent atomic Keystone signature batch write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeystoneSignatureBatchResult {
    pub inserted: u32,
    pub already_present: u32,
}

/// One SQLite connection to a voting database, shared by every [`VotingDb`]
/// handle opened on the same sidecar path in this process.
///
/// In-process writers serialize on the connection mutex, so SQLite reports
/// `SQLITE_BUSY` only when another process holds the file.
pub(crate) struct SidecarConnection {
    conn: Mutex<Connection>,
    chain_submission_coordination: crate::chain_submission::coordination::SubmissionCoordination,
}

/// Database handle for voting state: a shared SQLite connection plus a
/// wallet identifier that scopes all round data to a single wallet.
///
/// Handles are cheap to clone through [`VotingDb::scoped`]; each carries its
/// own wallet id while sharing the connection and the process-local chain
/// submission coordination.
pub struct VotingDb {
    inner: Arc<SidecarConnection>,
    wallet_id: Mutex<String>,
}

impl VotingDb {
    /// Open (or create) the voting database at the given path.
    /// Runs migrations automatically.
    /// Call `set_wallet_id` before performing any round operations.
    ///
    /// Every call opens its own connection. Wallet integrations should use
    /// [`VotingDb::open_wallet_sidecar`], which shares one connection per
    /// sidecar path.
    pub fn open(path: &str) -> Result<Self, VotingError> {
        Ok(Self::from_connection(Self::open_connection(path)?))
    }

    pub(crate) fn open_connection(path: &str) -> Result<Connection, VotingError> {
        let mut conn = if path == ":memory:" {
            Connection::open_in_memory()
        } else {
            Connection::open(path)
        }
        .map_err(|e| VotingError::from_sqlite("failed to open database", &e))?;

        conn.busy_timeout(SQLITE_BUSY_TIMEOUT).map_err(|e| {
            VotingError::from_sqlite("failed to configure database busy timeout", &e)
        })?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| VotingError::from_sqlite("failed to set pragmas", &e))?;

        migrations::migrate(&mut conn)?;
        Ok(conn)
    }

    pub(crate) fn from_connection(conn: Connection) -> Self {
        Self {
            inner: Arc::new(SidecarConnection {
                conn: Mutex::new(conn),
                chain_submission_coordination: Default::default(),
            }),
            wallet_id: Mutex::new(String::new()),
        }
    }

    pub(crate) fn from_shared(inner: Arc<SidecarConnection>, wallet_id: &str) -> Self {
        Self {
            inner,
            wallet_id: Mutex::new(wallet_id.to_string()),
        }
    }

    pub(crate) fn shared_connection(&self) -> Arc<SidecarConnection> {
        Arc::clone(&self.inner)
    }

    /// Returns a handle on the same connection scoped to another wallet.
    ///
    /// Use this to read several accounts' state through one open sidecar
    /// instead of opening a connection per account.
    pub fn scoped(&self, wallet_id: &str) -> Self {
        Self::from_shared(Arc::clone(&self.inner), wallet_id)
    }

    /// Whether two handles share one underlying connection.
    pub fn shares_connection_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub(crate) fn chain_submission_coordination(
        &self,
    ) -> &crate::chain_submission::coordination::SubmissionCoordination {
        &self.inner.chain_submission_coordination
    }

    /// Runs `body` inside one `BEGIN IMMEDIATE` transaction and commits it.
    ///
    /// SQLite waits up to its busy timeout for another process to release the
    /// write lock; a failure past that timeout surfaces as
    /// [`VotingError::DbBusy`] so hosts can retry later instead of parsing
    /// text. `body` must be pure over the database: it must not perform
    /// network I/O or proof work while the lock is held.
    pub(crate) fn write_transaction<T>(
        &self,
        context: &str,
        body: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T, VotingError>,
    ) -> Result<T, VotingError> {
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| VotingError::from_sqlite(context, &e))?;
        let value = body(&tx)?;
        tx.commit()
            .map_err(|e| VotingError::from_sqlite(context, &e))?;
        Ok(value)
    }

    /// Set the wallet identifier used to scope all subsequent operations.
    pub fn set_wallet_id(&self, id: &str) {
        *self.wallet_id.lock().expect("wallet_id mutex poisoned") = id.to_string();
    }

    /// Get the current wallet identifier. Panics if not set.
    pub fn wallet_id(&self) -> String {
        let id = self
            .wallet_id
            .lock()
            .expect("wallet_id mutex poisoned")
            .clone();
        assert!(
            !id.is_empty(),
            "wallet_id must be set before performing voting operations"
        );
        id
    }

    /// Get a lock on the underlying connection for query execution.
    pub fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.inner.conn.lock().expect("database mutex poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::VotingRoundParams;

    const W: &str = "test-wallet";

    fn test_db() -> VotingDb {
        VotingDb::open(":memory:").unwrap()
    }

    fn test_params() -> VotingRoundParams {
        VotingRoundParams {
            vote_round_id: "test-round-1".to_string(),
            snapshot_height: 1000,
            ea_pk: vec![0xEA; 32],
            nc_root: vec![0xAA; 32],
            nullifier_imt_root: vec![0xBB; 32],
        }
    }

    #[test]
    fn test_open_in_memory() {
        let db = test_db();
        let conn = db.conn();
        let version: u32 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 19);
    }

    #[test]
    fn writes_wait_for_a_transient_external_writer() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "zcash-voting-busy-timeout-{}-{unique}.sqlite",
            std::process::id()
        ));
        let path_string = path.to_string_lossy().into_owned();
        let db = VotingDb::open(&path_string).unwrap();
        db.conn()
            .execute_batch("CREATE TABLE busy_timeout_probe (value INTEGER NOT NULL)")
            .unwrap();

        let lock = Connection::open(&path).unwrap();
        lock.busy_timeout(SQLITE_BUSY_TIMEOUT).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            lock.execute_batch("ROLLBACK").unwrap();
        });

        let started = std::time::Instant::now();
        db.conn()
            .execute("INSERT INTO busy_timeout_probe (value) VALUES (1)", [])
            .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(300));

        release.join().unwrap();
        drop(db);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_string}-shm"));
        let _ = std::fs::remove_file(format!("{path_string}-wal"));
    }

    #[test]
    fn scoped_handles_share_one_connection_and_keep_their_own_wallet_id() {
        let db = test_db();
        db.set_wallet_id("wallet-a");
        let other = db.scoped("wallet-b");
        assert!(db.shares_connection_with(&other));
        assert_eq!(db.wallet_id(), "wallet-a");
        assert_eq!(other.wallet_id(), "wallet-b");

        db.conn()
            .execute_batch("CREATE TABLE scoped_probe (value INTEGER NOT NULL)")
            .unwrap();
        other
            .conn()
            .execute("INSERT INTO scoped_probe (value) VALUES (7)", [])
            .unwrap();
        let value: i64 = db
            .conn()
            .query_row("SELECT value FROM scoped_probe", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, 7);
    }

    #[test]
    fn write_transaction_reports_db_busy_past_the_busy_timeout() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "zcash-voting-db-busy-{}-{unique}.sqlite",
            std::process::id()
        ));
        let path_string = path.to_string_lossy().into_owned();
        let db = VotingDb::open(&path_string).unwrap();
        db.conn()
            .execute_batch("CREATE TABLE busy_probe (value INTEGER NOT NULL)")
            .unwrap();
        db.conn().busy_timeout(Duration::from_millis(100)).unwrap();

        let lock = Connection::open(&path).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();

        let error = db
            .write_transaction("busy probe", |tx| {
                tx.execute("INSERT INTO busy_probe (value) VALUES (1)", [])
                    .map_err(|e| VotingError::from_sqlite("insert", &e))?;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.kind(), crate::VotingErrorKind::DbBusy, "{error}");
        assert!(error.retryable());
        assert!(error.to_string().contains("busy probe"), "{error}");

        lock.execute_batch("ROLLBACK").unwrap();
        db.write_transaction("busy probe", |tx| {
            tx.execute("INSERT INTO busy_probe (value) VALUES (1)", [])
                .map_err(|e| VotingError::from_sqlite("insert", &e))?;
            Ok(())
        })
        .unwrap();

        drop(lock);
        drop(db);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_string}-shm"));
        let _ = std::fs::remove_file(format!("{path_string}-wal"));
    }

    #[test]
    fn test_round_lifecycle() {
        let db = test_db();
        let conn = db.conn();
        let params = test_params();

        queries::insert_round(&conn, W, Network::Testnet, &params, None).unwrap();

        let state = queries::get_round_state(&conn, "test-round-1", W).unwrap();
        assert_eq!(state.phase, RoundPhase::Initialized);
        assert_eq!(state.network, Network::Testnet);
        assert_eq!(state.snapshot_height, 1000);
        assert!(!state.proof_generated);

        let rounds = queries::list_rounds(&conn, W).unwrap();
        assert_eq!(rounds.len(), 1);
        assert_eq!(rounds[0].round_id, "test-round-1");
        assert_eq!(rounds[0].network, Network::Testnet);

        queries::clear_round(&conn, "test-round-1", W).unwrap();
        let rounds = queries::list_rounds(&conn, W).unwrap();
        assert!(rounds.is_empty());
    }

    #[test]
    fn test_tree_state_cache() {
        let db = test_db();
        let conn = db.conn();
        queries::insert_round(&conn, W, Network::Testnet, &test_params(), None).unwrap();

        let tree_state = vec![0xCC; 1024];
        queries::store_tree_state(&conn, "test-round-1", W, 1000, &tree_state).unwrap();

        let loaded = queries::load_tree_state(&conn, "test-round-1", W).unwrap();
        assert_eq!(loaded, tree_state);
    }

    #[test]
    fn test_proof_storage() {
        let db = test_db();
        let conn = db.conn();
        queries::insert_round(&conn, W, Network::Testnet, &test_params(), None).unwrap();
        queries::insert_bundle(&conn, "test-round-1", W, 0, &[]).unwrap();
        queries::store_proof(&conn, "test-round-1", W, 0, &vec![0xAB; 256]).unwrap();

        let state = queries::get_round_state(&conn, "test-round-1", W).unwrap();
        assert!(!state.proof_generated, "proof alone should not be enough");

        queries::store_van_position(&conn, "test-round-1", W, 0, 42).unwrap();
        let state = queries::get_round_state(&conn, "test-round-1", W).unwrap();
        assert!(
            state.proof_generated,
            "proof + VAN position should be enough"
        );
    }

    #[test]
    fn test_vote_storage() {
        let db = test_db();
        let conn = db.conn();
        queries::insert_round(&conn, W, Network::Testnet, &test_params(), None).unwrap();
        queries::insert_bundle(&conn, "test-round-1", W, 0, &[]).unwrap();

        let commitment = vec![0xCC; 128];
        queries::store_vote(&conn, "test-round-1", W, 0, 0, 0, &commitment).unwrap();
        queries::store_vote(&conn, "test-round-1", W, 0, 1, 1, &commitment).unwrap();

        queries::record_vote_submission(&conn, "test-round-1", W, 0, 0, "vote-tx").unwrap();
        queries::record_vote_submission(&conn, "test-round-1", W, 0, 0, "vote-tx").unwrap();
        queries::store_vote(&conn, "test-round-1", W, 0, 0, 0, &commitment).unwrap();
        let replace_err =
            queries::store_vote(&conn, "test-round-1", W, 0, 0, 1, &commitment).unwrap_err();
        assert!(
            replace_err
                .to_string()
                .contains("cannot replace submitted vote"),
            "{replace_err}"
        );
        assert_eq!(
            queries::get_vote_tx_hash(&conn, "test-round-1", W, 0, 0).unwrap(),
            Some("vote-tx".to_string())
        );

        let err = queries::record_vote_submission(&conn, "test-round-1", W, 0, 99, "vote-tx")
            .unwrap_err();
        assert!(matches!(err, VotingError::InvalidInput { .. }));
    }

    #[test]
    fn test_get_votes() {
        let db = test_db();
        let conn = db.conn();
        queries::insert_round(&conn, W, Network::Testnet, &test_params(), None).unwrap();
        queries::insert_bundle(&conn, "test-round-1", W, 0, &[]).unwrap();

        let votes = queries::get_votes(&conn, "test-round-1", W).unwrap();
        assert!(votes.is_empty());

        let commitment = vec![0xCC; 128];
        queries::store_vote(&conn, "test-round-1", W, 0, 0, 0, &commitment).unwrap();
        queries::store_vote(&conn, "test-round-1", W, 0, 1, 2, &commitment).unwrap();

        let votes = queries::get_votes(&conn, "test-round-1", W).unwrap();
        assert_eq!(votes.len(), 2);
        assert_eq!(votes[0].proposal_id, 0);
        assert_eq!(votes[0].choice, 0);
        assert_eq!(votes[1].proposal_id, 1);
        assert_eq!(votes[1].choice, 2);

        queries::record_vote_submission(&conn, "test-round-1", W, 0, 0, "vote-tx").unwrap();
        let votes = queries::get_votes(&conn, "test-round-1", W).unwrap();
        assert_eq!(
            queries::get_vote_tx_hash(&conn, "test-round-1", W, 0, 0).unwrap(),
            Some("vote-tx".to_string())
        );
        assert_eq!(votes.len(), 2);
    }

    #[test]
    fn test_wallet_isolation() {
        let db = test_db();
        let conn = db.conn();
        let params = test_params();

        queries::insert_round(&conn, "wallet-a", Network::Testnet, &params, None).unwrap();
        queries::insert_round(&conn, "wallet-b", Network::Testnet, &params, None).unwrap();

        queries::insert_bundle(&conn, "test-round-1", "wallet-a", 0, &[]).unwrap();
        queries::insert_bundle(&conn, "test-round-1", "wallet-b", 0, &[]).unwrap();

        let commitment = vec![0xCC; 128];
        queries::store_vote(&conn, "test-round-1", "wallet-a", 0, 0, 1, &commitment).unwrap();
        queries::store_vote(&conn, "test-round-1", "wallet-b", 0, 0, 2, &commitment).unwrap();

        let votes_a = queries::get_votes(&conn, "test-round-1", "wallet-a").unwrap();
        let votes_b = queries::get_votes(&conn, "test-round-1", "wallet-b").unwrap();
        assert_eq!(votes_a.len(), 1);
        assert_eq!(votes_b.len(), 1);
        assert_eq!(votes_a[0].choice, 1);
        assert_eq!(votes_b[0].choice, 2);

        queries::clear_round(&conn, "test-round-1", "wallet-a").unwrap();
        let rounds_b = queries::list_rounds(&conn, "wallet-b").unwrap();
        assert_eq!(
            rounds_b.len(),
            1,
            "wallet-b round should survive wallet-a clear"
        );
    }
}
