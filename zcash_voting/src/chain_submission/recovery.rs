//! Streaming exact commitment-tree recovery for sticky bound generations.

use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use incrementalmerkletree::frontier::Frontier;
use serde::Deserialize;
use vote_commitment_tree::{MerkleHashVote, TREE_DEPTH};

use super::{
    coordination::{CapturedSubmissionOperation, SubmissionOperationLease},
    generation::DerivedChainSubmission,
    protocol::ChainProtocolClient,
    CandidateTransactionHash, ChainHttpRequest, ChainSubmissionDiagnostic,
    ChainSubmissionDiagnosticKind, ChainTransport, ChainTransportError,
};

const MAX_RECOVERY_LEAVES: u64 = 1 << TREE_DEPTH;
const MAX_RECOVERY_REQUESTS: usize = 4_096;
const MAX_LEAVES_PER_PAGE: usize = 4_096;
const MAX_RECOVERY_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_RECOVERY_TOTAL_BYTES: u64 = 32 * 1024 * 1024 * 1024;
const RECOVERY_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const RECOVERY_PASS_TIMEOUT: Duration = Duration::from_secs(72 * 60 * 60);

#[derive(Debug)]
pub(super) enum RecoveryScanFailure {
    Transport(ChainTransportError),
    Invalid(ChainSubmissionDiagnostic),
    Interrupted,
}

pub(super) enum RecoveryScanOutcome<'a> {
    Match {
        final_van_position: u64,
        vote_commitment_positions: Vec<u64>,
    },
    NoMatch(RecoveryRetryAuthorization<'a>),
}

/// Single-use proof that one continuously locked pass scanned a complete fixed
/// snapshot without finding the generation's exact output layout.
pub(super) struct RecoveryRetryAuthorization<'a> {
    operation: &'a CapturedSubmissionOperation,
    _lease: &'a SubmissionOperationLease,
    generation_digest: super::ChainSubmissionGenerationDigest,
    candidate: Option<CandidateTransactionHash>,
}

impl RecoveryRetryAuthorization<'_> {
    pub(super) fn operation(&self) -> &CapturedSubmissionOperation {
        self.operation
    }

    pub(super) fn generation_digest(&self) -> super::ChainSubmissionGenerationDigest {
        self.generation_digest
    }

    pub(super) fn candidate(&self) -> Option<CandidateTransactionHash> {
        self.candidate
    }
}

#[derive(Deserialize)]
struct LatestResponse {
    tree: Option<TreeState>,
}

#[derive(Deserialize)]
struct TreeState {
    next_index: u64,
    root: Option<String>,
    height: u64,
}

#[derive(Deserialize)]
struct LeavesResponse {
    #[serde(default)]
    blocks: Vec<LeafBlock>,
    #[serde(default)]
    next_from_height: u64,
}

#[derive(Deserialize)]
struct LeafBlock {
    height: u64,
    start_index: u64,
    #[serde(default)]
    leaves: Vec<String>,
    root: Option<String>,
}

