//! Precomputation APIs for delegation inputs.
//!
//! [`precompute_delegation`] is the primary warm-up entry point: round setup,
//! bundle witnesses, governance PCZT construction, and PIR-backed nullifier
//! proofs. Lower-level helpers remain available for callers that already
//! persisted intermediate state.

use std::borrow::Borrow;

#[cfg(any(feature = "tree-sync", feature = "client-tree-sync"))]
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

use zcash_client_sqlite::WalletDb;

use crate::{
    round::{self, VotingDb},
    types::{NoteInfo, VotingError, VotingRoundParams, WitnessData},
};

#[cfg(feature = "pir")]
use crate::{
    delegate::{
        cache_prepared_setup, prepared_epoch, setup, BranchIdProvider, DelegationKeys,
        PreparedDelegationReport,
    },
    round::BundleLayout,
    types::{Cancellation, DelegationProgressReporter, Network},
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
    network: Network,
) -> Result<PirPrecomputeReport, VotingError> {
    let result =
        db.precompute_delegation_pir(round_id, bundle_index, notes, pir_client, network.id())?;
    Ok(PirPrecomputeReport {
        cached: result.cached_count,
        fetched: result.fetched_count,
    })
}

/// Inputs for [`precompute_delegation`].
#[cfg(feature = "pir")]
pub struct PrecomputeDelegationInputs<'a> {
    pub round_params: &'a VotingRoundParams,
    pub session_json: Option<&'a str>,
    pub bundle_index: u32,
    pub round_note_infos: &'a [NoteInfo],
    pub anchor_tree_state_bytes: &'a [u8],
    pub keys: &'a DelegationKeys,
    pub branch_id_provider: &'a dyn BranchIdProvider,
    pub network: Network,
    pub cancellation: &'a dyn Cancellation,
}

/// Warms delegation bundle state: round setup, witnesses, governance PCZT, and PIR.
///
/// Callers supply the full round note selection, the snapshot anchor tree state,
/// and a wallet database handle for witness generation. Delegation signing keys
/// must already include the hotkey and display round name.
///
/// # Errors
///
/// Returns [`VotingError::Cancelled`] when `cancellation` is set. Other failures
/// come from round setup, bundle planning, witness generation, PCZT
/// construction, PIR precompute, or prepared-setup cache insertion.
#[cfg(feature = "pir")]
pub fn precompute_delegation<C, P, CL, R>(
    db: &VotingDb,
    wallet_db: &WalletDb<C, P, CL, R>,
    inputs: PrecomputeDelegationInputs<'_>,
    pir_client: &pir_client::PirClientBlocking,
    stages: &dyn DelegationProgressReporter,
) -> Result<PreparedDelegationReport, VotingError>
where
    C: Borrow<rusqlite::Connection>,
    P: zcash_protocol::consensus::Parameters,
{
    ensure_not_cancelled(inputs.cancellation)?;
    let round_id = inputs.round_params.vote_round_id.as_str();
    db.ensure_round(inputs.round_params, inputs.session_json)?;

    let bundle_setup = db.ensure_bundles_with_skipped_suffix(round_id, inputs.round_note_infos)?;
    let bundle_note_infos =
        round::bundle_notes_for_index(inputs.round_note_infos, &bundle_setup, inputs.bundle_index)?;

    note_witnesses(
        db,
        round_id,
        inputs.bundle_index,
        inputs.anchor_tree_state_bytes,
        &bundle_note_infos,
        wallet_db,
    )?;

    warm_delegation_pir(
        db,
        round_id,
        inputs.bundle_index,
        &bundle_note_infos,
        inputs.keys,
        bundle_setup,
        inputs.branch_id_provider,
        pir_client,
        inputs.network,
        inputs.cancellation,
        stages,
    )
}

/// Builds governance PCZT material, runs PIR precompute, and caches prepared setup.
///
/// Witnesses must already be cached for `notes`. Prefer [`precompute_delegation`]
/// for the full warm-up path from round notes and anchor tree state.
///
/// # Errors
///
/// Returns [`VotingError::Cancelled`] when `cancellation` is set. Other
/// failures come from PCZT construction, PIR precompute, or prepared-setup
/// cache insertion.
#[cfg(feature = "pir")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn warm_delegation_pir(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    notes: &[NoteInfo],
    keys: &DelegationKeys,
    layout: BundleLayout,
    branch_id_provider: &dyn BranchIdProvider,
    pir_client: &pir_client::PirClientBlocking,
    network: Network,
    cancellation: &dyn Cancellation,
    stages: &dyn DelegationProgressReporter,
) -> Result<PreparedDelegationReport, VotingError> {
    ensure_not_cancelled(cancellation)?;
    let prepared_epoch = prepared_epoch(db)?;
    let setup = setup(
        db,
        round_id,
        bundle_index,
        notes,
        keys,
        branch_id_provider,
        stages,
    )?;
    ensure_not_cancelled(cancellation)?;
    let report = delegation_pir(db, round_id, bundle_index, notes, pir_client, network)?;
    ensure_not_cancelled(cancellation)?;
    let _cached = cache_prepared_setup(
        db,
        round_id,
        bundle_index,
        keys,
        notes,
        prepared_epoch,
        setup,
    )?;

    Ok(PreparedDelegationReport {
        report,
        layout,
        bundle_index,
    })
}

