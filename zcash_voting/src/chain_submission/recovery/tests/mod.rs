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
        return vec![ChainHttpResponse::json(200, br#"{"tree":{}}"#.to_vec())];
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
                    "height": 1, "leaves": encoded, "root": root
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

mod http_metadata;
mod layout;
mod pagination;