pub(super) async fn scan_exact_layout<'a, T: ChainTransport>(
    protocol: &ChainProtocolClient<T>,
    derived: &DerivedChainSubmission,
    candidate: Option<CandidateTransactionHash>,
    operation: &'a CapturedSubmissionOperation,
    lease: &'a SubmissionOperationLease,
    interrupted: impl Fn() -> bool,
) -> Result<RecoveryScanOutcome<'a>, RecoveryScanFailure> {
    let started = Instant::now();
    let endpoint = protocol.endpoints().first().ok_or_else(|| {
        RecoveryScanFailure::Invalid(invalid("tree recovery has no configured endpoint"))
    })?;
    let round = hex::encode(derived.generation().identity().vote_round_id());
    let latest_url = format!("{endpoint}/shielded-vote/v1/commitment-tree/{round}/latest");
    let (latest, latest_bytes): (LatestResponse, usize) =
        get_json_with_size(protocol.transport(), latest_url, &interrupted).await?;
    let snapshot = latest.tree.ok_or_else(|| {
        RecoveryScanFailure::Invalid(invalid("tree recovery latest response omitted tree state"))
    })?;
    if snapshot.next_index > MAX_RECOVERY_LEAVES || snapshot.height > u32::MAX as u64 {
        return Err(RecoveryScanFailure::Invalid(invalid(
            "tree recovery snapshot exceeds protocol bounds",
        )));
    }
    let snapshot_root = match snapshot.root.as_deref() {
        Some(value) if !value.is_empty() => Some(decode_leaf(value, "snapshot root")?),
        None | Some("") if snapshot.next_index == 0 => None,
        _ => {
            return Err(RecoveryScanFailure::Invalid(invalid(
                "nonempty tree recovery snapshot omitted its root",
            )))
        }
    };
    let expected = derived.expected_layout().leaves();
    let mut frontier: Frontier<MerkleHashVote, { TREE_DEPTH as u8 }> = Frontier::empty();
    let mut window: std::collections::VecDeque<[u8; 32]> =
        std::collections::VecDeque::with_capacity(expected.len());
    let mut next_index = 0_u64;
    let mut from_height = 0_u64;
    let mut previous_height = None;
    let mut leaf_request_count = 0_usize;
    let mut total_bytes = latest_bytes as u64;
    let mut match_start = None;

    while next_index < snapshot.next_index {
        if interrupted() {
            return Err(RecoveryScanFailure::Interrupted);
        }
        if leaf_request_count >= MAX_RECOVERY_REQUESTS || started.elapsed() > RECOVERY_PASS_TIMEOUT
        {
            return Err(RecoveryScanFailure::Invalid(invalid(
                "tree recovery exhausted its bounded pass",
            )));
        }
        let url = format!(
            "{endpoint}/shielded-vote/v1/commitment-tree/{round}/leaves?from_height={from_height}&to_height={}",
            snapshot.height
        );
        let (page, bytes): (LeavesResponse, usize) =
            get_json_with_size(protocol.transport(), url, &interrupted).await?;
        leaf_request_count += 1;
        total_bytes = total_bytes.saturating_add(bytes as u64);
        if total_bytes > MAX_RECOVERY_TOTAL_BYTES || page.blocks.is_empty() {
            return Err(RecoveryScanFailure::Invalid(invalid(
                "tree recovery pagination is incomplete or oversized",
            )));
        }
        let page_leaf_count: usize = page.blocks.iter().map(|block| block.leaves.len()).sum();
        if page_leaf_count > MAX_LEAVES_PER_PAGE {
            return Err(RecoveryScanFailure::Invalid(invalid(
                "tree recovery page exceeds the leaf limit",
            )));
        }
        for block in page.blocks {
            if block.height > snapshot.height
                || previous_height.is_some_and(|height| block.height <= height)
                || block.start_index != next_index
            {
                return Err(RecoveryScanFailure::Invalid(invalid(
                    "tree recovery block sequence is discontinuous",
                )));
            }
            previous_height = Some(block.height);
            for encoded in block.leaves {
                if next_index >= snapshot.next_index
                    || !frontier.append(decode_leaf(&encoded, "tree leaf")?)
                {
                    return Err(RecoveryScanFailure::Invalid(invalid(
                        "tree recovery leaf sequence exceeds the fixed snapshot",
                    )));
                }
                let bytes = BASE64_STANDARD.decode(encoded).map_err(|_| {
                    RecoveryScanFailure::Invalid(invalid("tree recovery leaf is invalid base64"))
                })?;
                window.push_back(bytes.try_into().map_err(|_| {
                    RecoveryScanFailure::Invalid(invalid("tree recovery leaf has invalid length"))
                })?);
                if window.len() > expected.len() {
                    window.pop_front();
                }
                next_index += 1;
                if window.len() == expected.len()
                    && window.iter().copied().eq(expected.iter().copied())
                {
                    let start = next_index - expected.len() as u64;
                    if match_start.replace(start).is_some() {
                        return Err(RecoveryScanFailure::Invalid(invalid(
                            "tree recovery found multiple exact generation layouts",
                        )));
                    }
                }
            }
            let block_root = decode_leaf(
                block.root.as_deref().ok_or_else(|| {
                    RecoveryScanFailure::Invalid(invalid("tree recovery block omitted its root"))
                })?,
                "block root",
            )?;
            if frontier.root() != block_root {
                return Err(RecoveryScanFailure::Invalid(invalid(
                    "tree recovery block root contradicts its leaves",
                )));
            }
        }
        if next_index < snapshot.next_index {
            if page.next_from_height <= from_height || page.next_from_height > snapshot.height {
                return Err(RecoveryScanFailure::Invalid(invalid(
                    "tree recovery pagination cursor is invalid",
                )));
            }
            from_height = page.next_from_height;
        } else if page.next_from_height != 0 {
            return Err(RecoveryScanFailure::Invalid(invalid(
                "tree recovery pagination continued beyond the fixed snapshot",
            )));
        }
    }
    if started.elapsed() > RECOVERY_PASS_TIMEOUT {
        return Err(RecoveryScanFailure::Invalid(invalid(
            "tree recovery exhausted its bounded pass",
        )));
    }
    if snapshot_root.is_some_and(|root| frontier.root() != root) {
        return Err(RecoveryScanFailure::Invalid(invalid(
            "tree recovery final root contradicts the fixed snapshot",
        )));
    }
    if let Some(start) = match_start {
        return Ok(RecoveryScanOutcome::Match {
            final_van_position: start,
            vote_commitment_positions: (1..expected.len())
                .map(|offset| start + offset as u64)
                .collect(),
        });
    }
    Ok(RecoveryScanOutcome::NoMatch(RecoveryRetryAuthorization {
        operation,
        _lease: lease,
        generation_digest: derived.generation().digest(),
        candidate,
    }))
}

