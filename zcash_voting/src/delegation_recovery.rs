//! Recovery of confirmed delegation VANs after local voting-state loss.
//!
//! The recovery path reconstructs the initial VAN for each canonical bundle,
//! locates those public commitments in the vote tree, and records only the
//! state needed to continue with ZKP #2. The normal vote-tree sync remains the
//! authority for validating the discovered positions before witness creation.

#[allow(unused_imports)]
pub(crate) use crate::backend::pasta_curves;

use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc, time::Duration};

use pasta_curves::group::ff::PrimeField;
use rusqlite::TransactionBehavior;
use vote_commitment_tree::{
    sync_api::{BlockCommitmentsPage, TreeState},
    MerkleHashVote, SyncLimits,
};
use vote_commitment_tree_client::http_sync_api::{
    parse_commitment_leaves_response, parse_latest_tree_response,
};

use crate::{
    action::derive_hotkey_x_coords_from_raw_address,
    governance,
    helper::{
        transport::{HelperResponse, HelperTransport, HelperTransportError},
        url::canonicalize_server_base_url,
    },
    note_bundling::recoverable_bundle_policy_v1,
    round::bundle_notes_for_index_for_round,
    storage::{queries, VotingDb},
    types::{validate_vote_round_id_hex, NoteInfo, VotingError, VotingHotkey},
    van_blinding::VanBlindingKey,
};

const TREE_QUERY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_TREE_QUERY_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Future returned by a host-owned vote-tree query transport.
pub type VoteTreeQueryFuture<'a> =
    Pin<Box<dyn Future<Output = Result<HelperResponse, HelperTransportError>> + Send + 'a>>;

/// Route-aware GET transport used only by delegation recovery tree queries.
///
/// Existing [`HelperTransport`] implementations automatically satisfy this
/// interface, so wallets can reuse the same fail-closed Tor or direct route
/// without depending on the vote-chain submission client.
pub trait VoteTreeQueryTransport: Send + Sync {
    /// Performs one complete GET request under `timeout`.
    fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> VoteTreeQueryFuture<'a>;
}

impl<T> VoteTreeQueryTransport for T
where
    T: HelperTransport + ?Sized,
{
    fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> VoteTreeQueryFuture<'a> {
        HelperTransport::get(self, url, timeout)
    }
}

/// Read-only client for locating reconstructed delegation VANs in vote trees.
#[derive(Clone)]
pub struct DelegationRecoveryClient {
    transport: Arc<dyn VoteTreeQueryTransport>,
    endpoints: Vec<String>,
}

impl DelegationRecoveryClient {
    /// Creates a recovery client from host-routed HTTP and vote-chain bases.
    pub fn new(
        transport: Arc<dyn VoteTreeQueryTransport>,
        endpoints: &[String],
    ) -> Result<Self, VotingError> {
        if endpoints.is_empty() {
            return Err(VotingError::InvalidInput {
                message: "vote-chain endpoint set must not be empty".to_string(),
            });
        }
        let mut canonical = Vec::with_capacity(endpoints.len());
        for endpoint in endpoints {
            let endpoint = canonicalize_server_base_url(endpoint, "vote-chain")?;
            if canonical.contains(&endpoint) {
                return Err(VotingError::InvalidInput {
                    message: "vote-chain endpoint set contains duplicate canonical identities"
                        .to_string(),
                });
            }
            canonical.push(endpoint);
        }
        Ok(Self {
            transport,
            endpoints: canonical,
        })
    }

    fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }

    async fn tree_state_once(
        &self,
        endpoint_index: usize,
        round_id: &str,
    ) -> Result<TreeState, DelegationVanRecoveryError> {
        let base = &self.endpoints[endpoint_index % self.endpoint_count()];
        let url = format!("{base}/shielded-vote/v1/commitment-tree/{round_id}/latest");
        let response = self.transport.get(&url, TREE_QUERY_TIMEOUT).await?;
        require_successful_tree_response(&response)?;
        parse_latest_tree_response(response.body())
            .map_err(|error| DelegationVanRecoveryError::Decode(error.to_string()))
    }

    async fn tree_commitments_once(
        &self,
        endpoint_index: usize,
        round_id: &str,
        from_height: u32,
        to_height: u32,
    ) -> Result<BlockCommitmentsPage, DelegationVanRecoveryError> {
        let base = &self.endpoints[endpoint_index % self.endpoint_count()];
        let url = format!(
            "{base}/shielded-vote/v1/commitment-tree/{round_id}/leaves?from_height={from_height}&to_height={to_height}"
        );
        let response = self.transport.get(&url, TREE_QUERY_TIMEOUT).await?;
        require_successful_tree_response(&response)?;
        parse_commitment_leaves_response(response.body())
            .map_err(|error| DelegationVanRecoveryError::Decode(error.to_string()))
    }
}

