mod database_authority;
mod migrations;
pub mod operations;
pub mod queries;

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use rusqlite::{Connection, OpenFlags};

use crate::types::{Network, VotingError};

use self::database_authority::DatabaseAuthority;

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

/// Database handle for voting state. Wraps a SQLite connection and a
/// wallet identifier that scopes all round data to a single wallet.
pub struct VotingDb {
    conn: Mutex<Connection>,
    wallet_id: Mutex<String>,
    database_authority: Arc<DatabaseAuthority>,
}

impl VotingDb {
    /// Opens or creates a voting database at a UTF-8 filesystem path.
    ///
    /// Empty paths, SQLite's `:memory:` magic name, and `file:` URIs are
    /// rejected. Use [`VotingDb::open_path`] for native filesystem paths and
    /// [`VotingDb::open_in_memory`] for an independent in-memory database.
    pub fn open(path: &str) -> Result<Self, VotingError> {
        Self::open_path(Path::new(path))
    }

    /// Opens or creates a voting database at a native filesystem path.
    ///
    /// URI interpretation is disabled so database identity is determined only
    /// by the filesystem path. Handles resolving to the same canonical path
    /// share lifecycle coordination.
    pub fn open_path(path: &Path) -> Result<Self, VotingError> {
        validate_database_path(path)?;
        let opening_path = canonical_database_path(path)?;
        let connection = open_canonical_database_file(&opening_path)?;
        // A first open creates the final path. Resolve it again so concurrent
        // casing aliases on a case-insensitive filesystem converge before
        // authority interning.
        let canonical_path = canonical_opened_database_path(&opening_path)?;
        let database_authority = DatabaseAuthority::for_file(canonical_path)?;

        Self::initialize(connection, database_authority)
    }

    /// Opens a fresh private in-memory voting database.
    pub fn open_in_memory() -> Result<Self, VotingError> {
        let connection = Connection::open_in_memory().map_err(|error| VotingError::Internal {
            message: format!("failed to open in-memory database: {error}"),
        })?;
        Self::initialize(connection, DatabaseAuthority::private())
    }

    /// Configures and migrates one newly opened voting database.
    fn initialize(
        mut connection: Connection,
        database_authority: Arc<DatabaseAuthority>,
    ) -> Result<Self, VotingError> {
        connection
            .busy_timeout(SQLITE_BUSY_TIMEOUT)
            .map_err(|error| VotingError::Internal {
                message: format!("failed to configure database busy timeout: {error}"),
            })?;

        connection
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|error| VotingError::Internal {
                message: format!("failed to set pragmas: {error}"),
            })?;

        migrations::migrate(&mut connection)?;

        Ok(Self {
            conn: Mutex::new(connection),
            wallet_id: Mutex::new(String::new()),
            database_authority,
        })
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
        self.conn.lock().expect("database mutex poisoned")
    }

    /// Returns lifecycle coordination shared by every handle to this database.
    pub(crate) fn chain_submission_coordination(
        &self,
    ) -> &crate::chain_submission::coordination::SubmissionCoordination {
        self.database_authority.chain_submission()
    }
}

/// Opens a resolved database path without following a later symlink change.
fn open_canonical_database_file(canonical_path: &Path) -> Result<Connection, VotingError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    Connection::open_with_flags(canonical_path, flags).map_err(|error| VotingError::Internal {
        message: format!("failed to open database: {error}"),
    })
}

/// Rejects SQLite's non-filesystem magic names before any database is opened.
fn validate_database_path(path: &Path) -> Result<(), VotingError> {
    let invalid_path = path.as_os_str().is_empty()
        || path == Path::new(":memory:")
        || path.to_str().is_some_and(|path| path.starts_with("file:"));
    if invalid_path {
        return Err(VotingError::InvalidInput {
            message: "voting database must be a filesystem path; use open_in_memory for an independent in-memory database".to_string(),
        });
    }
    Ok(())
}

/// Resolves the exact filesystem path before SQLite opens it.
///
/// Existing symlinks resolve to their current target. For a new database, the
/// parent is resolved first and the new filename is appended, so SQLite and the
/// authority registry receive the same path.
fn canonical_database_path(path: &Path) -> Result<std::path::PathBuf, VotingError> {
    canonical_database_path_with_symlink_limit(path, 40)
}

/// Resolves symlink chains whose final database file does not exist yet.
fn canonical_database_path_with_symlink_limit(
    path: &Path,
    remaining_symlinks: usize,
) -> Result<std::path::PathBuf, VotingError> {
    match std::fs::canonicalize(path) {
        Ok(canonical_path) => Ok(canonical_path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    if remaining_symlinks == 0 {
                        return Err(VotingError::InvalidInput {
                            message: "voting database path has too many symbolic links".to_string(),
                        });
                    }
                    let symlink_target =
                        std::fs::read_link(path).map_err(|error| VotingError::Storage {
                            message: format!("failed to read SQLite database symlink: {error}"),
                        })?;
                    let target_path = if symlink_target.is_absolute() {
                        symlink_target
                    } else {
                        path.parent()
                            .filter(|parent| !parent.as_os_str().is_empty())
                            .unwrap_or_else(|| Path::new("."))
                            .join(symlink_target)
                    };
                    canonical_database_path_with_symlink_limit(&target_path, remaining_symlinks - 1)
                }
                Ok(_) => Err(VotingError::Storage {
                    message: "SQLite database path disappeared while resolving it".to_string(),
                }),
                Err(metadata_error) if metadata_error.kind() == std::io::ErrorKind::NotFound => {
                    canonical_new_database_path(path)
                }
                Err(metadata_error) => Err(VotingError::Storage {
                    message: format!("failed to inspect SQLite database path: {metadata_error}"),
                }),
            }
        }
        Err(error) => Err(VotingError::Storage {
            message: format!("failed to resolve SQLite database path: {error}"),
        }),
    }
}

/// Resolves the parent of a database file that has not been created yet.
fn canonical_new_database_path(path: &Path) -> Result<std::path::PathBuf, VotingError> {
    let file_name = path.file_name().ok_or_else(|| VotingError::InvalidInput {
        message: "voting database path must name a file".to_string(),
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let canonical_parent = std::fs::canonicalize(parent).map_err(|error| VotingError::Storage {
        message: format!("failed to resolve SQLite database parent: {error}"),
    })?;
    Ok(canonical_parent.join(file_name))
}

/// Resolves the filesystem identity of a path SQLite has created or opened.
fn canonical_opened_database_path(path: &Path) -> Result<std::path::PathBuf, VotingError> {
    std::fs::canonicalize(path).map_err(|error| VotingError::Storage {
        message: format!("failed to resolve opened SQLite database path: {error}"),
    })
}

#[cfg(test)]
mod tests {
    mod database_authority;

    use super::*;
    use crate::types::VotingRoundParams;

    const W: &str = "test-wallet";

    fn test_db() -> VotingDb {
        VotingDb::open_in_memory().unwrap()
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
        assert_eq!(version, 18);
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
