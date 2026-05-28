//! Precomputation APIs for delegation inputs.
//!
//! Precompute operations prepare data that is expensive to derive during proof
//! generation: Orchard note witnesses from the wallet database and PIR-backed
//! non-membership proofs for nullifiers.

use std::borrow::Borrow;

#[cfg(any(feature = "tree-sync", feature = "client-tree-sync"))]
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

use zcash_client_sqlite::WalletDb;

use crate::{
    round::VotingDb,
    types::{NoteInfo, VotingError, WitnessData},
};

#[cfg(any(feature = "tree-sync", feature = "client-tree-sync"))]
pub use crate::vote::VanWitness;

#[cfg(any(feature = "tree-sync", feature = "client-tree-sync"))]
static VOTE_TREE_SYNCS: OnceLock<Mutex<HashMap<String, Arc<crate::tree_sync::VoteTreeSync>>>> =
    OnceLock::new();

/// Result of PIR precomputation for one delegation bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PirPrecomputeReport {
    pub cached: u32,
    pub fetched: u32,
}

/// Stores `tree_state_bytes`, generates Orchard witnesses, and caches them.
///
/// The tree state must be the exact snapshot anchor for the round. The wallet
/// database supplies historical note paths; voting state is persisted in
/// `db`.
pub fn note_witnesses<C, P, CL, R>(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    tree_state_bytes: &[u8],
    notes: &[NoteInfo],
    wallet_db: &WalletDb<C, P, CL, R>,
) -> Result<Vec<WitnessData>, VotingError>
where
    C: Borrow<rusqlite::Connection>,
    P: zcash_protocol::consensus::Parameters,
{
    crate::witness::store_tree_state_and_generate_note_witnesses(
        db,
        round_id,
        bundle_index,
        tree_state_bytes,
        notes,
        wallet_db,
    )
}

/// Loads a round's cached tree state, generates Orchard witnesses, and caches them.
///
/// This is the FFI-friendly variant for callers that already persisted the
/// round tree state through [`VotingDb`] and should not reach into storage
/// query helpers.
pub fn stored_note_witnesses<C, P, CL, R>(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    notes: &[NoteInfo],
    wallet_db: &WalletDb<C, P, CL, R>,
) -> Result<Vec<WitnessData>, VotingError>
where
    C: Borrow<rusqlite::Connection>,
    P: zcash_protocol::consensus::Parameters,
{
    let tree_state_bytes = {
        let conn = db.conn();
        let wallet_id = db.wallet_id();
        crate::storage::queries::load_tree_state(&conn, round_id, &wallet_id)?
    };
    note_witnesses(
        db,
        round_id,
        bundle_index,
        &tree_state_bytes,
        notes,
        wallet_db,
    )
}

/// Verifies an Orchard note witness against its stored root.
///
/// Returns `Ok(())` when the witness recomputes to the expected root and
/// [`VotingError::InvalidInput`] when the bytes are malformed or mismatched.
pub fn verify_witness(witness: &WitnessData) -> Result<(), VotingError> {
    if crate::witness::verify_witness(witness)? {
        Ok(())
    } else {
        Err(VotingError::InvalidInput {
            message: format!(
                "witness root mismatch at note position {}",
                witness.position
            ),
        })
    }
}

/// Syncs the vote commitment tree for one round and returns the latest height.
#[cfg(any(feature = "tree-sync", feature = "client-tree-sync"))]
pub fn sync_vote_tree(db: &VotingDb, round_id: &str, node_url: &str) -> Result<u32, VotingError> {
    vote_tree_sync_for(db)?.sync(db, round_id, node_url)
}

/// Generates the VAN witness needed by `vote::commit`.
#[cfg(any(feature = "tree-sync", feature = "client-tree-sync"))]
pub fn van_witness(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    anchor_height: u32,
) -> Result<VanWitness, VotingError> {
    vote_tree_sync_for(db)?.generate_van_witness(db, round_id, bundle_index, anchor_height)
}

/// Drops cached vote tree state for one round, or all rounds when `round_id` is empty.
#[cfg(any(feature = "tree-sync", feature = "client-tree-sync"))]
pub fn reset_vote_tree(db: &VotingDb, round_id: &str) -> Result<(), VotingError> {
    vote_tree_sync_for(db)?.reset(round_id)
}

#[cfg(any(feature = "tree-sync", feature = "client-tree-sync"))]
fn vote_tree_sync_for(db: &VotingDb) -> Result<Arc<crate::tree_sync::VoteTreeSync>, VotingError> {
    let wallet_id = db.wallet_id();
    let mut guard = VOTE_TREE_SYNCS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|e| VotingError::Internal {
            message: format!("vote tree sync registry lock poisoned: {e}"),
        })?;
    Ok(guard
        .entry(wallet_id)
        .or_insert_with(|| Arc::new(crate::tree_sync::VoteTreeSync::new()))
        .clone())
}

/// Fetches and persists PIR-backed IMT non-membership proofs for one bundle.
///
/// This must run after delegation setup, because padded-note secrets are
/// produced by the PCZT construction step.
#[cfg(feature = "pir")]
pub fn delegation_pir(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    notes: &[NoteInfo],
    pir_client: &pir_client::PirClientBlocking,
    network: crate::types::Network,
) -> Result<PirPrecomputeReport, VotingError> {
    let result =
        db.precompute_delegation_pir(round_id, bundle_index, notes, pir_client, network.id())?;
    Ok(PirPrecomputeReport {
        cached: result.cached_count,
        fetched: result.fetched_count,
    })
}

