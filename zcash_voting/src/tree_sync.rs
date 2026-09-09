//! Vote commitment tree sync and VAN witness generation.
//!
//! Manages per-round in-memory `TreeClient` instances that sync incrementally
//! from a chain node via HTTP, then generates Merkle authentication paths
//! (witnesses) for Vote Authority Notes (VANs) needed by ZKP #2.

#[allow(unused_imports)]
pub(crate) use crate::backend::pasta_curves;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, RwLock};

use vote_commitment_tree::{MerklePath, TreeClient, TreeSyncApi};
use vote_commitment_tree_client::http_sync_api::HttpTreeSyncApi;

use crate::storage::{queries, VotingDb};
use crate::types::VotingError;
use crate::vote::{VanWitness, VAN_AUTH_PATH_LEN};
use crate::HyperTransport;

impl From<(MerklePath, u32)> for VanWitness {
    fn from((path, anchor_height): (MerklePath, u32)) -> Self {
        let auth_path = path
            .auth_path()
            .iter()
            .take(VAN_AUTH_PATH_LEN)
            .map(|hash| hash.to_bytes().to_vec())
            .collect();
        Self {
            auth_path,
            position: path.position(),
            anchor_height,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pasta_curves::group::ff::PrimeField;
    use pasta_curves::Fp;
    use std::sync::mpsc;
    use std::time::Duration;
    use vote_commitment_tree::MemoryTreeServer;

    use crate::{governance::BALLOT_DIVISOR, round::RoundParams, types::NoteInfo};

    pub(super) const ROUND_ID: &str =
        "0101010101010101010101010101010101010101010101010101010101010101";
    const SECOND_ROUND_ID: &str =
        "0202020202020202020202020202020202020202020202020202020202020202";
    pub(super) const WALLET_ID: &str = "wallet-tree-sync";

    #[test]
    fn sync_rebuilds_when_recovery_marks_already_synced_position() {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(WALLET_ID);
        db.create_round(crate::Network::Testnet, &round_params(), None)
            .unwrap();
        let notes = (0..6).map(note).collect::<Vec<_>>();
        db.ensure_bundles(ROUND_ID, &notes).unwrap();
        db.store_van_position(ROUND_ID, 0, 0).unwrap();
        db.store_van_position(ROUND_ID, 1, 1).unwrap();
        db.conn()
            .execute(
                "UPDATE bundles
                 SET gov_comm = CASE bundle_index WHEN 0 THEN ?1 ELSE ?2 END
                 WHERE round_id = ?3 AND wallet_id = ?4",
                rusqlite::params![
                    Fp::from(1).to_repr().as_slice(),
                    Fp::from(2).to_repr().as_slice(),
                    ROUND_ID,
                    WALLET_ID,
                ],
            )
            .unwrap();

        let sync = VoteTreeSync::new();
        let server = server_with_single_leaf_blocks(7);

        let height = sync.sync_with_api(&db, ROUND_ID, &server).unwrap();
        assert_eq!(height, 7);

        // A resumed wallet may confirm earlier cast-vote transactions after a
        // tree sync already passed their new VAN leaves. Those positions must
        // still be retained for later vote witnesses.
        let conn = db.conn();
        for (bundle_index, proposal_id) in [(0, 1), (1, 2)] {
            crate::storage::queries::store_vote(
                &conn,
                ROUND_ID,
                WALLET_ID,
                bundle_index,
                proposal_id,
                0,
                &[0xAA; 32],
            )
            .unwrap();
            crate::storage::queries::record_vote_submission(
                &conn,
                ROUND_ID,
                WALLET_ID,
                bundle_index,
                proposal_id,
                "confirmed-vote",
            )
            .unwrap();
        }
        drop(conn);
        db.store_van_position(ROUND_ID, 0, 2).unwrap();
        db.store_van_position(ROUND_ID, 1, 4).unwrap();

        let height = sync.sync_with_api(&db, ROUND_ID, &server).unwrap();
        let witness = sync.generate_van_witness(&db, ROUND_ID, 1, height).unwrap();

        assert_eq!(height, 7);
        assert_eq!(witness.position, 4);
        assert_eq!(witness.anchor_height, 7);
    }

    #[test]
    fn sync_rejects_a_confirmed_position_for_a_different_van() {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(WALLET_ID);
        db.create_round(crate::Network::Testnet, &round_params(), None)
            .unwrap();
        db.ensure_bundles(ROUND_ID, &[note(0)]).unwrap();
        db.store_van_position(ROUND_ID, 0, 0).unwrap();
        db.conn()
            .execute(
                "UPDATE bundles SET gov_comm = ?1
                 WHERE round_id = ?2 AND wallet_id = ?3 AND bundle_index = 0",
                rusqlite::params![Fp::from(9).to_repr().as_slice(), ROUND_ID, WALLET_ID],
            )
            .unwrap();

        let sync = VoteTreeSync::new();
        let error = sync
            .sync_with_api(&db, ROUND_ID, &server_with_single_leaf_blocks(1))
            .expect_err("a different public leaf must not authorize voting");
        assert!(
            error
                .to_string()
                .contains("does not match its synced vote-tree leaf"),
            "{error}"
        );
        let witness_error = sync
            .generate_van_witness(&db, ROUND_ID, 0, 1)
            .expect_err("unverified tree state must not produce a witness");
        assert!(
            witness_error
                .to_string()
                .contains("failed to generate witness"),
            "{witness_error}"
        );
    }

    #[test]
    fn sync_retains_incremental_state_when_confirmed_position_is_not_yet_synced() {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(WALLET_ID);
        db.create_round(crate::Network::Testnet, &round_params(), None)
            .unwrap();
        db.ensure_bundles(ROUND_ID, &[note(0)]).unwrap();
        db.store_van_position(ROUND_ID, 0, 1).unwrap();
        db.conn()
            .execute(
                "UPDATE bundles SET gov_comm = ?1
                 WHERE round_id = ?2 AND wallet_id = ?3 AND bundle_index = 0",
                rusqlite::params![Fp::from(2).to_repr().as_slice(), ROUND_ID, WALLET_ID],
            )
            .unwrap();

        let sync = VoteTreeSync::new();
        let mut server = server_with_single_leaf_blocks(1);
        let error = sync
            .sync_with_api(&db, ROUND_ID, &server)
            .expect_err("a position beyond the synced tree must remain pending");
        assert!(
            error
                .to_string()
                .contains("is absent from the synced vote tree"),
            "{error}"
        );

        let round_client = sync.clients.lock().unwrap().get(ROUND_ID).cloned().unwrap();
        assert_eq!(round_client.lock().unwrap().client.size(), 1);
        assert!(sync.generate_van_witness(&db, ROUND_ID, 0, 1).is_err());

        server.append(Fp::from(2)).unwrap();
        server.checkpoint(2).unwrap();
        let height = sync.sync_with_api(&db, ROUND_ID, &server).unwrap();
        let witness = sync.generate_van_witness(&db, ROUND_ID, 0, height).unwrap();
        assert_eq!(height, 2);
        assert_eq!(witness.position, 1);
    }

    #[test]
    fn blocked_sync_does_not_block_another_round() {
        struct BlockingApi {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        }

        impl TreeSyncApi for BlockingApi {
            type Error = std::convert::Infallible;

            fn get_block_commitments(
                &self,
                _from_height: u32,
                _to_height: u32,
            ) -> Result<vote_commitment_tree::sync_api::BlockCommitmentsPage, Self::Error>
            {
                unreachable!("empty tree does not fetch commitment pages")
            }

            fn get_root_at_height(&self, _height: u32) -> Result<Option<Fp>, Self::Error> {
                Ok(None)
            }

            fn get_tree_state(
                &self,
            ) -> Result<vote_commitment_tree::sync_api::TreeState, Self::Error> {
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
                Ok(vote_commitment_tree::sync_api::TreeState {
                    next_index: 0,
                    root: Fp::zero(),
                    height: 0,
                })
            }
        }

        let sync = Arc::new(VoteTreeSync::new());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let first_sync = sync.clone();
        let first = std::thread::spawn(move || {
            let db = db_for_round(ROUND_ID);
            first_sync
                .sync_with_api(
                    &db,
                    ROUND_ID,
                    &BlockingApi {
                        entered: entered_tx,
                        release: release_rx,
                    },
                )
                .unwrap()
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let second_sync = sync.clone();
        let (second_done_tx, second_done_rx) = mpsc::channel();
        let second = std::thread::spawn(move || {
            let db = db_for_round(SECOND_ROUND_ID);
            let result =
                second_sync.sync_with_api(&db, SECOND_ROUND_ID, &MemoryTreeServer::empty());
            second_done_tx.send(result).unwrap();
        });

        let second_result = second_done_rx.recv_timeout(Duration::from_secs(2));
        release_tx.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();

        assert_eq!(second_result.unwrap().unwrap(), 0);
    }

    fn db_for_round(round_id: &str) -> VotingDb {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(WALLET_ID);
        let mut params = round_params();
        params.vote_round_id = round_id.to_string();
        db.create_round(crate::Network::Testnet, &params, None)
            .unwrap();
        db
    }

    pub(super) fn server_with_single_leaf_blocks(count: u32) -> MemoryTreeServer {
        let mut server = MemoryTreeServer::empty();
        for index in 0..count {
            server.append(Fp::from(u64::from(index + 1))).unwrap();
            server.checkpoint(index + 1).unwrap();
        }
        server
    }

    pub(super) fn round_params() -> RoundParams {
        RoundParams {
            vote_round_id: ROUND_ID.to_string(),
            snapshot_height: 100,
            ea_pk: vec![1; 32],
            nc_root: vec![2; 32],
            nullifier_imt_root: vec![3; 32],
        }
    }

    pub(super) fn note(position: u64) -> NoteInfo {
        NoteInfo {
            commitment: vec![1; 32],
            nullifier: {
                let mut nf = vec![2; 32];
                nf[0] = position as u8;
                nf
            },
            value: BALLOT_DIVISOR + 500_000,
            position,
            diversifier: vec![3; 11],
            rho: vec![4; 32],
            rseed: vec![5; 32],
            scope: 0,
            ufvk_str: "uviewtest".to_string(),
        }
    }
}

struct RoundTreeClient {
    client: TreeClient,
    marked_positions: BTreeSet<u64>,
}

impl RoundTreeClient {
    fn empty() -> Self {
        Self {
            client: TreeClient::empty(),
            marked_positions: BTreeSet::new(),
        }
    }

    fn needs_resync_for(&self, positions: &BTreeSet<u64>) -> bool {
        positions
            .iter()
            .any(|pos| !self.marked_positions.contains(pos) && *pos < self.client.size())
    }

    fn mark_positions(&mut self, positions: &BTreeSet<u64>) {
        for pos in positions {
            self.client.mark_position(*pos);
        }
        self.marked_positions.extend(positions.iter().copied());
    }
}

/// What a [`VoteTreeSync`] holds for one round, as seen without waiting on
/// the round's lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CachedRoundState {
    /// No in-memory tree for the round.
    Absent,
    /// The round's tree exists but is locked by a sync or witness in flight.
    Syncing,
    /// The height the round's tree is synced to; zero until a first sync
    /// completes.
    SyncedTo(u32),
}

/// Manages per-round in-memory vote commitment trees for VAN witness generation.
///
/// The map lock is held only while locating a round. Each round has a separate
/// lock, so a remote sync cannot block unrelated rounds for the same wallet.
pub struct VoteTreeSync {
    clients: Mutex<HashMap<String, Arc<Mutex<RoundTreeClient>>>>,
    operations: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    reset_gate: RwLock<()>,
    transport: Arc<dyn vote_commitment_tree_client::transport::Transport>,
}

impl VoteTreeSync {
    /// Creates a tree sync over the SDK's direct HTTP transport.
    pub fn new() -> Self {
        Self::with_transport(Arc::new(HyperTransport::new()))
    }

    /// Creates a tree sync over a host-supplied transport.
    pub fn with_transport(
        transport: Arc<dyn vote_commitment_tree_client::transport::Transport>,
    ) -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            operations: Mutex::new(HashMap::new()),
            reset_gate: RwLock::new(()),
            transport,
        }
    }

    /// Runs one complete tree operation without allowing sync/reset interleaving.
    /// Registry locks are released before waiting on a round or doing network I/O.
    fn coordinated<R>(
        &self,
        round_id: &str,
        operation: impl FnOnce() -> Result<R, VotingError>,
    ) -> Result<R, VotingError> {
        let _reset_gate = self.reset_gate.read().map_err(|_| VotingError::Internal {
            message: "tree reset gate poisoned".into(),
        })?;
        let lock = self
            .operations
            .lock()
            .map_err(|_| VotingError::Internal {
                message: "tree operation registry poisoned".into(),
            })?
            .entry(round_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _operation = lock.lock().map_err(|_| VotingError::Internal {
            message: "tree operation lock poisoned".into(),
        })?;
        operation()
    }

    /// Synchronizes with ordered failover, cleans failed attempts, and captures
    /// the witness under one operation guard on this bound tree client.
    pub(crate) fn sync_and_witness(
        &self,
        db: &VotingDb,
        round_id: &str,
        bundle_index: u32,
        node_urls: &[String],
    ) -> Result<VanWitness, VotingError> {
        self.coordinated(round_id, || {
            let mut failure = None;
            for node_url in node_urls {
                crate::proving_runtime::check_interruption()?;
                let api = HttpTreeSyncApi::new(node_url, round_id, self.transport.clone());
                match self.sync_locked(db, round_id, &api) {
                    Ok(height) => return self.witness_locked(db, round_id, bundle_index, height),
                    Err(error) => {
                        self.reset_locked(round_id)?;
                        failure = Some(error);
                    }
                }
            }
            Err(failure.unwrap_or_else(|| VotingError::InvalidInput {
                message: "vote tree sync requires a node URL".into(),
            }))
        })
    }

    /// Sync the vote commitment tree for a specific round from a chain node.
    ///
    /// Creates a per-round `TreeClient` on first call, then syncs incrementally
    /// on subsequent calls. VAN positions from ALL bundles are automatically
    /// marked for witness generation before syncing. If recovery records a new
    /// VAN position that is already behind the synced tip, the round client is
    /// rebuilt so the sparse tree retains that historical leaf.
    /// Before a bundle's first vote, sync also requires its confirmed event
    /// position to contain the stored delegation VAN. Capability import
    /// recomputes that VAN from the customer's own public hotkey target. A
    /// VAN mismatch or inconsistent tree state invalidates the round client so
    /// witness generation cannot use unverified data. A confirmed position that
    /// has not reached the synced tree yet is reported without discarding the
    /// incremental client, allowing a later sync to resume normally.
    ///
    /// Returns the latest synced block height.
    pub fn sync(&self, db: &VotingDb, round_id: &str, node_url: &str) -> Result<u32, VotingError> {
        let api = HttpTreeSyncApi::new(node_url, round_id, self.transport.clone());
        self.sync_with_api(db, round_id, &api)
    }

    pub(crate) fn sync_with_api<A>(
        &self,
        db: &VotingDb,
        round_id: &str,
        api: &A,
    ) -> Result<u32, VotingError>
    where
        A: TreeSyncApi,
    {
        self.coordinated(round_id, || self.sync_locked(db, round_id, api))
    }

    fn sync_locked<A: TreeSyncApi>(
        &self,
        db: &VotingDb,
        round_id: &str,
        api: &A,
    ) -> Result<u32, VotingError> {
        let wallet_id = db.wallet_id();
        let entries = queries::load_van_tree_entries(&db.conn(), round_id, &wallet_id)?;
        let positions = entries
            .iter()
            .map(|entry| u64::from(entry.position))
            .collect::<BTreeSet<_>>();

        let round_client = {
            let mut clients = self.clients.lock().map_err(|e| VotingError::Internal {
                message: format!("tree client registry lock poisoned: {e}"),
            })?;
            clients
                .entry(round_id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(RoundTreeClient::empty())))
                .clone()
        };
        let mut round_client = round_client.lock().map_err(|e| VotingError::Internal {
            message: format!("round tree client lock poisoned: {e}"),
        })?;

        if round_client.needs_resync_for(&positions) {
            *round_client = RoundTreeClient::empty();
        }
        round_client.mark_positions(&positions);

        round_client
            .client
            .sync(api)
            .map_err(|e| VotingError::Internal {
                message: format!("vote tree sync failed: {}", e),
            })?;

        let anchor_height = round_client.client.last_synced_height().unwrap_or(0);
        let validation = (|| {
            let mut missing_bundle = None;
            for entry in entries {
                let Some(expected_delegation_van) = entry.expected_delegation_van else {
                    continue;
                };
                let Some(path) = round_client
                    .client
                    .witness(u64::from(entry.position), anchor_height)
                else {
                    missing_bundle.get_or_insert(entry.bundle_index);
                    continue;
                };
                let root = round_client
                    .client
                    .root_at_height(anchor_height)
                    .ok_or_else(|| VotingError::Internal {
                        message: format!(
                            "synced vote tree has no root at anchor height {anchor_height}"
                        ),
                    })?;
                if !path.verify(expected_delegation_van, root) {
                    return Err(VotingError::InvalidInput {
                        message: format!(
                            "confirmed delegation bundle {} does not match its synced vote-tree leaf",
                            entry.bundle_index
                        ),
                    });
                }
            }
            Ok(missing_bundle)
        })();
        match validation {
            Err(error) => {
                *round_client = RoundTreeClient::empty();
                return Err(error);
            }
            Ok(Some(bundle_index)) => {
                return Err(VotingError::InvalidInput {
                    message: format!(
                        "confirmed delegation bundle {bundle_index} is absent from the synced vote tree"
                    ),
                });
            }
            Ok(None) => {}
        }

        // Empty tree is valid before the first delegation commitment is appended.
        // Report height 0 so callers can proceed instead of failing sync.
        Ok(round_client.client.last_synced_height().unwrap_or(0))
    }

    /// Generate a VAN Merkle witness for ZKP #2.
    ///
    /// Requires `sync` to have been called first for this round. Loads the VAN
    /// position for the specified bundle and generates a witness at the given
    /// anchor height.
    pub fn generate_van_witness(
        &self,
        db: &VotingDb,
        round_id: &str,
        bundle_index: u32,
        anchor_height: u32,
    ) -> Result<VanWitness, VotingError> {
        self.coordinated(round_id, || {
            self.witness_locked(db, round_id, bundle_index, anchor_height)
        })
    }

    fn witness_locked(
        &self,
        db: &VotingDb,
        round_id: &str,
        bundle_index: u32,
        anchor_height: u32,
    ) -> Result<VanWitness, VotingError> {
        let van_position = db.load_van_position(round_id, bundle_index)?;

        let round_client = {
            let clients = self.clients.lock().map_err(|e| VotingError::Internal {
                message: format!("tree client registry lock poisoned: {e}"),
            })?;
            clients
                .get(round_id)
                .cloned()
                .ok_or_else(|| VotingError::InvalidInput {
                    message: "must call sync before generate_van_witness".to_string(),
                })?
        };
        let round_client = round_client.lock().map_err(|e| VotingError::Internal {
            message: format!("round tree client lock poisoned: {e}"),
        })?;

        let path = round_client
            .client
            .witness(van_position as u64, anchor_height)
            .ok_or_else(|| VotingError::Internal {
                message: format!(
                    "failed to generate witness for position {} at height {}",
                    van_position, anchor_height
                ),
            })?;

        Ok(VanWitness::from((path, anchor_height)))
    }

    /// Rounds that currently hold in-memory tree state on this sync, synced
    /// or partially synced.
    ///
    /// A diagnostic for hosts accounting for memory or deciding what
    /// [`Self::reset`] would drop; a round appears from its first sync attempt
    /// until it is reset.
    pub fn cached_rounds(&self) -> Vec<String> {
        self.clients
            .lock()
            .map(|clients| clients.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Whether any caller besides this sync still holds the transport it was
    /// built over. A sync over a host transport nobody else holds can never
    /// be asked for by that transport again.
    pub fn transport_is_shared(&self) -> bool {
        Arc::strong_count(&self.transport) > 1
    }

    /// The round's in-memory state, without blocking on a sync in flight.
    ///
    /// Only the map lock is taken; the round lock is tried, so a caller
    /// choosing between clients is never parked behind another client's
    /// network round trip.
    pub fn cached_round_state(&self, round_id: &str) -> CachedRoundState {
        let Ok(clients) = self.clients.lock() else {
            return CachedRoundState::Absent;
        };
        let Some(round_client) = clients.get(round_id).map(Arc::clone) else {
            return CachedRoundState::Absent;
        };
        drop(clients);
        let state = match round_client.try_lock() {
            Ok(round_client) => {
                CachedRoundState::SyncedTo(round_client.client.last_synced_height().unwrap_or(0))
            }
            Err(std::sync::TryLockError::WouldBlock) => CachedRoundState::Syncing,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => CachedRoundState::SyncedTo(
                poisoned
                    .into_inner()
                    .client
                    .last_synced_height()
                    .unwrap_or(0),
            ),
        };
        state
    }

    /// Drop the in-memory TreeClient for a round so the next `sync` call
    /// creates a fresh one and does a full resync. This recovers from stale
    /// state that would otherwise cause `StartIndexMismatch` or `RootMismatch`.
    /// If `round_id` is empty, all clients are dropped.
    pub fn reset(&self, round_id: &str) -> Result<(), VotingError> {
        if round_id.is_empty() {
            let _gate = self.reset_gate.write().map_err(|_| VotingError::Internal {
                message: "tree reset gate poisoned".into(),
            })?;
            self.reset_locked(round_id)
        } else {
            self.coordinated(round_id, || self.reset_locked(round_id))
        }
    }

    fn reset_locked(&self, round_id: &str) -> Result<(), VotingError> {
        let mut guard = self.clients.lock().map_err(|e| VotingError::Internal {
            message: format!("tree client registry lock poisoned: {e}"),
        })?;
        if round_id.is_empty() {
            guard.clear();
        } else {
            guard.remove(round_id);
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "tree_sync/tests/coordination.rs"]
mod coordination_tests;
