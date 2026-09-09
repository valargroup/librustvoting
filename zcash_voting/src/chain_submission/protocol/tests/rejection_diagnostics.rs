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
    // What a proxy error page looks like: the gateway itself always answers
    // in JSON, so this is the case where knowing the body is the whole
    // diagnosis. (A router 404 or 405 is the separate, definitely-unsent
    // `EndpointUnsupported` case.)
    transport.queue(Ok(ChainHttpResponse::new(
        502,
        b"502 bad gateway".to_vec(),
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
        diagnostic.message().contains("502 bad gateway"),
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
async fn an_html_200_from_a_proxy_is_definitely_unsent_with_endpoint_unsupported() {
    let transport = Arc::new(ScriptedTransport::default());
    // What production answered on 2026-09-08 for a route its vote-sdk did not
    // yet serve: the explorer's single-page fallback, HTTP 200 and HTML. The
    // gateway never writes HTML, so the body was never decoded and nothing
    // was dispatched.
    transport.queue(Ok(ChainHttpResponse::new(
        200,
        b"<!doctype html>\n<html lang=\"en\">\n  <head>\n    <meta charset=\"UTF-8\" />".to_vec(),
        Some("text/html; charset=utf-8".to_string()),
        Vec::new(),
    )));
    let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);

    let PostAttemptOutcome::EndpointUnsupported(diagnostic) =
        client.submit_delegation(0, &delegation()).await
    else {
        panic!("a fallback page proves the route was never reached");
    };
    assert_eq!(
        diagnostic.kind(),
        ChainSubmissionDiagnosticKind::EndpointUnsupported
    );
    assert!(
        diagnostic
            .message()
            .contains("does not serve /shielded-vote/v1/delegate-vote"),
        "{}",
        diagnostic.message()
    );
    assert!(
        diagnostic.message().contains("text/html"),
        "{}",
        diagnostic.message()
    );
}

#[tokio::test]
async fn an_oversized_fallback_page_is_still_definitely_unsent() {
    // What makes an answer definitely unsent is its status and content type:
    // a router 404 fired before any handler, and the gateway never writes
    // HTML. How long the page is says nothing about whether the request was
    // decoded, so the size limit that bounds a JSON body must not turn these
    // into ambiguous dispatch and strand a reservation the chain never saw.
    let oversized = vec![b'x'; MAX_CHAIN_HTTP_RESPONSE_BYTES + 1];
    for (status, content_type) in [
        (200, "text/html; charset=utf-8"),
        (404, "text/html"),
        (405, "application/json"),
    ] {
        let transport = Arc::new(ScriptedTransport::default());
        transport.queue(Ok(ChainHttpResponse::new(
            status,
            oversized.clone(),
            Some(content_type.to_string()),
            Vec::new(),
        )));
        let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);

        assert!(
            matches!(
                client.submit_delegation(0, &delegation()).await,
                PostAttemptOutcome::EndpointUnsupported(_)
            ),
            "an oversized {status} {content_type} answer is definitely unsent"
        );
    }
}

#[tokio::test]
async fn a_router_404_or_405_is_definitely_unsent_with_endpoint_unsupported() {
    // gorilla mux answers before any handler runs: 404 for an unknown path,
    // 405 for a known path with the wrong method. JSON or not, no handler
    // decoded the request.
    for (status, body, content_type) in [
        (
            404,
            r#"{"error":"not found"}"#,
            Some("application/json".to_string()),
        ),
        (
            405,
            "405 method not allowed",
            Some("text/plain; charset=utf-8".to_string()),
        ),
        (404, "", None),
    ] {
        let transport = Arc::new(ScriptedTransport::default());
        transport.queue(Ok(ChainHttpResponse::new(
            status,
            body.as_bytes().to_vec(),
            content_type,
            Vec::new(),
        )));
        let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);
        let outcome = client.submit_delegation(0, &delegation()).await;
        assert!(
            matches!(
                outcome,
                PostAttemptOutcome::EndpointUnsupported(ref diagnostic)
                    if diagnostic.message().contains(&format!("HTTP {status}"))
            ),
            "status {status}: {outcome:?}"
        );
    }
}

#[tokio::test]
async fn other_non_json_answers_stay_ambiguous() {
    // A proxy error page or challenge after the request may have been
    // forwarded is still unknown dispatch, not proof the route is missing;
    // so is a 200 of any type other than HTML.
    for (status, content_type) in [
        (403, "text/html"),
        (500, "text/html"),
        (502, "text/html"),
        (503, "text/html"),
        (504, "text/html"),
        (200, "text/plain"),
    ] {
        let transport = Arc::new(ScriptedTransport::default());
        transport.queue(Ok(ChainHttpResponse::new(
            status,
            b"<html>error</html>".to_vec(),
            Some(content_type.to_string()),
            Vec::new(),
        )));
        let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);
        assert!(
            matches!(
                client.submit_delegation(0, &delegation()).await,
                PostAttemptOutcome::PossiblyDispatched(_)
            ),
            "status {status}"
        );
    }
}