#[cfg(all(test, any(feature = "tree-sync", feature = "client-tree-sync")))]
mod tree_sync_tests {
    use super::*;
    use pasta_curves::Fp;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };
    use vote_commitment_tree::{MemoryTreeServer, MerkleHashVote};

    const ROUND_ID: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    const WALLET_ID: &str = "wallet-tree-sync";

    #[test]
    fn vote_tree_sync_witness_and_reset_happy_path() {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(WALLET_ID);
        db.create_round(&round_params()).unwrap();
        db.ensure_bundles(ROUND_ID, &[note(0)]).unwrap();
        db.store_van_position(ROUND_ID, 0, 0).unwrap();
        let server = start_tree_server(1, vec![1], 2);

        let height = sync_vote_tree(&db, ROUND_ID, &server).unwrap();
        let witness = van_witness(&db, ROUND_ID, 0, height).unwrap();
        reset_vote_tree(&db, ROUND_ID).unwrap();

        assert_eq!(height, 1);
        assert_eq!(witness.position, 0);
        assert_eq!(witness.anchor_height, 1);
        assert_eq!(witness.auth_path.len(), crate::vote::VAN_AUTH_PATH_LEN);
        assert!(witness.auth_path.iter().all(|hash| hash.len() == 32));
    }

    #[derive(Clone)]
    struct MockTreeBlock {
        height: u32,
        start_index: u64,
        leaf: String,
        root: String,
    }

    fn start_tree_server(height: u32, leaf_values: Vec<u64>, expected_requests: usize) -> String {
        let (latest_root, blocks) = mock_tree_blocks(&leaf_values);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            for _ in 0..expected_requests {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 2048];
                let len = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..len]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");
                let body = tree_response_body(path, height, &latest_root, &blocks);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        url
    }

    fn tree_response_body(
        path: &str,
        height: u32,
        latest_root: &Option<String>,
        blocks: &[MockTreeBlock],
    ) -> String {
        if path.ends_with("/latest") {
            match latest_root {
                Some(root) => format!(
                    r#"{{"tree":{{"next_index":{},"root":"{}","height":{}}}}}"#,
                    blocks.len(),
                    root,
                    height
                ),
                None => format!(
                    r#"{{"tree":{{"next_index":{},"height":{}}}}}"#,
                    blocks.len(),
                    height
                ),
            }
        } else if path.contains("/leaves?") {
            if height == 0 || blocks.is_empty() {
                r#"{"blocks":[]}"#.to_string()
            } else {
                let Some(block) = blocks.first() else {
                    return r#"{"blocks":[]}"#.to_string();
                };
                format!(
                    r#"{{"blocks":[{{"height":{},"start_index":{},"leaves":["{}"],"root":"{}"}}]}}"#,
                    block.height, block.start_index, block.leaf, block.root
                )
            }
        } else {
            r#"{"tree":null}"#.to_string()
        }
    }

    fn mock_tree_blocks(leaf_values: &[u64]) -> (Option<String>, Vec<MockTreeBlock>) {
        if leaf_values.is_empty() {
            return (None, vec![]);
        }

        let mut server = MemoryTreeServer::empty();
        let mut blocks = Vec::with_capacity(leaf_values.len());
        for (index, value) in leaf_values.iter().copied().enumerate() {
            let height = u32::try_from(index + 1).unwrap();
            server.append(Fp::from(value)).unwrap();
            server.checkpoint(height).unwrap();
            let root = server.root_at_height(height).unwrap();
            blocks.push(MockTreeBlock {
                height,
                start_index: u64::try_from(index).unwrap(),
                leaf: base64_encode(&MerkleHashVote::from_fp(Fp::from(value)).to_bytes()),
                root: base64_encode(&MerkleHashVote::from_fp(root).to_bytes()),
            });
        }

        let latest_root = blocks.last().map(|block| block.root.clone());
        (latest_root, blocks)
    }

    fn base64_encode(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0];
            let b1 = *chunk.get(1).unwrap_or(&0);
            let b2 = *chunk.get(2).unwrap_or(&0);
            out.push(TABLE[(b0 >> 2) as usize] as char);
            out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
            if chunk.len() > 1 {
                out.push(TABLE[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(TABLE[(b2 & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    fn round_params() -> crate::round::RoundParams {
        crate::round::RoundParams {
            vote_round_id: ROUND_ID.to_string(),
            snapshot_height: 100,
            ea_pk: vec![1; 32],
            nc_root: vec![2; 32],
            nullifier_imt_root: vec![3; 32],
        }
    }

    fn note(position: u64) -> NoteInfo {
        NoteInfo {
            commitment: vec![1; 32],
            nullifier: vec![2; 32],
            value: crate::governance::BALLOT_DIVISOR,
            position,
            diversifier: vec![3; 11],
            rho: vec![4; 32],
            rseed: vec![5; 32],
            scope: 0,
            ufvk_str: "uviewtest".to_string(),
        }
    }
}