#[cfg(feature = "pir")]
fn ensure_not_cancelled(cancellation: &dyn Cancellation) -> Result<(), VotingError> {
    if cancellation.is_cancelled() {
        Err(VotingError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(all(test, feature = "pir"))]
mod pir_tests {
    use super::*;
    use crate::delegate::{BranchIdProvider, DelegationKeys};
    use crate::round::BundleLayout;
    use crate::types::{Cancellation, NoopProgressReporter, NoteInfo};

    const ROUND_ID: &str = "0101010101010101010101010101010101010101010101010101010101010101";

    #[test]
    fn warm_delegation_pir_honours_cancellation() {
        struct AlwaysCancelled;

        impl Cancellation for AlwaysCancelled {
            fn is_cancelled(&self) -> bool {
                true
            }
        }

        struct FixedBranchId(u32);

        impl BranchIdProvider for FixedBranchId {
            fn consensus_branch_id(&self) -> Result<u32, VotingError> {
                Ok(self.0)
            }
        }

        struct StaticPirTransport;

        impl pir_client::Transport for StaticPirTransport {
            fn get<'a>(&'a self, url: &'a str) -> pir_client::TransportFuture<'a> {
                Box::pin(async move {
                    let path = request_path(url);
                    match path {
                        "/tier0" => Ok(transport_response(vec![
                            0;
                            ((1usize
                                << pir_types::TIER0_LAYERS)
                                - 1)
                                * 32
                                + pir_types::TIER1_ROWS * 64
                        ])),
                        "/params/tier1" => Ok(transport_response(
                            serde_json::to_vec(&pir_types::YpirScenario {
                                num_items: pir_types::TIER1_YPIR_ROWS,
                                item_size_bits: pir_types::TIER1_ITEM_BITS,
                            })
                            .unwrap(),
                        )),
                        "/params/tier2" => Ok(transport_response(
                            serde_json::to_vec(&pir_types::YpirScenario {
                                num_items: pir_types::TIER1_YPIR_ROWS,
                                item_size_bits: pir_types::TIER2_ITEM_BITS,
                            })
                            .unwrap(),
                        )),
                        "/root" => Ok(transport_response(
                            serde_json::to_vec(&pir_types::RootInfo {
                                root29: hex::encode([0u8; 32]),
                                root25: hex::encode([0u8; 32]),
                                num_ranges: 1,
                                pir_depth: pir_types::PIR_DEPTH,
                                height: None,
                            })
                            .unwrap(),
                        )),
                        _ => Err(anyhow::anyhow!("unexpected GET {path}")),
                    }
                })
            }

            fn post<'a>(&'a self, url: &'a str, _body: Vec<u8>) -> pir_client::TransportFuture<'a> {
                Box::pin(async move {
                    Err(anyhow::anyhow!(
                        "unexpected POST {}; warm path should cancel first",
                        request_path(url)
                    ))
                })
            }
        }

        fn request_path(url: &str) -> &str {
            let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
            without_scheme
                .find('/')
                .map(|idx| &without_scheme[idx..])
                .unwrap_or("/")
        }

        fn transport_response(body: Vec<u8>) -> pir_client::TransportResponse {
            pir_client::TransportResponse {
                status: 200,
                headers: Vec::new(),
                body,
            }
        }

        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id("warm-delegation-cancel");
        let keys = DelegationKeys::with_hotkey_bytes(
            vec![8; 96],
            &[7; 43],
            [9; 32],
            0,
            0,
            1,
            "Demo Round".to_string(),
        )
        .unwrap();
        let notes = vec![NoteInfo {
            commitment: vec![1; 32],
            nullifier: vec![2; 32],
            value: crate::governance::BALLOT_DIVISOR,
            position: 42,
            diversifier: vec![3; 11],
            rho: vec![4; 32],
            rseed: vec![5; 32],
            scope: 0,
            ufvk_str: "uviewtest".to_string(),
        }];
        let layout = BundleLayout {
            bundle_count: 1,
            eligible_weight: 42,
            dropped_count: 0,
        };
        let branch_id = FixedBranchId(0xC8E71055);
        let pir_client = pir_client::PirClientBlocking::with_transport(
            "https://pir.test",
            std::sync::Arc::new(StaticPirTransport),
        )
        .unwrap();
        let stages = NoopProgressReporter;

        let err = warm_delegation_pir(
            &db,
            ROUND_ID,
            0,
            &notes,
            &keys,
            layout,
            &branch_id,
            &pir_client,
            Network::Testnet,
            &AlwaysCancelled,
            &stages,
        )
        .unwrap_err();

        assert!(matches!(err, VotingError::Cancelled));
    }
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
