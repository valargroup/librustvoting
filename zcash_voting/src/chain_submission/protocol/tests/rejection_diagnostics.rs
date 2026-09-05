//! How a deterministic chain rejection, and an answer that never reached the
//! chain at all, become a durable diagnostic.

use super::*;

#[tokio::test]
async fn valid_422_is_a_deterministic_rejection_with_optional_hash() {
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(json(
        422,
        format!(r#"{{"tx_hash":"{HASH}","code":7,"log":"round closed"}}"#),
    )));
    let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);

    let outcome = client.submit_delegation(0, &delegation()).await;
    let PostAttemptOutcome::Rejected {
        code,
        kind,
        diagnostic,
        candidate_transaction_hash,
    } = outcome
    else {
        panic!("expected deterministic rejection");
    };
    assert_eq!(code, 7);
    assert_eq!(kind, ChainRejectionKind::Other);
    assert_eq!(
        diagnostic.kind(),
        ChainSubmissionDiagnosticKind::ChainRejected
    );
    assert_eq!(
        candidate_transaction_hash,
        Some(CandidateTransactionHash::from_str(HASH).unwrap())
    );
    // The chain's own words are what make a rejection actionable.
    assert_eq!(
        diagnostic.message(),
        "vote chain rejected transaction with code 7: round closed"
    );
}