fn require_successful_tree_response(
    response: &HelperResponse,
) -> Result<(), DelegationVanRecoveryError> {
    if !response.is_success() {
        return Err(DelegationVanRecoveryError::HttpStatus(response.status()));
    }
    if response.body().len() > MAX_TREE_QUERY_RESPONSE_BYTES {
        return Err(DelegationVanRecoveryError::Decode(format!(
            "response exceeds {MAX_TREE_QUERY_RESPONSE_BYTES} byte limit"
        )));
    }
    let is_json = response.content_type().is_some_and(|content_type| {
        content_type
            .split(';')
            .next()
            .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"))
    });
    if !is_json {
        return Err(DelegationVanRecoveryError::Decode(
            "response Content-Type must be application/json".to_string(),
        ));
    }
    Ok(())
}

/// Errors from delegation VAN reconstruction, chain scanning, or persistence.
#[derive(Debug, thiserror::Error)]
pub enum DelegationVanRecoveryError {
    /// Local reconstruction or persistence failed.
    #[error("delegation recovery failed: {0}")]
    Voting(#[from] VotingError),
    /// The host-owned route could not complete a tree query.
    #[error("delegation recovery tree transport failed: {0}")]
    Transport(#[from] HelperTransportError),
    /// A tree endpoint returned a non-success status.
    #[error("delegation recovery tree endpoint returned HTTP {0}")]
    HttpStatus(u16),
    /// The tree response or paginated stream was inconsistent.
    #[error("delegation recovery tree response was not usable: {0}")]
    Decode(String),
    /// The host cancelled recovery.
    #[error("delegation recovery cancelled")]
    Cancelled,
}

struct DelegationVanCandidate {
    bundle_index: u32,
    notes: Vec<NoteInfo>,
    van_comm_rand: [u8; 32],
    van: [u8; 32],
    total_note_value: u64,
    address_index: u32,
}

/// Reconstructs and recovers confirmed initial delegation VANs for one round.
///
/// Bundle planning is fixed to [`recoverable_bundle_policy_v1`]. The scan is
/// performed only after local reconstruction, and no recovered fields are
/// written until one complete endpoint stream passes start-index, pagination,
/// and final-size consistency checks. Found bundles are committed in one SQLite
/// transaction. Missing bundles remain prepared and are reported to the caller.
///
/// The scan only discovers candidate leaf positions. Before constructing ZKP
/// #2, callers must run the normal vote-tree sync. That path independently
/// validates the complete tree and verifies that every recovered position
/// contains its reconstructed VAN.
///
/// This recovers the initial VAN created by ZKP #1. It does not discover a
/// successor VAN created by an already-confirmed ZKP #2 transaction.
///
/// Returns the exact canonical bundle indices recovered by this scan. Bundles
/// not returned remain prepared for ordinary delegation submission.
pub async fn recover_confirmed_delegations(
    db: &VotingDb,
    recovery_client: &DelegationRecoveryClient,
    round_id: &str,
    round_note_infos: &[NoteInfo],
    voting_hotkey: &VotingHotkey,
    cancel: &(dyn Fn() -> bool + Send + Sync),
) -> Result<Vec<u32>, DelegationVanRecoveryError> {
    validate_vote_round_id_hex(round_id)?;
    let candidates = reconstruct_candidates(db, round_id, round_note_infos, voting_hotkey)?;
    if candidates.is_empty() {
        return Err(VotingError::InvalidInput {
            message: format!("round {round_id} has no recoverable delegation bundles"),
        }
        .into());
    }
    if cancel() {
        return Err(DelegationVanRecoveryError::Cancelled);
    }

    let mut last_error = None;
    let mut discovery = None;
    for endpoint_index in 0..recovery_client.endpoint_count() {
        match discover_endpoint_positions(
            recovery_client,
            endpoint_index,
            round_id,
            &candidates,
            cancel,
        )
        .await
        {
            Ok(scan) => {
                discovery = Some(scan);
                break;
            }
            Err(DelegationVanRecoveryError::Cancelled) => {
                return Err(DelegationVanRecoveryError::Cancelled)
            }
            Err(error) => last_error = Some(error),
        }
    }
    let observed_by_bundle = discovery.ok_or_else(|| {
        last_error
            .unwrap_or_else(|| DelegationVanRecoveryError::Decode("no endpoint result".to_string()))
    })?;
    if cancel() {
        return Err(DelegationVanRecoveryError::Cancelled);
    }

    let mut recovered_bundle_indices = Vec::new();
    let mut recovered_states = Vec::new();
    for candidate in &candidates {
        if let Some(&van_leaf_position) = observed_by_bundle.get(&candidate.bundle_index) {
            recovered_bundle_indices.push(candidate.bundle_index);
            recovered_states.push(queries::RecoveredDelegationState {
                bundle_index: candidate.bundle_index,
                van_comm_rand: candidate.van_comm_rand,
                gov_comm: candidate.van,
                total_note_value: candidate.total_note_value,
                address_index: candidate.address_index,
                van_leaf_position,
            });
        }
    }

    if !recovered_states.is_empty() {
        let wallet_id = db.wallet_id();
        let mut conn = db.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| VotingError::Internal {
                message: format!("begin recovered delegation transaction failed: {error}"),
            })?;
        for state in &recovered_states {
            let candidate = candidates
                .iter()
                .find(|candidate| candidate.bundle_index == state.bundle_index)
                .expect("recovered state is built from a candidate");
            queries::require_bundle_notes(
                &tx,
                round_id,
                &wallet_id,
                state.bundle_index,
                &candidate.notes,
            )?;
            queries::store_recovered_delegation_state(&tx, round_id, &wallet_id, state)?;
        }
        tx.commit().map_err(|error| VotingError::Internal {
            message: format!("commit recovered delegation transaction failed: {error}"),
        })?;
    }

    Ok(recovered_bundle_indices)
}

fn reconstruct_candidates(
    db: &VotingDb,
    round_id: &str,
    round_note_infos: &[NoteInfo],
    voting_hotkey: &VotingHotkey,
) -> Result<Vec<DelegationVanCandidate>, VotingError> {
    let wallet_id = db.wallet_id();
    let (round, network) =
        queries::load_round_params_with_network(&db.conn(), round_id, &wallet_id)?;
    if network != voting_hotkey.network() {
        return Err(VotingError::InvalidInput {
            message: format!(
                "voting hotkey network {:?} does not match round network {:?}",
                voting_hotkey.network(),
                network
            ),
        });
    }
    let layout = db.ensure_bundles_with_skipped_suffix_with_policy(
        round_id,
        round_note_infos,
        recoverable_bundle_policy_v1(),
    )?;
    let round_id_bytes: [u8; 32] = hex::decode(&round.vote_round_id)
        .map_err(|error| VotingError::InvalidInput {
            message: format!("vote_round_id is not valid hex: {error}"),
        })?
        .try_into()
        .map_err(|bytes: Vec<u8>| VotingError::InvalidInput {
            message: format!("vote_round_id must be 32 bytes, got {}", bytes.len()),
        })?;
    let (g_d_new_x, pk_d_new_x) =
        derive_hotkey_x_coords_from_raw_address(voting_hotkey.raw_orchard_address())?;
    let blinding_key = VanBlindingKey::from_hotkey(voting_hotkey);

    let mut candidates = Vec::with_capacity(layout.bundle_count as usize);
    let mut bundles_by_van = BTreeMap::<[u8; 32], u32>::new();
    for bundle_index in 0..layout.bundle_count {
        let notes = bundle_notes_for_index_for_round(
            round_note_infos,
            &layout,
            bundle_index,
            db,
            round_id,
        )?;
        let total_note_value = notes
            .iter()
            .try_fold(0u64, |total, note| total.checked_add(note.value))
            .ok_or_else(|| VotingError::InvalidInput {
                message: format!("total note weight overflows u64 for bundle {bundle_index}"),
            })?;
        let van_blinding = blinding_key.derive(network, &round, bundle_index, &notes)?;
        let van_comm_rand = van_blinding.field().to_repr();
        let van: [u8; 32] = governance::construct_van(
            &g_d_new_x,
            &pk_d_new_x,
            total_note_value,
            &round_id_bytes,
            &van_comm_rand,
        )?
        .try_into()
        .expect("construct_van returns a canonical 32-byte field encoding");
        if let Some(existing_bundle) = bundles_by_van.insert(van, bundle_index) {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "delegation bundles {existing_bundle} and {bundle_index} reconstruct the same VAN"
                ),
            });
        }
        candidates.push(DelegationVanCandidate {
            bundle_index,
            notes,
            van_comm_rand,
            van,
            total_note_value,
            address_index: voting_hotkey.address_index(),
        });
    }
    Ok(candidates)
}

