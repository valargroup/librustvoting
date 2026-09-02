use super::*;

#[tokio::test]
async fn empty_checkpoint_ignores_start_index_after_earlier_leaves() {
    let leaves = [[3; 32], [4; 32]];
    let mut frontier: Frontier<MerkleHashVote, { TREE_DEPTH as u8 }> = Frontier::empty();
    for leaf in &leaves {
        assert!(frontier.append(MerkleHashVote::from_bytes(leaf).unwrap()));
    }
    let root = BASE64_STANDARD.encode(frontier.root().to_bytes());
    let responses = vec![
        ChainHttpResponse::json(
            200,
            serde_json::to_vec(&serde_json::json!({
                "tree": { "next_index": 2, "root": root, "height": 2 }
            }))
            .unwrap(),
        ),
        ChainHttpResponse::json(
            200,
            serde_json::to_vec(&serde_json::json!({
                "blocks": [
                    {
                        "height": 1,
                        "start_index": 0,
                        "leaves": leaves.map(|leaf| BASE64_STANDARD.encode(leaf)),
                        "root": root
                    },
                    {
                        "height": 2,
                        "start_index": 0,
                        "leaves": [],
                        "root": root
                    }
                ],
                "next_from_height": 0
            }))
            .unwrap(),
        ),
    ];

    let (outcome, _) = scan_responses(responses, None).await.unwrap();

    assert!(matches!(
        outcome,
        RecoveryScanOutcome::Match {
            final_van_position: 0,
            vote_commitment_positions
        } if vote_commitment_positions == vec![1]
    ));
}

#[tokio::test]
async fn nonempty_block_with_discontinuous_start_index_is_rejected() {
    let leaves = [[3; 32], [4; 32]];
    let mut frontier: Frontier<MerkleHashVote, { TREE_DEPTH as u8 }> = Frontier::empty();
    for leaf in &leaves {
        assert!(frontier.append(MerkleHashVote::from_bytes(leaf).unwrap()));
    }
    let root = BASE64_STANDARD.encode(frontier.root().to_bytes());
    let responses = vec![
        ChainHttpResponse::json(
            200,
            serde_json::to_vec(&serde_json::json!({
                "tree": { "next_index": 2, "root": root, "height": 1 }
            }))
            .unwrap(),
        ),
        ChainHttpResponse::json(
            200,
            serde_json::to_vec(&serde_json::json!({
                "blocks": [{
                    "height": 1,
                    "start_index": 1,
                    "leaves": leaves.map(|leaf| BASE64_STANDARD.encode(leaf)),
                    "root": root
                }],
                "next_from_height": 0
            }))
            .unwrap(),
        ),
    ];

    let failure = scan_responses(responses, None)
        .await
        .err()
        .expect("nonempty blocks must identify their first leaf");

    assert!(matches!(failure, RecoveryScanFailure::Invalid(_)));
}

#[tokio::test]
async fn empty_checkpoint_with_contradictory_root_is_rejected() {
    let leaves = [[3; 32], [4; 32]];
    let mut frontier: Frontier<MerkleHashVote, { TREE_DEPTH as u8 }> = Frontier::empty();
    for leaf in &leaves {
        assert!(frontier.append(MerkleHashVote::from_bytes(leaf).unwrap()));
    }
    let root = BASE64_STANDARD.encode(frontier.root().to_bytes());
    let contradictory_root = BASE64_STANDARD.encode([8; 32]);
    let responses = vec![
        ChainHttpResponse::json(
            200,
            serde_json::to_vec(&serde_json::json!({
                "tree": { "next_index": 2, "root": root, "height": 2 }
            }))
            .unwrap(),
        ),
        ChainHttpResponse::json(
            200,
            serde_json::to_vec(&serde_json::json!({
                "blocks": [
                    {
                        "height": 1,
                        "start_index": 0,
                        "leaves": leaves.map(|leaf| BASE64_STANDARD.encode(leaf)),
                        "root": root
                    },
                    {
                        "height": 2,
                        "start_index": 0,
                        "leaves": [],
                        "root": contradictory_root
                    }
                ],
                "next_from_height": 0
            }))
            .unwrap(),
        ),
    ];

    let failure = scan_responses(responses, None)
        .await
        .err()
        .expect("empty checkpoints must preserve the current root");

    assert!(matches!(failure, RecoveryScanFailure::Invalid(_)));
}

#[tokio::test]
async fn cursor_progressing_empty_page_is_accepted() {
    let leaves = [[3; 32], [4; 32]];
    let mut frontier: Frontier<MerkleHashVote, { TREE_DEPTH as u8 }> = Frontier::empty();
    for leaf in &leaves {
        assert!(frontier.append(MerkleHashVote::from_bytes(leaf).unwrap()));
    }
    let root = BASE64_STANDARD.encode(frontier.root().to_bytes());
    let responses = vec![
        ChainHttpResponse::json(
            200,
            serde_json::to_vec(&serde_json::json!({
                "tree": { "next_index": 2, "root": root, "height": 2 }
            }))
            .unwrap(),
        ),
        ChainHttpResponse::json(200, br#"{"blocks":[],"next_from_height":1}"#.to_vec()),
        ChainHttpResponse::json(
            200,
            serde_json::to_vec(&serde_json::json!({
                "blocks": [{
                    "height": 2,
                    "start_index": 0,
                    "leaves": leaves.map(|leaf| BASE64_STANDARD.encode(leaf)),
                    "root": root
                }],
                "next_from_height": 0
            }))
            .unwrap(),
        ),
    ];

    let (outcome, urls) = scan_responses(responses, None).await.unwrap();

    assert!(matches!(
        outcome,
        RecoveryScanOutcome::Match {
            final_van_position: 0,
            vote_commitment_positions
        } if vote_commitment_positions == vec![1]
    ));
    assert!(urls[1].contains("from_height=0&to_height=2"));
    assert!(urls[2].contains("from_height=1&to_height=2"));
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
