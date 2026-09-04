//! Connect-error classification: a typed transport failure decides
//! retryability; message text is consulted only when nothing typed is attached.

use super::super::map_pir_connect_error;
use crate::{
    http_transport::{PirHttpFailure, PirHttpFailurePhase},
    VotingError,
};

const ENDPOINT: &str = "https://pir.example";

fn typed_status_failure(status: u16, body: &str) -> anyhow::Error {
    anyhow::Error::from(PirHttpFailure {
        phase: PirHttpFailurePhase::Status,
        http_status: Some(status),
    })
    .context(format!("PIR HTTP status {status} body={body}"))
    .context("connect root fetch failed")
}

#[test]
fn a_retryable_http_failure_stays_retryable_even_if_its_body_echoes_a_layout_mismatch() {
    let error = map_pir_connect_error(
        ENDPOINT,
        typed_status_failure(503, "PIR layout mismatch: upstream unavailable"),
    );

    assert!(
        matches!(
            error,
            VotingError::PirUnavailable {
                endpoint: Some(ref endpoint),
                http_status: Some(503),
                retryable: true,
                ..
            } if endpoint == ENDPOINT
        ),
        "{error}"
    );
    assert!(error.retryable());
}

#[test]
fn a_non_retryable_http_failure_keeps_its_typed_classification() {
    let error = map_pir_connect_error(ENDPOINT, typed_status_failure(400, "PIR poly_len mismatch"));

    assert!(
        matches!(
            error,
            VotingError::PirUnavailable {
                http_status: Some(400),
                retryable: false,
                ..
            }
        ),
        "{error}"
    );
    assert!(!error.retryable());
}

#[test]
fn an_untyped_layout_mismatch_is_a_configuration_error() {
    for message in [
        "PIR layout mismatch: depth 20 != 18",
        "PIR poly_len mismatch",
    ] {
        let error = map_pir_connect_error(ENDPOINT, anyhow::anyhow!("{message}"));

        assert!(matches!(error, VotingError::InvalidInput { .. }), "{error}");
        assert!(!error.retryable());
        assert!(error.to_string().contains(message), "{error}");
    }
}

#[test]
fn an_untyped_transport_error_is_unavailable_but_not_retryable_by_text() {
    let error = map_pir_connect_error(ENDPOINT, anyhow::anyhow!("connection reset"));

    assert!(
        matches!(
            error,
            VotingError::PirUnavailable {
                http_status: None,
                retryable: false,
                ..
            }
        ),
        "{error}"
    );
}
