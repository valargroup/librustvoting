use super::*;

#[tokio::test]
async fn constructs_exact_atomic_vote_batch_url_and_json() {
    let transport = Arc::new(ScriptedTransport::default());
    let batch_digest = [9; 32];
    transport.queue(Ok(json(
        200,
        format!(
            r#"{{"tx_hash":"{HASH}","code":0,"batch_digest":"{}"}}"#,
            hex::encode(batch_digest)
        ),
    )));
    let client = protocol_client(
        transport.clone(),
        Network::Testnet,
        &["https://vote.example"],
    );
    let wire = crate::wire::VoteCommitmentBatchWire {
        votes: vec![vote(), vote()],
    };

    assert!(matches!(
        client
            .submit_vote_batch_with_dispatch(0, &wire, batch_digest, ChainPostDispatch::default(),)
            .await,
        PostAttemptOutcome::Accepted(_)
    ));
    let calls = transport.calls.lock().unwrap();
    assert_eq!(
        calls[0].1.url(),
        "https://vote.example/shielded-vote/v1/cast-vote-batch"
    );
    assert_eq!(calls[0].2, wire.to_json().unwrap().as_bytes());
}

#[tokio::test]
async fn accepts_rejection_with_matching_server_digest() {
    let transport = Arc::new(ScriptedTransport::default());
    let batch_digest = [9; 32];
    transport.queue(Ok(json(
        422,
        format!(
            r#"{{"tx_hash":"{HASH}","code":7,"log":"round closed","batch_digest":"{}"}}"#,
            hex::encode(batch_digest)
        ),
    )));
    let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);
    let wire = crate::wire::VoteCommitmentBatchWire {
        votes: vec![vote(), vote()],
    };

    assert!(matches!(
        client
            .submit_vote_batch_with_dispatch(0, &wire, batch_digest, ChainPostDispatch::default(),)
            .await,
        PostAttemptOutcome::Rejected {
            code: 7,
            candidate_transaction_hash: Some(_),
            ..
        }
    ));
}

#[tokio::test]
async fn rejects_missing_noncanonical_or_mismatched_server_digest() {
    let expected_batch_digest = [0xab; 32];
    let responses = [
        format!(r#"{{"tx_hash":"{HASH}","code":0}}"#),
        format!(
            r#"{{"tx_hash":"{HASH}","code":0,"batch_digest":"{}"}}"#,
            hex::encode(expected_batch_digest).to_ascii_uppercase()
        ),
        format!(
            r#"{{"tx_hash":"{HASH}","code":0,"batch_digest":"{}"}}"#,
            hex::encode([8; 32])
        ),
    ];

    for response in responses {
        let transport = Arc::new(ScriptedTransport::default());
        transport.queue(Ok(json(200, response)));
        let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);
        let wire = crate::wire::VoteCommitmentBatchWire {
            votes: vec![vote(), vote()],
        };

        assert!(matches!(
            client
                .submit_vote_batch_with_dispatch(
                    0,
                    &wire,
                    expected_batch_digest,
                    ChainPostDispatch::default(),
                )
                .await,
            PostAttemptOutcome::PossiblyDispatched(_)
        ));
    }
}
