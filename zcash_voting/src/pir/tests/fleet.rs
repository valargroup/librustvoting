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

#[test]
fn equivalent_endpoint_spellings_canonicalize_to_one_identity() {
    use super::super::normalize_endpoint_url;

    assert_eq!(
        normalize_endpoint_url(" HTTPS://PIR.Example:443/ "),
        "https://pir.example"
    );
    assert_eq!(
        normalize_endpoint_url("https://pir.example"),
        "https://pir.example"
    );
    assert_eq!(
        normalize_endpoint_url("http://pir.example:8080/mount//"),
        "http://pir.example:8080/mount"
    );
    assert_eq!(
        normalize_endpoint_url("http://Pir.Example:80/mount"),
        "http://pir.example/mount"
    );
    // Unparseable input keeps the plain trimming; connect reports the error.
    assert_eq!(normalize_endpoint_url("not a url/"), "not a url");
}

#[test]
fn a_slash_ending_a_query_value_is_not_a_trailing_path_slash() {
    use super::super::normalize_endpoint_url;

    assert_eq!(
        normalize_endpoint_url("https://pir.example/api?token=abc/"),
        "https://pir.example/api?token=abc/",
        "the query is kept as given"
    );
    assert_eq!(
        normalize_endpoint_url("https://pir.example/api/?token=abc/"),
        "https://pir.example/api?token=abc/",
        "only the path loses its trailing slash"
    );
    assert_ne!(
        normalize_endpoint_url("https://pir.example/api?token=abc/"),
        normalize_endpoint_url("https://pir.example/api?token=abc"),
        "a tokenized endpoint is not the same resource without its final character"
    );
}

#[test]
fn unreserved_percent_escapes_normalize_to_one_endpoint_identity() {
    use super::super::normalize_endpoint_url;

    assert_eq!(
        normalize_endpoint_url("https://pir.example/%7Eoperator"),
        normalize_endpoint_url("https://pir.example/~operator")
    );
    assert_eq!(
        normalize_endpoint_url("https://pir.example/%7eoperator/%41b"),
        "https://pir.example/~operator/Ab"
    );
    // Reserved escapes keep their meaning, in canonical uppercase hex.
    assert_eq!(
        normalize_endpoint_url("https://pir.example/a%2fb"),
        "https://pir.example/a%2Fb"
    );
    // A malformed escape is left alone rather than guessed at.
    assert_eq!(
        normalize_endpoint_url("https://pir.example/a%zz"),
        "https://pir.example/a%zz"
    );
}

#[test]
fn dot_segments_resolve_to_one_endpoint_identity() {
    use super::super::normalize_endpoint_url;

    assert_eq!(
        normalize_endpoint_url("https://pir.example/a/../api"),
        normalize_endpoint_url("https://pir.example/api")
    );
    assert_eq!(
        normalize_endpoint_url("https://pir.example/./api/."),
        "https://pir.example/api"
    );
    assert_eq!(
        normalize_endpoint_url("https://pir.example/a/b/../../c"),
        "https://pir.example/c"
    );
    // `..` above the root is dropped rather than escaping the host.
    assert_eq!(
        normalize_endpoint_url("https://pir.example/../api"),
        "https://pir.example/api"
    );
    // An escaped dot segment resolves after escape normalization.
    assert_eq!(
        normalize_endpoint_url("https://pir.example/%2E%2E/api"),
        "https://pir.example/api"
    );
}

/// An empty fleet builds, and fails only if something actually asks it for a
/// PIR answer.
///
/// A step that advances a delegation the chain already holds needs no PIR, and
/// a host running one has no endpoint to give. Enforcing the requirement at
/// construction made the fleet a precondition of opening a session at all,
/// which is what stopped a submitted delegation from resuming without PIR.
#[test]
fn an_empty_fleet_defers_its_requirement_to_the_first_use() {
    let endpoints: Vec<String> = Vec::new();
    let result = failover_over(
        &endpoints,
        |endpoint: &str| -> Result<String, VotingError> {
            panic!("no endpoint to connect: {endpoint}")
        },
        |_session: &String| -> Result<u8, VotingError> { panic!("no session to use") },
    );

    let error = result.expect_err("an empty fleet cannot answer a PIR request");
    assert!(
        error
            .to_string()
            .contains("at least one PIR server URL is required"),
        "{error}"
    );
}