async fn discover_endpoint_positions(
    recovery_client: &DelegationRecoveryClient,
    endpoint_index: usize,
    round_id: &str,
    candidates: &[DelegationVanCandidate],
    cancel: &(dyn Fn() -> bool + Send + Sync),
) -> Result<BTreeMap<u32, u32>, DelegationVanRecoveryError> {
    let limits = SyncLimits::default();
    let state = recovery_client
        .tree_state_once(endpoint_index, round_id)
        .await?;

    let mut bundle_by_van = BTreeMap::new();
    for candidate in candidates {
        let leaf = MerkleHashVote::from_bytes(&candidate.van).ok_or_else(|| {
            DelegationVanRecoveryError::Decode(format!(
                "reconstructed VAN for bundle {} is not canonical",
                candidate.bundle_index
            ))
        })?;
        bundle_by_van.insert(leaf, candidate.bundle_index);
    }

    let mut observed_by_bundle = BTreeMap::new();
    if state.next_index == 0 {
        return Ok(observed_by_bundle);
    }

    let mut page_from = 0;
    let mut pages_fetched = 0usize;
    let mut next_position = 0u64;
    loop {
        if cancel() {
            return Err(DelegationVanRecoveryError::Cancelled);
        }
        if pages_fetched >= limits.max_pages {
            return Err(DelegationVanRecoveryError::Decode(format!(
                "vote tree scan exceeded {} pages",
                limits.max_pages
            )));
        }
        let page = recovery_client
            .tree_commitments_once(endpoint_index, round_id, page_from, state.height)
            .await?;
        pages_fetched += 1;

        for block in page.blocks {
            if !block.leaves.is_empty() && block.start_index != next_position {
                return Err(DelegationVanRecoveryError::Decode(format!(
                    "vote tree start_index mismatch at height {}: expected {}, got {}",
                    block.height, next_position, block.start_index
                )));
            }
            for leaf in block.leaves {
                let position = next_position;
                if let Some(&bundle_index) = bundle_by_van.get(&leaf) {
                    let position = u32::try_from(position).map_err(|_| {
                        DelegationVanRecoveryError::Decode(format!(
                            "VAN position {position} for bundle {bundle_index} does not fit in u32"
                        ))
                    })?;
                    observed_by_bundle.entry(bundle_index).or_insert(position);
                }
                next_position = next_position.checked_add(1).ok_or_else(|| {
                    DelegationVanRecoveryError::Decode(
                        "vote tree leaf count overflows u64".to_string(),
                    )
                })?;
            }
        }

        if page.next_from_height == 0 {
            break;
        }
        if page.next_from_height <= page_from || page.next_from_height > state.height {
            return Err(DelegationVanRecoveryError::Decode(format!(
                "invalid vote tree pagination cursor: current={page_from}, next={}",
                page.next_from_height
            )));
        }
        page_from = page.next_from_height;
    }

    if next_position != state.next_index {
        return Err(DelegationVanRecoveryError::Decode(format!(
            "incomplete vote tree scan: local next_index={}, server next_index={}",
            next_position, state.next_index
        )));
    }

    Ok(observed_by_bundle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
        time::Duration,
    };

    use base64::prelude::*;
    use pasta_curves::{group::ff::PrimeField, Fp};
    use serde_json::json;
    use vote_commitment_tree::{
        sync_api::{BlockCommitments, TreeState, TreeSyncApi},
        MemoryTreeServer,
    };

    use crate::{
        phases::DelegationPhase, round::RoundParams, tree_sync::VoteTreeSync, types::Network,
    };

    const WALLET_ID: &str = "delegation-recovery-wallet";

    #[derive(Default)]
    struct MockTransport {
        responses: Mutex<VecDeque<Result<HelperResponse, HelperTransportError>>>,
        gets: Mutex<Vec<String>>,
    }

    impl VoteTreeQueryTransport for MockTransport {
        fn get<'a>(&'a self, url: &'a str, _timeout: Duration) -> VoteTreeQueryFuture<'a> {
            Box::pin(async move {
                self.gets.lock().unwrap().push(url.to_string());
                self.responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("mock response")
            })
        }
    }

    fn response(body: serde_json::Value) -> HelperResponse {
        HelperResponse::new(
            200,
            serde_json::to_vec(&body).unwrap(),
            Some("application/json".to_string()),
        )
    }

    fn tree_state_response(state: &TreeState) -> HelperResponse {
        response(json!({
            "tree": {
                "next_index": state.next_index,
                "root": BASE64_STANDARD.encode(state.root.to_repr()),
                "height": state.height,
            }
        }))
    }

    fn commitments_response(blocks: &[BlockCommitments], next_from_height: u32) -> HelperResponse {
        response(json!({
            "blocks": blocks
                .iter()
                .map(|block| json!({
                    "height": block.height,
                    "start_index": block.start_index,
                    "leaves": block
                        .leaves
                        .iter()
                        .map(|leaf| BASE64_STANDARD.encode(leaf.to_bytes()))
                        .collect::<Vec<_>>(),
                    "root": BASE64_STANDARD.encode(block.root.to_repr()),
                }))
                .collect::<Vec<_>>(),
            "next_from_height": next_from_height,
        }))
    }

    fn round_id() -> String {
        hex::encode(Fp::from(1).to_repr())
    }

    fn round_params() -> RoundParams {
        RoundParams {
            vote_round_id: round_id(),
            snapshot_height: 42,
            ea_pk: Fp::from(2).to_repr().to_vec(),
            nc_root: Fp::from(3).to_repr().to_vec(),
            nullifier_imt_root: Fp::from(4).to_repr().to_vec(),
        }
    }

    fn note(position: u64) -> NoteInfo {
        NoteInfo {
            commitment: Fp::from(position + 10).to_repr().to_vec(),
            nullifier: Fp::from(position + 20).to_repr().to_vec(),
            value: 13_000_000,
            position,
            diversifier: vec![position as u8; 11],
            rho: Fp::from(position + 30).to_repr().to_vec(),
            rseed: Fp::from(position + 40).to_repr().to_vec(),
            scope: 0,
            ufvk_str: String::new(),
        }
    }

    fn db() -> VotingDb {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(WALLET_ID);
        db.init_round(Network::Regtest, &round_params(), None)
            .unwrap();
        db
    }

    fn hotkey() -> VotingHotkey {
        VotingHotkey::from_stored_secret(&[0x43; 64], Network::Regtest).unwrap()
    }

    fn server_with_candidate(candidate: &DelegationVanCandidate) -> MemoryTreeServer {
        let candidate = MerkleHashVote::from_bytes(&candidate.van).unwrap();
        let mut server = MemoryTreeServer::empty();
        server.append(Fp::from(90)).unwrap();
        server.checkpoint(5).unwrap();
        server.append(candidate.inner()).unwrap();
        server.append(Fp::from(91)).unwrap();
        server.checkpoint(7).unwrap();
        server
    }

    fn paginated_responses(server: &MemoryTreeServer) -> Vec<HelperResponse> {
        let state = server.get_tree_state().unwrap();
        let page = server.get_block_commitments(0, state.height).unwrap();
        assert_eq!(page.blocks.len(), 2);
        vec![
            tree_state_response(&state),
            commitments_response(&page.blocks[..1], page.blocks[1].height),
            commitments_response(&page.blocks[1..], 0),
        ]
    }

    fn client_with_responses(
        responses: Vec<HelperResponse>,
    ) -> (DelegationRecoveryClient, Arc<MockTransport>) {
        client_with_endpoints_and_responses(&["https://vote.example".to_string()], responses)
    }

    fn client_with_endpoints_and_responses(
        endpoint_urls: &[String],
        responses: Vec<HelperResponse>,
    ) -> (DelegationRecoveryClient, Arc<MockTransport>) {
        let transport = Arc::new(MockTransport::default());
        transport
            .responses
            .lock()
            .unwrap()
            .extend(responses.into_iter().map(Ok));
        (
            DelegationRecoveryClient::new(transport.clone(), endpoint_urls).unwrap(),
            transport,
        )
    }

    #[test]
    fn recovery_client_rejects_empty_and_duplicate_canonical_endpoints() {
        let transport = Arc::new(MockTransport::default());
        assert!(DelegationRecoveryClient::new(transport.clone(), &[]).is_err());

        let duplicates = [
            "HTTPS://vote.example:443/".to_string(),
            "https://vote.example".to_string(),
        ];
        assert!(DelegationRecoveryClient::new(transport, &duplicates).is_err());
    }

    #[tokio::test]
    async fn fresh_database_recovers_van_and_can_generate_zkp2_witness() {
        let notes = vec![note(0)];
        let voting_hotkey = hotkey();
        let original = db();
        let candidate = reconstruct_candidates(&original, &round_id(), &notes, &voting_hotkey)
            .unwrap()
            .remove(0);
        let server = server_with_candidate(&candidate);
        let (client, transport) = client_with_responses(paginated_responses(&server));

        let recovered_db = db();
        let report = recover_confirmed_delegations(
            &recovered_db,
            &client,
            &round_id(),
            &notes,
            &voting_hotkey,
            &|| false,
        )
        .await
        .unwrap();

        assert_eq!(report, vec![0]);
        assert_eq!(recovered_db.load_van_position(&round_id(), 0).unwrap(), 1);
        assert_eq!(
            recovered_db.delegation_phase(&round_id(), 0).unwrap(),
            DelegationPhase::Confirmed
        );
        assert_eq!(
            recovered_db.get_delegation_tx_hash(&round_id(), 0).unwrap(),
            None
        );
        let state = recovered_db.get_round_state(&round_id()).unwrap();
        assert!(state.proof_generated);
        let zkp2 =
            queries::load_zkp2_inputs(&recovered_db.conn(), &round_id(), WALLET_ID, 0).unwrap();
        assert_eq!(zkp2.gov_comm_rand, candidate.van_comm_rand);
        assert_eq!(zkp2.total_note_value, candidate.total_note_value);

        let tree_sync = VoteTreeSync::new();
        let height = tree_sync
            .sync_with_api(&recovered_db, &round_id(), &server)
            .unwrap();
        let witness = tree_sync
            .generate_van_witness(&recovered_db, &round_id(), 0, height)
            .unwrap();
        assert_eq!(witness.position, 1);
        assert_eq!(witness.anchor_height, 7);

        let gets = transport.gets.lock().unwrap();
        assert_eq!(gets.len(), 3);
        assert!(gets[0].ends_with(&format!(
            "/shielded-vote/v1/commitment-tree/{}/latest",
            round_id()
        )));
        assert!(gets[1].ends_with(&format!(
            "/shielded-vote/v1/commitment-tree/{}/leaves?from_height=0&to_height=7",
            round_id()
        )));
        assert!(gets[2].ends_with(&format!(
            "/shielded-vote/v1/commitment-tree/{}/leaves?from_height=7&to_height=7",
            round_id()
        )));
        drop(gets);

        transport
            .responses
            .lock()
            .unwrap()
            .extend(paginated_responses(&server).into_iter().map(Ok));
        let repeated = recover_confirmed_delegations(
            &recovered_db,
            &client,
            &round_id(),
            &notes,
            &voting_hotkey,
            &|| false,
        )
        .await
        .unwrap();
        assert_eq!(repeated, report);
    }

    #[tokio::test]
    async fn valid_scan_reports_missing_bundle_without_recovery_state() {
        let notes = vec![note(0)];
        let voting_hotkey = hotkey();
        let mut server = MemoryTreeServer::empty();
        server.append(Fp::from(90)).unwrap();
        server.checkpoint(5).unwrap();
        let state = server.get_tree_state().unwrap();
        let page = server.get_block_commitments(0, state.height).unwrap();
        let (client, _) = client_with_responses(vec![
            tree_state_response(&state),
            commitments_response(&page.blocks, 0),
        ]);
        let recovered_db = db();

        let report = recover_confirmed_delegations(
            &recovered_db,
            &client,
            &round_id(),
            &notes,
            &voting_hotkey,
            &|| false,
        )
        .await
        .unwrap();

        assert!(report.is_empty());
        assert_eq!(
            recovered_db.delegation_phase(&round_id(), 0).unwrap(),
            DelegationPhase::Prepared
        );
        assert!(
            queries::load_zkp2_inputs(&recovered_db.conn(), &round_id(), WALLET_ID, 0,).is_err()
        );
    }

    #[tokio::test]
    async fn valid_scan_recovers_only_the_bundles_that_are_present() {
        let notes = (0..6).map(note).collect::<Vec<_>>();
        let voting_hotkey = hotkey();
        let original = db();
        let candidates =
            reconstruct_candidates(&original, &round_id(), &notes, &voting_hotkey).unwrap();
        assert_eq!(candidates.len(), 2);

        let mut server = MemoryTreeServer::empty();
        server
            .append(
                MerkleHashVote::from_bytes(&candidates[0].van)
                    .unwrap()
                    .inner(),
            )
            .unwrap();
        server.checkpoint(5).unwrap();
        let state = server.get_tree_state().unwrap();
        let page = server.get_block_commitments(0, state.height).unwrap();
        let (client, _) = client_with_responses(vec![
            tree_state_response(&state),
            commitments_response(&page.blocks, 0),
        ]);
        let recovered_db = db();

        let report = recover_confirmed_delegations(
            &recovered_db,
            &client,
            &round_id(),
            &notes,
            &voting_hotkey,
            &|| false,
        )
        .await
        .unwrap();

        assert_eq!(report, vec![0]);
        assert_eq!(recovered_db.load_van_position(&round_id(), 0).unwrap(), 0);
        assert_eq!(
            recovered_db.delegation_phase(&round_id(), 0).unwrap(),
            DelegationPhase::Confirmed
        );
        assert_eq!(
            recovered_db.delegation_phase(&round_id(), 1).unwrap(),
            DelegationPhase::Prepared
        );
        assert!(
            !recovered_db
                .get_round_state(&round_id())
                .unwrap()
                .proof_generated
        );
    }

    #[tokio::test]
    async fn invalid_endpoint_falls_back_to_the_next_complete_scan() {
        let notes = vec![note(0)];
        let voting_hotkey = hotkey();
        let original = db();
        let candidate = reconstruct_candidates(&original, &round_id(), &notes, &voting_hotkey)
            .unwrap()
            .remove(0);
        let server = server_with_candidate(&candidate);
        let state = server.get_tree_state().unwrap();
        let page = server.get_block_commitments(0, state.height).unwrap();
        let (client, transport) = client_with_endpoints_and_responses(
            &[
                "https://first.example".to_string(),
                "https://second.example".to_string(),
            ],
            vec![
                HelperResponse::new(500, b"{}".to_vec(), Some("application/json".to_string())),
                tree_state_response(&state),
                commitments_response(&page.blocks, 0),
            ],
        );
        let recovered_db = db();

        let report = recover_confirmed_delegations(
            &recovered_db,
            &client,
            &round_id(),
            &notes,
            &voting_hotkey,
            &|| false,
        )
        .await
        .unwrap();

        assert_eq!(report, vec![0]);
        let gets = transport.gets.lock().unwrap();
        assert!(gets[0].starts_with("https://first.example/"));
        assert!(gets[1].starts_with("https://second.example/"));
        assert!(gets[2].starts_with("https://second.example/"));
    }

    #[tokio::test]
    async fn cancellation_after_scan_writes_no_recovered_state() {
        let notes = vec![note(0)];
        let voting_hotkey = hotkey();
        let original = db();
        let candidate = reconstruct_candidates(&original, &round_id(), &notes, &voting_hotkey)
            .unwrap()
            .remove(0);
        let server = server_with_candidate(&candidate);
        let state = server.get_tree_state().unwrap();
        let page = server.get_block_commitments(0, state.height).unwrap();
        let (client, _) = client_with_responses(vec![
            tree_state_response(&state),
            commitments_response(&page.blocks, 0),
        ]);
        let recovered_db = db();
        let cancellation_checks = AtomicUsize::new(0);

        let error = recover_confirmed_delegations(
            &recovered_db,
            &client,
            &round_id(),
            &notes,
            &voting_hotkey,
            &|| cancellation_checks.fetch_add(1, Ordering::SeqCst) >= 2,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, DelegationVanRecoveryError::Cancelled));
        assert_eq!(
            recovered_db.delegation_phase(&round_id(), 0).unwrap(),
            DelegationPhase::Prepared
        );
    }

    #[tokio::test]
    async fn discovered_position_must_pass_normal_tree_sync_before_zkp2() {
        let notes = vec![note(0)];
        let voting_hotkey = hotkey();
        let original = db();
        let candidate = reconstruct_candidates(&original, &round_id(), &notes, &voting_hotkey)
            .unwrap()
            .remove(0);
        let server = server_with_candidate(&candidate);
        let state = server.get_tree_state().unwrap();
        let mut page = server.get_block_commitments(0, state.height).unwrap();
        let candidate_leaf = MerkleHashVote::from_bytes(&candidate.van).unwrap();
        let other_leaf = MerkleHashVote::from_bytes(&Fp::from(90).to_repr()).unwrap();
        page.blocks[0].leaves[0] = candidate_leaf;
        page.blocks[1].leaves[0] = other_leaf;
        let (client, _) = client_with_responses(vec![
            tree_state_response(&state),
            commitments_response(&page.blocks, 0),
        ]);
        let recovered_db = db();

        let report = recover_confirmed_delegations(
            &recovered_db,
            &client,
            &round_id(),
            &notes,
            &voting_hotkey,
            &|| false,
        )
        .await
        .unwrap();

        assert_eq!(report, vec![0]);
        assert_eq!(recovered_db.load_van_position(&round_id(), 0).unwrap(), 0);
        let error = VoteTreeSync::new()
            .sync_with_api(&recovered_db, &round_id(), &server)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not match its synced vote-tree leaf"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn conflicting_local_state_rolls_back_recovered_batch() {
        let notes = (0..6).map(note).collect::<Vec<_>>();
        let voting_hotkey = hotkey();
        let original = db();
        let candidates =
            reconstruct_candidates(&original, &round_id(), &notes, &voting_hotkey).unwrap();
        assert_eq!(candidates.len(), 2);
        let mut server = MemoryTreeServer::empty();
        for candidate in &candidates {
            server
                .append(MerkleHashVote::from_bytes(&candidate.van).unwrap().inner())
                .unwrap();
        }
        server.checkpoint(5).unwrap();
        let state = server.get_tree_state().unwrap();
        let page = server.get_block_commitments(0, state.height).unwrap();
        let (client, _) = client_with_responses(vec![
            tree_state_response(&state),
            commitments_response(&page.blocks, 0),
        ]);
        let recovered_db = db();
        recovered_db
            .ensure_bundles_with_policy(&round_id(), &notes, recoverable_bundle_policy_v1())
            .unwrap();
        recovered_db
            .conn()
            .execute(
                "UPDATE bundles SET van_comm_rand = ?1
                 WHERE round_id = ?2 AND wallet_id = ?3 AND bundle_index = 1",
                rusqlite::params![vec![0xFF_u8; 32], round_id(), WALLET_ID],
            )
            .unwrap();

        let error = recover_confirmed_delegations(
            &recovered_db,
            &client,
            &round_id(),
            &notes,
            &voting_hotkey,
            &|| false,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("conflicts with stored state"));
        assert!(recovered_db.load_van_position(&round_id(), 0).is_err());
        assert!(recovered_db.load_van_position(&round_id(), 1).is_err());
    }
}