#[tokio::test]
async fn rejection_diagnostic_carries_the_server_log_escaped_and_bounded() {
    let transport = Arc::new(ScriptedTransport::default());
    // Server-controlled text, including anything it echoes back from the
    // request, is surfaced rather than dropped: an operator cannot act on
    // a bare code. It stays data — escaped, bounded, never interpreted.
    let reason = "delegation proof verification failed";
    transport.queue(Ok(json(422, format!(r#"{{"code":7,"log":"{reason}"}}"#))));
    let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);

    let PostAttemptOutcome::Rejected { diagnostic, .. } =
        client.submit_delegation(0, &delegation()).await
    else {
        panic!("expected deterministic rejection");
    };
    assert_eq!(
        diagnostic.message(),
        format!("vote chain rejected transaction with code 7: {reason}")
    );
}

#[tokio::test]
async fn a_hostile_server_log_cannot_exceed_or_escape_the_durable_diagnostic() {
    let transport = Arc::new(ScriptedTransport::default());
    let hostile = "line\\nbreak\\tand ".repeat(200);
    transport.queue(Ok(json(422, format!(r#"{{"code":7,"log":"{hostile}"}}"#))));
    let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);

    let PostAttemptOutcome::Rejected { diagnostic, .. } =
        client.submit_delegation(0, &delegation()).await
    else {
        panic!("expected deterministic rejection");
    };
    assert!(
        diagnostic.message().len()
            <= crate::chain_submission::MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES,
        "{}",
        diagnostic.message().len()
    );
    assert!(!diagnostic.message().contains('\n'));
    assert!(!diagnostic.message().contains('\t'));
}

#[tokio::test]
async fn a_non_json_response_reports_its_type_and_body() {
    let transport = Arc::new(ScriptedTransport::default());
    // What a router 404 or proxy error page looks like: the gateway itself
    // always answers in JSON, so this is the case where knowing the body
    // is the whole diagnosis.
    transport.queue(Ok(ChainHttpResponse::new(
        404,
        b"404 page not found".to_vec(),
        Some("text/plain; charset=utf-8".to_string()),
        Vec::new(),
    )));
    let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);

    let PostAttemptOutcome::PossiblyDispatched(diagnostic) =
        client.submit_delegation(0, &delegation()).await
    else {
        panic!("a non-JSON answer leaves the dispatch unknown");
    };
    assert!(
        diagnostic.message().contains("text/plain"),
        "{}",
        diagnostic.message()
    );
    assert!(
        diagnostic.message().contains("404 page not found"),
        "{}",
        diagnostic.message()
    );
}

#[tokio::test]
async fn nullifier_spent_is_classified_only_by_numeric_code() {
    // The log is reported verbatim but never classifies: a code-2 rejection
    // stays `NullifierAlreadySpent` while its log talks about something
    // else, and a code-7 rejection stays `Other` while its log claims a
    // spent nullifier.
    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(json(
        422,
        r#"{"code":2,"log":"an unrelated and untrusted message"}"#,
    )));
    let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);

    assert!(matches!(
        client.submit_delegation(0, &delegation()).await,
        PostAttemptOutcome::Rejected {
            code: 2,
            kind: ChainRejectionKind::NullifierAlreadySpent,
            ref diagnostic,
            ..
        } if diagnostic.message().contains("an unrelated and untrusted message")
    ));

    let transport = Arc::new(ScriptedTransport::default());
    transport.queue(Ok(json(
        422,
        r#"{"code":7,"log":"nullifier already spent"}"#,
    )));
    let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);
    assert!(matches!(
        client.submit_delegation(0, &delegation()).await,
        PostAttemptOutcome::Rejected {
            code: 7,
            kind: ChainRejectionKind::Other,
            ..
        }
    ));
}

#[tokio::test]
async fn a_log_that_echoes_the_submitted_request_is_redacted_before_it_is_stored() {
    let transport = Arc::new(ScriptedTransport::default());
    // What a node that quotes the request back looks like: an explanation the
    // operator needs, wrapped around proof bytes it must not keep.
    let proof = "ab".repeat(600);
    transport.queue(Ok(json(
        422,
        format!(r#"{{"code":7,"log":"delegation proof rejected: {proof}"}}"#),
    )));
    let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);

    let PostAttemptOutcome::Rejected { diagnostic, .. } =
        client.submit_delegation(0, &delegation()).await
    else {
        panic!("expected deterministic rejection");
    };
    assert_eq!(
        diagnostic.message(),
        "vote chain rejected transaction with code 7: delegation proof rejected: [redacted]"
    );
}

#[tokio::test]
async fn a_non_json_body_that_quotes_the_request_is_redacted_too() {
    let transport = Arc::new(ScriptedTransport::default());
    let signature = "9f".repeat(64);
    transport.queue(Ok(ChainHttpResponse::new(
        502,
        format!("upstream refused {signature}").into_bytes(),
        Some("text/plain".to_string()),
        Vec::new(),
    )));
    let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);

    let PostAttemptOutcome::PossiblyDispatched(diagnostic) =
        client.submit_delegation(0, &delegation()).await
    else {
        panic!("a non-JSON answer leaves the dispatch unknown");
    };
    assert!(
        !diagnostic.message().contains(&signature),
        "{}",
        diagnostic.message()
    );
    assert!(
        diagnostic.message().contains("upstream refused [redacted]"),
        "{}",
        diagnostic.message()
    );
}

#[tokio::test]
async fn a_transaction_hash_in_a_log_stays_legible() {
    let transport = Arc::new(ScriptedTransport::default());
    // 64 hex characters: public, and frequently the whole diagnosis.
    transport.queue(Ok(json(
        422,
        format!(r#"{{"code":7,"log":"conflicts with {HASH}"}}"#),
    )));
    let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);

    let PostAttemptOutcome::Rejected { diagnostic, .. } =
        client.submit_delegation(0, &delegation()).await
    else {
        panic!("expected deterministic rejection");
    };
    assert_eq!(
        diagnostic.message(),
        format!("vote chain rejected transaction with code 7: conflicts with {HASH}")
    );
}

#[tokio::test]
async fn an_encoded_blob_wrapped_in_punctuation_is_redacted() {
    let transport = Arc::new(ScriptedTransport::default());
    // A minified echo of the request: the blob touches no whitespace, so a
    // word-at-a-time reading would keep every byte of it.
    let proof = "ab".repeat(600);
    transport.queue(Ok(json(
        422,
        format!(r#"{{"code":7,"log":"rejected request={{\"proof\":\"{proof}\"}}"}}"#),
    )));
    let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);

    let PostAttemptOutcome::Rejected { diagnostic, .. } =
        client.submit_delegation(0, &delegation()).await
    else {
        panic!("expected deterministic rejection");
    };
    assert!(
        !diagnostic.message().contains("abababab"),
        "{}",
        diagnostic.message()
    );
    assert!(
        diagnostic.message().contains("[redacted]"),
        "{}",
        diagnostic.message()
    );
    // The punctuation around it, and the words, are not encoded material.
    assert!(
        diagnostic.message().contains("rejected request="),
        "{}",
        diagnostic.message()
    );
    assert!(
        diagnostic.message().contains("proof"),
        "{}",
        diagnostic.message()
    );
}