async fn get_json_with_size<T: ChainTransport, R: for<'de> Deserialize<'de>>(
    transport: &T,
    url: String,
    interrupted: &impl Fn() -> bool,
) -> Result<(R, usize), RecoveryScanFailure> {
    if interrupted() {
        return Err(RecoveryScanFailure::Interrupted);
    }
    let request = ChainHttpRequest::new(
        url,
        vec![("accept".to_string(), "application/json".to_string())],
        RECOVERY_REQUEST_TIMEOUT,
        MAX_RECOVERY_RESPONSE_BYTES,
    );
    let response = tokio::time::timeout(RECOVERY_REQUEST_TIMEOUT, transport.chain_get(request))
        .await
        .map_err(|_| {
            RecoveryScanFailure::Transport(ChainTransportError::possibly_dispatched(
                "tree recovery request timed out",
            ))
        })?
        .map_err(RecoveryScanFailure::Transport)?;
    if response.status() != 200
        || response.body().len() > MAX_RECOVERY_RESPONSE_BYTES
        || response
            .content_type()
            .is_none_or(|value| value.split(';').next() != Some("application/json"))
    {
        return Err(RecoveryScanFailure::Invalid(invalid(
            "tree recovery response has invalid HTTP metadata",
        )));
    }
    let size = response.body().len();
    serde_json::from_slice(response.body())
        .map(|value| (value, size))
        .map_err(|_| RecoveryScanFailure::Invalid(invalid("tree recovery response is malformed")))
}

fn decode_leaf(encoded: &str, label: &str) -> Result<MerkleHashVote, RecoveryScanFailure> {
    let bytes = BASE64_STANDARD
        .decode(encoded)
        .map_err(|_| RecoveryScanFailure::Invalid(invalid(format!("{label} is invalid base64"))))?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
        RecoveryScanFailure::Invalid(invalid(format!("{label} has invalid length")))
    })?;
    MerkleHashVote::from_bytes(&bytes)
        .ok_or_else(|| RecoveryScanFailure::Invalid(invalid(format!("{label} is noncanonical"))))
}

