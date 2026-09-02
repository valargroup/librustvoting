use super::*;

#[tokio::test]
async fn constructs_exact_atomic_vote_batch_url_and_json() {
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(json(200, format!(r#"{{"tx_hash":"{HASH}","code":0}}"#))));
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
            .submit_vote_batch_with_dispatch(0, &wire, ChainPostDispatch::default())
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
