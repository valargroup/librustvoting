mod migrations;
pub mod operations;
pub mod queries;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
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
    /// Identity of the sidecar the connection is open on. Every connection
    /// to one file gets the same id within the process, so proof
    /// single-flighting, round locks, and tree caches keyed by it coordinate
    /// across separately opened handles; each in-memory database gets its
    /// own. Two sidecars that use the same wallet id stay distinct.
    id: u64,
    /// The open span this connection belongs to; see [`OpenSidecar`].
    epoch: u64,
    conn: Mutex<Connection>,
    /// Chain-submission coordination for the sidecar's open span, shared by
    /// every connection to the file so in-flight and identity locks hold
    /// across separately opened handles operating on the same durable rows.
    chain_submission_coordination:
        Arc<crate::chain_submission::coordination::SubmissionCoordination>,
}

static NEXT_SIDECAR_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Sidecar ids already assigned to file paths in this process.
static SIDECAR_IDS: LazyLock<Mutex<HashMap<PathBuf, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// One open span of a sidecar: the connections currently open on it and
/// the epoch that span was given. A sidecar whose last connection closes and
/// that is opened again starts a new epoch, so process-local caches that
/// captured the old epoch see it as closed even though the path-derived id
/// is the same; the file may have been replaced in between.
struct OpenSidecar {
    connections: usize,
    epoch: u64,
    /// The chain-submission coordination authority for this open span. It is
    /// keyed by the sidecar, not the connection: two `VotingDb::open` calls on
    /// one file operate on the same durable submission rows, so they must
    /// share in-flight and identity locks or a second caller could mistake
    /// the first caller's live reservation for an abandoned one.
    chain_submission_coordination:
        Arc<crate::chain_submission::coordination::SubmissionCoordination>,
}

/// Open sidecars by id, so process-local caches keyed by the id can tell
/// when a sidecar has no handle left through any connection.
static OPEN_SIDECARS: LazyLock<Mutex<HashMap<u64, OpenSidecar>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_SIDECAR_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Records one more connection to sidecar `id` and returns the epoch of the
/// open span it belongs to together with that span's shared chain-submission
/// coordination.
fn note_sidecar_opened(
    id: u64,
) -> (
    u64,
    Arc<crate::chain_submission::coordination::SubmissionCoordination>,
) {
    let mut open = OPEN_SIDECARS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let span = open.entry(id).or_insert_with(|| OpenSidecar {
        connections: 0,
        epoch: NEXT_SIDECAR_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        chain_submission_coordination: Arc::default(),
    });
    span.connections += 1;
    (span.epoch, Arc::clone(&span.chain_submission_coordination))
}

impl Drop for SidecarConnection {
    fn drop(&mut self) {
        let mut open = OPEN_SIDECARS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(span) = open.get_mut(&self.id) {
            span.connections -= 1;
            if span.connections == 0 {
                open.remove(&self.id);
            }
        }
    }
}

/// Whether the sidecar with `id` is still open in the open span `epoch`,
/// through any handle. False once its last connection closed, even if the
/// same path has since been opened again.
pub(crate) fn sidecar_is_open_in_epoch(id: u64, epoch: u64) -> bool {
    OPEN_SIDECARS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&id)
        .is_some_and(|span| span.epoch == epoch)
}

/// The sidecar id for `path`: the id assigned to its canonical path, or a
/// fresh one for an in-memory database.
fn sidecar_id_for(path: &str) -> u64 {
    let fresh = || NEXT_SIDECAR_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if path == ":memory:" {
        return fresh();
    }
    let key = sidecar_registry_key(Path::new(path));
    *SIDECAR_IDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(key)
        .or_insert_with(fresh)
}

/// The canonical identity of a sidecar path, so every spelling of one file
/// maps to one key. An existing file is canonicalized in full, so a symlink
/// to the sidecar and its real path share an identity. A file not created
/// yet has its parent directory canonicalized (symlinks and `..` resolved)
/// with the file name appended; a parent that cannot be canonicalized keeps
/// the path as given.
pub(crate) fn sidecar_registry_key(sidecar_path: &Path) -> PathBuf {
    if let Ok(existing) = sidecar_path.canonicalize() {
        return existing;
    }
    match (sidecar_path.parent(), sidecar_path.file_name()) {
        (Some(parent), Some(file_name)) => {
            let parent = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            parent
                .canonicalize()
                .map(|parent| parent.join(file_name))
                .unwrap_or_else(|_| sidecar_path.to_path_buf())
        }
        _ => sidecar_path.to_path_buf(),
    }
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
        Ok(Self::from_connection(Self::open_connection(path)?, path))
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

    pub(crate) fn from_connection(conn: Connection, path: &str) -> Self {
        let id = sidecar_id_for(path);
        let (epoch, chain_submission_coordination) = note_sidecar_opened(id);
        Self {
            inner: Arc::new(SidecarConnection {
                id,
                epoch,
                conn: Mutex::new(conn),
                chain_submission_coordination,
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
    /// instead of opening a connection per account. An empty `wallet_id` is
    /// refused with [`VotingError::InvalidInput`]: a handle scoped to no
    /// wallet would fail on its first wallet-scoped operation.
    pub fn scoped(&self, wallet_id: &str) -> Result<Self, VotingError> {
        if wallet_id.is_empty() {
            return Err(VotingError::InvalidInput {
                message: "wallet id must not be empty".to_string(),
            });
        }
        Ok(Self::from_shared(Arc::clone(&self.inner), wallet_id))
    }

    /// Identity of the underlying sidecar: shared by every connection to one
    /// file within the process, unique per in-memory database.
    pub(crate) fn sidecar_id(&self) -> u64 {
        self.inner.id
    }

    /// The open span of the underlying sidecar; see
    /// [`sidecar_is_open_in_epoch`].
    pub(crate) fn sidecar_epoch(&self) -> u64 {
        self.inner.epoch
    }

    /// Whether two handles share one underlying connection.
    pub fn shares_connection_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Whether two handles share one chain-submission coordination authority.
    /// True for every handle on one sidecar file within one open span, even
    /// across separately opened connections.
    #[cfg(test)]
    pub(crate) fn shares_chain_coordination_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(
            &self.inner.chain_submission_coordination,
            &other.inner.chain_submission_coordination,
        )
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

    /// Runs `body` inside one deferred read transaction and rolls it back.
    ///
    /// The connection mutex is held for the whole call, so no other handle
    /// in this process can interleave a write between two of `body`'s reads,
    /// and the transaction pins one WAL read snapshot against writers in
    /// other processes. `body` receives only the transaction, so it cannot
    /// re-enter the (non-reentrant) connection mutex, and it must not write:
    /// the transaction ends with a rollback whatever `body` did.
    pub(crate) fn read_transaction<T>(
        &self,
        context: &str,
        body: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T, VotingError>,
    ) -> Result<T, VotingError> {
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(|e| VotingError::from_sqlite(context, &e))?;
        let value = body(&tx)?;
        tx.rollback()
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
    mod tree_confirmed_completion;

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
        assert_eq!(version, 22);
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
        let other = db.scoped("wallet-b").unwrap();
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