fn invalid(message: impl AsRef<str>) -> ChainSubmissionDiagnostic {
    ChainSubmissionDiagnostic::from_redacted_message(
        ChainSubmissionDiagnosticKind::InvalidProtocolResponse,
        message,
    )
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use incrementalmerkletree::frontier::Frontier;

    use super::*;
    use crate::{
        chain_submission::{
            coordination::SubmissionCoordination,
            generation::{ChainSubmissionRequest, ExpectedTreeLayout},
            ChainHttpResponse, ChainSubmissionGeneration, ChainSubmissionGenerationDigest,
            ChainSubmissionIdentity, ChainSubmissionTarget, ChainTransportFuture,
        },
        types::Network,
        wire::VoteCommitmentWire,
    };

    struct ScriptedTreeTransport {
        replies: Mutex<VecDeque<Result<ChainHttpResponse, ChainTransportError>>>,
        urls: Mutex<Vec<String>>,
    }

    impl ScriptedTreeTransport {
        fn new(replies: Vec<ChainHttpResponse>) -> Self {
            Self {
                replies: Mutex::new(replies.into_iter().map(Ok).collect()),
                urls: Mutex::new(vec![]),
            }
        }
    }

    impl ChainTransport for ScriptedTreeTransport {
        fn chain_get<'a>(&'a self, request: ChainHttpRequest) -> ChainTransportFuture<'a> {
            Box::pin(async move {
                self.urls.lock().unwrap().push(request.url().to_string());
                self.replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("scripted tree response")
            })
        }

        fn chain_post_json<'a>(
            &'a self,
            _request: ChainHttpRequest,
            _json: Vec<u8>,
        ) -> ChainTransportFuture<'a> {
            Box::pin(async { panic!("scanner must not POST") })
        }
    }

    fn derived() -> DerivedChainSubmission {
        let identity = ChainSubmissionIdentity::new(
            "wallet",
            Network::Testnet,
            "chain",
            [1; 32],
            0,
            ChainSubmissionTarget::Vote { proposal_id: 7 },
        )
        .unwrap();
        DerivedChainSubmission::new(
            ChainSubmissionGeneration::new(
                identity.clone(),
                ChainSubmissionGenerationDigest::from_bytes([9; 32]),
            ),
            ChainSubmissionRequest::Vote(VoteCommitmentWire {
                van_nullifier: hex::encode([2; 32]),
                vote_authority_note_new: hex::encode([3; 32]),
                vote_commitment: hex::encode([4; 32]),
                proposal_id: 7,
                proof: hex::encode([5; 8]),
                vote_round_id: hex::encode(identity.vote_round_id()),
                anchor_height: 1,
                r_vpk: hex::encode([6; 32]),
                vote_auth_sig: hex::encode([7; 64]),
            }),
            ExpectedTreeLayout::Vote {
                successor_van: [3; 32],
                vote_commitment: [4; 32],
            },
            vec![7],
        )
    }

    fn tree_responses(leaves: &[[u8; 32]]) -> Vec<ChainHttpResponse> {
        if leaves.is_empty() {
            return vec![ChainHttpResponse::json(
                200,
                br#"{"tree":{"next_index":0,"height":0}}"#.to_vec(),
            )];
        }
        let mut frontier: Frontier<MerkleHashVote, { TREE_DEPTH as u8 }> = Frontier::empty();
        for leaf in leaves {
            assert!(frontier.append(MerkleHashVote::from_bytes(leaf).unwrap()));
        }
        let root = BASE64_STANDARD.encode(frontier.root().to_bytes());
        let encoded = leaves
            .iter()
            .map(|leaf| BASE64_STANDARD.encode(leaf))
            .collect::<Vec<_>>();
        vec![
            ChainHttpResponse::json(
                200,
                serde_json::to_vec(&serde_json::json!({
                    "tree": { "next_index": leaves.len(), "root": root, "height": 1 }
                }))
                .unwrap(),
            ),
            ChainHttpResponse::json(
                200,
                serde_json::to_vec(&serde_json::json!({
                    "blocks": [{
                        "height": 1, "start_index": 0, "leaves": encoded, "root": root
                    }],
                    "next_from_height": 0
                }))
                .unwrap(),
            ),
        ]
    }

    async fn scan(
        leaves: &[[u8; 32]],
        candidate: Option<CandidateTransactionHash>,
    ) -> Result<(RecoveryScanOutcome<'static>, Vec<String>), RecoveryScanFailure> {
        scan_responses(tree_responses(leaves), candidate).await
    }

    async fn scan_responses(
        responses: Vec<ChainHttpResponse>,
        candidate: Option<CandidateTransactionHash>,
    ) -> Result<(RecoveryScanOutcome<'static>, Vec<String>), RecoveryScanFailure> {
        // The test-owned operation and lease are leaked so the returned
        // authorization can be inspected without weakening its production lifetime.
        let derived = Box::leak(Box::new(derived()));
        let operation = Box::leak(Box::new(CapturedSubmissionOperation::new(
            derived.generation().identity().clone(),
            11,
        )));
        let coordination = Box::leak(Box::new(SubmissionCoordination::default()));
        let lease = Box::leak(Box::new(
            coordination
                .acquire(operation, &[operation.identity().clone()])
                .await
                .unwrap(),
        ));
        let transport = ScriptedTreeTransport::new(responses);
        let protocol = ChainProtocolClient::new(
            transport,
            Network::Testnet,
            &["https://chain.example".to_string()],
        )
        .unwrap();
        let outcome =
            scan_exact_layout(&protocol, derived, candidate, operation, lease, || false).await?;
        let urls = protocol.transport().urls.lock().unwrap().clone();
        Ok((outcome, urls))
    }

    #[tokio::test]
    async fn exact_vote_layout_recovers_adjacent_ordered_positions() {
        let (outcome, urls) = scan(&[[8; 32], [3; 32], [4; 32], [7; 32]], None)
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            RecoveryScanOutcome::Match {
                final_van_position: 1,
                ref vote_commitment_positions
            } if vote_commitment_positions == &[2]
        ));
        assert_eq!(urls.len(), 2);
        assert!(urls[0].ends_with(&format!("/{}/latest", hex::encode([1; 32]))));
    }

    #[tokio::test]
    async fn complete_no_match_authorizes_only_the_captured_generation_and_candidate() {
        let candidate = CandidateTransactionHash::from_bytes([10; 32]);
        let (outcome, _) = scan(&[[3; 32], [8; 32], [4; 32]], Some(candidate))
            .await
            .unwrap();
        let RecoveryScanOutcome::NoMatch(authorization) = outcome else {
            panic!("nonadjacent leaves must not match")
        };
        assert_eq!(authorization.operation().host_operation_epoch(), 11);
        assert_eq!(authorization.generation_digest().as_bytes(), &[9; 32]);
        assert_eq!(authorization.candidate(), Some(candidate));
    }

    #[tokio::test]
    async fn empty_snapshot_is_a_complete_no_match_without_a_leaves_request() {
        let (outcome, urls) = scan(&[], None).await.unwrap();
        assert!(matches!(outcome, RecoveryScanOutcome::NoMatch(_)));
        assert_eq!(urls.len(), 1);
    }

    #[tokio::test]
    async fn duplicate_complete_layout_is_ambiguous_not_confirmation() {
        let failure = scan(&[[3; 32], [4; 32], [8; 32], [3; 32], [4; 32]], None)
            .await
            .err()
            .expect("duplicate layout is invalid");
        assert!(matches!(failure, RecoveryScanFailure::Invalid(_)));
    }

    #[tokio::test]
    async fn incomplete_pagination_produces_no_authorization() {
        let leaves = [[8; 32], [9; 32]];
        let mut frontier: Frontier<MerkleHashVote, { TREE_DEPTH as u8 }> = Frontier::empty();
        for leaf in &leaves {
            assert!(frontier.append(MerkleHashVote::from_bytes(leaf).unwrap()));
        }
        let final_root = BASE64_STANDARD.encode(frontier.root().to_bytes());
        let mut partial: Frontier<MerkleHashVote, { TREE_DEPTH as u8 }> = Frontier::empty();
        assert!(partial.append(MerkleHashVote::from_bytes(&leaves[0]).unwrap()));
        let responses = vec![
            ChainHttpResponse::json(
                200,
                serde_json::to_vec(&serde_json::json!({
                    "tree": { "next_index": 2, "root": final_root, "height": 1 }
                }))
                .unwrap(),
            ),
            ChainHttpResponse::json(
                200,
                serde_json::to_vec(&serde_json::json!({
                    "blocks": [{
                        "height": 1,
                        "start_index": 0,
                        "leaves": [BASE64_STANDARD.encode(leaves[0])],
                        "root": BASE64_STANDARD.encode(partial.root().to_bytes())
                    }],
                    "next_from_height": 0
                }))
                .unwrap(),
            ),
        ];
        let failure = scan_responses(responses, None)
            .await
            .err()
            .expect("incomplete pagination is not evidence");
        assert!(matches!(failure, RecoveryScanFailure::Invalid(_)));
    }

    #[test]
    fn full_tree_capacity_fits_the_fixed_request_and_byte_ceilings() {
        assert_eq!(MAX_RECOVERY_LEAVES, 16_777_216);
        assert_eq!(
            MAX_RECOVERY_LEAVES.div_ceil(MAX_LEAVES_PER_PAGE as u64),
            MAX_RECOVERY_REQUESTS as u64
        );
        assert_eq!(
            MAX_RECOVERY_REQUESTS as u64 * MAX_RECOVERY_RESPONSE_BYTES as u64,
            MAX_RECOVERY_TOTAL_BYTES
        );
        assert!(
            MAX_RECOVERY_REQUESTS as u64 * RECOVERY_REQUEST_TIMEOUT.as_secs()
                <= RECOVERY_PASS_TIMEOUT.as_secs()
        );
    }
}
