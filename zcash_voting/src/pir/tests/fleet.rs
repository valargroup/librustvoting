//! Endpoint failover: retryable failures move on in order, anything else stops.

use super::super::failover_over;
use crate::VotingError;

fn retryable(endpoint: &str) -> VotingError {
    VotingError::PirUnavailable {
        endpoint: Some(endpoint.to_string()),
        http_status: Some(503),
        retryable: true,
        message: "unavailable".to_string(),
    }
}

fn fatal() -> VotingError {
    VotingError::InvalidInput {
        message: "layout mismatch".to_string(),
    }
}

#[test]
fn failover_skips_retryable_connect_and_operation_failures_in_order() {
    let endpoints: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
    let mut connects = Vec::new();
    let mut operations = Vec::new();
    let (session, value) = failover_over(
        &endpoints,
        |endpoint| {
            connects.push(endpoint.to_string());
            if endpoint == "a" {
                Err(retryable(endpoint))
            } else {
                Ok(endpoint.to_string())
            }
        },
        |session| {
            operations.push(session.clone());
            if session == "b" {
                Err(retryable(session))
            } else {
                Ok(42)
            }
        },
    )
    .unwrap();
    assert_eq!((session.as_str(), value), ("c", 42));
    assert_eq!(connects, ["a", "b", "c"]);
    assert_eq!(operations, ["b", "c"]);
}

#[test]
fn failover_stops_on_non_retryable_failure_and_on_exhaustion() {
    let endpoints: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
    let error = failover_over(
        &endpoints,
        |endpoint| Ok(endpoint.to_string()),
        |_| Err::<(), _>(fatal()),
    )
    .unwrap_err();
    assert_eq!(error.kind(), crate::VotingErrorKind::InvalidInput);

    let error = failover_over(
        &endpoints,
        |endpoint| Err::<String, _>(retryable(endpoint)),
        |_| Ok(()),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        VotingError::PirUnavailable { endpoint: Some(ref endpoint), .. } if endpoint == "b"
    ));
}

#[test]
fn local_contention_is_returned_instead_of_failing_over() {
    let endpoints: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
    let mut operations = 0;
    let error = failover_over(
        &endpoints,
        |endpoint| Ok(endpoint.to_string()),
        |_| {
            operations += 1;
            Err::<(), _>(VotingError::DbBusy {
                message: "database is locked".to_string(),
            })
        },
    )
    .unwrap_err();

    assert!(error.retryable(), "the host may retry the operation later");
    assert_eq!(error.kind(), crate::VotingErrorKind::DbBusy);
    assert_eq!(
        operations, 1,
        "another endpoint cannot fix local contention"
    );
}
