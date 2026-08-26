//! Caller-transported helper-server status checks.
//!
//! This module owns the helper REST route, response parsing, retry semantics,
//! endpoint health ordering, and confirmation aggregation. Callers own the
//! actual HTTP stack and inject it through [`HelperHttpTransport`]. In
//! particular, this API never constructs the crate's `HyperTransport`.

use std::{
    collections::HashMap,
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use http::Uri;
use serde::Deserialize;

const HELPER_API_PATH: &str = "shielded-vote/v1";
const DEFAULT_FAILURE_THRESHOLD: u32 = 3;
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(30);
const MAX_STATUS_BODY_BYTES: usize = 64 * 1024;
const MAX_DIAGNOSTIC_DETAIL_BYTES: usize = 512;

/// Raw response returned by a caller-owned helper HTTP transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperHttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// A boxed async helper HTTP response.
pub type HelperHttpFuture<'a> =
    Pin<Box<dyn Future<Output = Result<HelperHttpResponse, HelperHttpTransportError>> + Send + 'a>>;

/// Transport-layer failure reported by a caller-owned HTTP implementation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperHttpTransportError {
    pub message: String,
}

impl HelperHttpTransportError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for HelperHttpTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HelperHttpTransportError {}

/// Async GET transport supplied by the wallet or host SDK.
///
/// `url` is the complete URL constructed by `zcash_voting`; transports must
/// send it as-is and must not append the helper API route. The cancellation
/// handle lets transports abort in-flight work when their HTTP stack supports
/// cancellation. Returning a cancellation-related transport error after the
/// handle is cancelled does not degrade endpoint health.
pub trait HelperHttpTransport: Send + Sync {
    fn get<'a>(
        &'a self,
        url: &'a str,
        cancellation: &'a HelperStatusCancellation,
    ) -> HelperHttpFuture<'a>;
}

/// Stable helper identity and its current transport base URL.
///
/// Health is keyed by `id`, not `transport_base_url`, so changing network
/// routing, proxying, or an origin URL does not silently create a new helper
/// identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperEndpoint {
    pub id: String,
    pub transport_base_url: String,
}

impl HelperEndpoint {
    pub fn new(id: impl Into<String>, transport_base_url: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            transport_base_url: transport_base_url.into(),
        }
    }
}

/// Injectable clock used by [`HelperHealthTracker`].
pub trait HelperHealthClock: Send + Sync {
    fn now(&self) -> Duration;
}

impl<F> HelperHealthClock for F
where
    F: Fn() -> Duration + Send + Sync,
{
    fn now(&self) -> Duration {
        self()
    }
}

fn system_health_now() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
}

#[derive(Clone, Copy, Debug, Default)]
struct HelperHealthState {
    consecutive_failures: u32,
    degraded_at: Option<Duration>,
}

/// Caller-instantiated, SDK-owned helper health policy and state.
///
/// Three consecutive failed operations degrade an endpoint for 30 seconds by
/// default. Healthy candidates retain caller order and are tried before
/// degraded candidates. If every candidate is degraded, all candidates are
/// still returned in caller order so a fleet-wide outage cannot permanently
/// block recovery.
pub struct HelperHealthTracker {
    failure_threshold: u32,
    cooldown: Duration,
    clock: Arc<dyn HelperHealthClock>,
    states: Mutex<HashMap<String, HelperHealthState>>,
}

impl HelperHealthTracker {
    /// Creates a tracker with the default three-failure, 30-second policy.
    pub fn new() -> Self {
        Self::with_clock(
            DEFAULT_FAILURE_THRESHOLD,
            DEFAULT_COOLDOWN,
            Arc::new(system_health_now as fn() -> Duration),
        )
    }

    /// Creates a tracker with caller-selected policy and the system clock.
    pub fn with_policy(failure_threshold: u32, cooldown: Duration) -> Self {
        Self::with_clock(
            failure_threshold,
            cooldown,
            Arc::new(system_health_now as fn() -> Duration),
        )
    }

    /// Creates a tracker with a deterministic caller-provided clock.
    pub fn with_clock(
        failure_threshold: u32,
        cooldown: Duration,
        clock: Arc<dyn HelperHealthClock>,
    ) -> Self {
        assert!(failure_threshold > 0, "failure_threshold must be positive");
        Self {
            failure_threshold,
            cooldown,
            clock,
            states: Mutex::new(HashMap::new()),
        }
    }

    pub fn failure_threshold(&self) -> u32 {
        self.failure_threshold
    }

    pub fn cooldown(&self) -> Duration {
        self.cooldown
    }

    /// Returns candidates in health-preferred order.
    pub fn candidates(&self, endpoints: &[HelperEndpoint]) -> Vec<HelperEndpoint> {
        let now = self.clock.now();
        let mut states = self.states.lock().expect("helper health lock poisoned");
        let mut healthy = Vec::with_capacity(endpoints.len());
        let mut degraded = Vec::new();

        for endpoint in endpoints {
            let state = states.entry(endpoint.id.clone()).or_default();
            let is_degraded = match state.degraded_at {
                Some(degraded_at) if elapsed(now, degraded_at) < self.cooldown => true,
                Some(_) => {
                    state.degraded_at = None;
                    state.consecutive_failures = self.failure_threshold - 1;
                    false
                }
                None => false,
            };
            if is_degraded {
                degraded.push(endpoint.clone());
            } else {
                healthy.push(endpoint.clone());
            }
        }

        if healthy.is_empty() {
            endpoints.to_vec()
        } else {
            healthy.extend(degraded);
            healthy
        }
    }

    /// Clears consecutive failure and cooldown state after a valid response.
    pub fn record_success(&self, endpoint_id: &str) {
        self.states
            .lock()
            .expect("helper health lock poisoned")
            .remove(endpoint_id);
    }

    /// Records one failed helper operation.
    pub fn record_failure(&self, endpoint_id: &str) {
        let mut states = self.states.lock().expect("helper health lock poisoned");
        let state = states.entry(endpoint_id.to_string()).or_default();
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        if state.consecutive_failures >= self.failure_threshold {
            state.degraded_at = Some(self.clock.now());
        }
    }
}

impl Default for HelperHealthTracker {
    fn default() -> Self {
        Self::new()
    }
}

fn elapsed(now: Duration, earlier: Duration) -> Duration {
    now.checked_sub(earlier).unwrap_or(Duration::ZERO)
}

/// Cloneable cancellation state for one or more helper status operations.
#[derive(Clone, Debug, Default)]
pub struct HelperStatusCancellation {
    cancelled: Arc<AtomicBool>,
}

impl HelperStatusCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Bounded retry policy for one helper endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperStatusRetryPolicy {
    /// Maximum time allowed for one transport attempt.
    pub attempt_timeout: Duration,
    /// Delay after each transient failed attempt. An empty list makes one attempt.
    pub delays: Vec<Duration>,
}

impl Default for HelperStatusRetryPolicy {
    fn default() -> Self {
        Self {
            attempt_timeout: Duration::from_secs(5),
            delays: vec![Duration::from_millis(200), Duration::from_millis(600)],
        }
    }
}

/// Aggregate result of checking a share against helper candidates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperConfirmationReport {
    pub outcome: HelperConfirmationOutcome,
    pub diagnostics: Vec<HelperEndpointDiagnostic>,
}

/// Typed result of a confirmed-by-any-helper pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HelperConfirmationOutcome {
    /// At least one helper returned a valid confirmed response.
    Confirmed { endpoint_id: String },
    /// At least one helper returned valid pending and none returned confirmed.
    Pending,
    /// No endpoint returned a valid pending or confirmed response.
    AllFailed,
    /// The operation was cancelled without degrading the active endpoint.
    Cancelled,
}

/// Final diagnostic for one attempted endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperEndpointDiagnostic {
    pub endpoint_id: String,
    pub transport_base_url: String,
    pub request_url: String,
    pub attempts: Vec<HelperAttemptDiagnostic>,
}

/// One raw HTTP attempt and its interpreted result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperAttemptDiagnostic {
    pub attempt: u32,
    pub result: HelperAttemptResult,
}

/// Typed interpretation of a helper HTTP attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HelperAttemptResult {
    Pending,
    Confirmed,
    TransportFailure { message: String },
    HttpFailure { status: u16, body: String },
    MalformedResponse { message: String },
}

/// Request validation error that occurs before helper health can be evaluated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperStatusRequestError {
    pub message: String,
}

impl fmt::Display for HelperStatusRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HelperStatusRequestError {}

/// Checks candidates sequentially and stops as soon as any helper confirms.
///
/// The SDK constructs and owns
/// `/shielded-vote/v1/share-status/{round_id}/{share_id}`. Callers supply only
/// endpoint identity, transport base URL, and an HTTP implementation. Valid
/// `pending` is a successful health response. Transport errors and transient
/// HTTP statuses (429, 500, 502, 503, and 504) are retried within the supplied
/// bound. Other non-2xx statuses, oversized/malformed bodies, and unknown
/// statuses immediately degrade that endpoint and move to the next helper.
/// Cancellation returns [`HelperConfirmationOutcome::Cancelled`] and never
/// changes health state.
pub async fn confirmed_by_any_helper<T: HelperHttpTransport + ?Sized>(
    transport: &T,
    health: &HelperHealthTracker,
    endpoints: &[HelperEndpoint],
    round_id: &str,
    share_id: &str,
    retry_policy: &HelperStatusRetryPolicy,
    cancellation: &HelperStatusCancellation,
) -> Result<HelperConfirmationReport, HelperStatusRequestError> {
    validate_route_segment("round_id", round_id)?;
    validate_route_segment("share_id", share_id)?;

    if cancellation.is_cancelled() {
        return Ok(HelperConfirmationReport {
            outcome: HelperConfirmationOutcome::Cancelled,
            diagnostics: Vec::new(),
        });
    }

    let candidates = health.candidates(endpoints);
    let mut diagnostics = Vec::with_capacity(candidates.len());
    let mut valid_pending = 0usize;

    for endpoint in candidates {
        if cancellation.is_cancelled() {
            return Ok(HelperConfirmationReport {
                outcome: HelperConfirmationOutcome::Cancelled,
                diagnostics,
            });
        }

        let request_url =
            helper_share_status_url(&endpoint.transport_base_url, round_id, share_id)?;
        let mut endpoint_diagnostic = HelperEndpointDiagnostic {
            endpoint_id: endpoint.id.clone(),
            transport_base_url: endpoint.transport_base_url.clone(),
            request_url: request_url.clone(),
            attempts: Vec::with_capacity(retry_policy.delays.len() + 1),
        };

        for attempt_index in 0..=retry_policy.delays.len() {
            if cancellation.is_cancelled() {
                diagnostics.push(endpoint_diagnostic);
                return Ok(HelperConfirmationReport {
                    outcome: HelperConfirmationOutcome::Cancelled,
                    diagnostics,
                });
            }

            let response = match tokio::time::timeout(
                retry_policy.attempt_timeout,
                transport.get(&request_url, cancellation),
            )
            .await
            {
                Ok(response) => response,
                Err(_) => Err(HelperHttpTransportError::new(format!(
                    "helper status request timed out after {:?}",
                    retry_policy.attempt_timeout
                ))),
            };
            if cancellation.is_cancelled() {
                diagnostics.push(endpoint_diagnostic);
                return Ok(HelperConfirmationReport {
                    outcome: HelperConfirmationOutcome::Cancelled,
                    diagnostics,
                });
            }

            let interpreted = interpret_response(response);
            endpoint_diagnostic.attempts.push(HelperAttemptDiagnostic {
                attempt: (attempt_index + 1) as u32,
                result: interpreted.clone(),
            });

            match interpreted {
                HelperAttemptResult::Confirmed => {
                    health.record_success(&endpoint.id);
                    diagnostics.push(endpoint_diagnostic);
                    return Ok(HelperConfirmationReport {
                        outcome: HelperConfirmationOutcome::Confirmed {
                            endpoint_id: endpoint.id,
                        },
                        diagnostics,
                    });
                }
                HelperAttemptResult::Pending => {
                    health.record_success(&endpoint.id);
                    valid_pending += 1;
                    break;
                }
                failure @ (HelperAttemptResult::TransportFailure { .. }
                | HelperAttemptResult::HttpFailure { .. }
                | HelperAttemptResult::MalformedResponse { .. }) => {
                    if is_transient_failure(&failure) {
                        if let Some(delay) = retry_policy.delays.get(attempt_index) {
                            if cancellation.is_cancelled() {
                                diagnostics.push(endpoint_diagnostic);
                                return Ok(HelperConfirmationReport {
                                    outcome: HelperConfirmationOutcome::Cancelled,
                                    diagnostics,
                                });
                            }
                            tokio::time::sleep(*delay).await;
                            continue;
                        }
                    }

                    health.record_failure(&endpoint.id);
                    break;
                }
            }
        }

        diagnostics.push(endpoint_diagnostic);
    }

    Ok(HelperConfirmationReport {
        outcome: if valid_pending > 0 {
            HelperConfirmationOutcome::Pending
        } else {
            HelperConfirmationOutcome::AllFailed
        },
        diagnostics,
    })
}

fn is_transient_failure(result: &HelperAttemptResult) -> bool {
    match result {
        HelperAttemptResult::TransportFailure { .. } => true,
        HelperAttemptResult::HttpFailure { status, .. } => {
            matches!(status, 429 | 500 | 502 | 503 | 504)
        }
        HelperAttemptResult::Pending
        | HelperAttemptResult::Confirmed
        | HelperAttemptResult::MalformedResponse { .. } => false,
    }
}

/// Constructs the complete SDK-owned helper share-status route.
pub fn helper_share_status_url(
    transport_base_url: &str,
    round_id: &str,
    share_id: &str,
) -> Result<String, HelperStatusRequestError> {
    validate_route_segment("round_id", round_id)?;
    validate_route_segment("share_id", share_id)?;

    let base = transport_base_url.trim_end_matches('/');
    let parsed = base
        .parse::<Uri>()
        .map_err(|error| HelperStatusRequestError {
            message: format!("invalid helper transport base URL: {error}"),
        })?;
    if !matches!(parsed.scheme_str(), Some("http" | "https")) || parsed.authority().is_none() {
        return Err(HelperStatusRequestError {
            message: "helper transport base URL must be absolute HTTP(S)".to_string(),
        });
    }
    if parsed.query().is_some() {
        return Err(HelperStatusRequestError {
            message: "helper transport base URL must not contain a query".to_string(),
        });
    }

    Ok(format!(
        "{base}/{HELPER_API_PATH}/share-status/{}/{}",
        round_id.to_ascii_lowercase(),
        share_id.to_ascii_lowercase()
    ))
}

fn validate_route_segment(label: &str, value: &str) -> Result<(), HelperStatusRequestError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(HelperStatusRequestError {
            message: format!("{label} must be exactly 64 hexadecimal characters"),
        });
    }
    Ok(())
}

#[derive(Deserialize)]
struct ShareStatusBody {
    status: String,
}

fn interpret_response(
    response: Result<HelperHttpResponse, HelperHttpTransportError>,
) -> HelperAttemptResult {
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            return HelperAttemptResult::TransportFailure {
                message: diagnostic_text(error.message.as_bytes()),
            }
        }
    };

    if !(200..300).contains(&response.status) {
        return HelperAttemptResult::HttpFailure {
            status: response.status,
            body: diagnostic_text(&response.body),
        };
    }
    if response.body.len() > MAX_STATUS_BODY_BYTES {
        return HelperAttemptResult::MalformedResponse {
            message: format!(
                "response body exceeds {MAX_STATUS_BODY_BYTES}-byte limit ({} bytes)",
                response.body.len()
            ),
        };
    }

    match serde_json::from_slice::<ShareStatusBody>(&response.body) {
        Ok(body) if body.status == "pending" => HelperAttemptResult::Pending,
        Ok(body) if body.status == "confirmed" => HelperAttemptResult::Confirmed,
        Ok(body) => HelperAttemptResult::MalformedResponse {
            message: format!("unexpected helper share status: {}", body.status),
        },
        Err(error) => HelperAttemptResult::MalformedResponse {
            message: format!("invalid helper share status JSON: {error}"),
        },
    }
}

fn diagnostic_text(bytes: &[u8]) -> String {
    let prefix = &bytes[..bytes.len().min(MAX_DIAGNOSTIC_DETAIL_BYTES)];
    let mut text = String::from_utf8_lossy(prefix).into_owned();
    if bytes.len() > MAX_DIAGNOSTIC_DETAIL_BYTES {
        text.push_str("…");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::{HashMap, VecDeque},
        sync::atomic::AtomicU64,
    };

    const ROUND_ID: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    const SHARE_ID: &str = "0202020202020202020202020202020202020202020202020202020202020202";

    struct FakeTransport {
        responses: Mutex<HashMap<String, VecDeque<FakeResponse>>>,
        requests: Mutex<Vec<String>>,
        cancellation: Option<HelperStatusCancellation>,
    }

    enum FakeResponse {
        Response(u16, &'static str),
        Error(&'static str),
        CancelThenError(&'static str),
        Hang,
    }

    impl FakeTransport {
        fn new(entries: impl IntoIterator<Item = (String, Vec<FakeResponse>)>) -> Self {
            Self {
                responses: Mutex::new(
                    entries
                        .into_iter()
                        .map(|(url, responses)| (url, responses.into()))
                        .collect(),
                ),
                requests: Mutex::new(Vec::new()),
                cancellation: None,
            }
        }

        fn with_cancellation(mut self, cancellation: HelperStatusCancellation) -> Self {
            self.cancellation = Some(cancellation);
            self
        }

        fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    impl HelperHttpTransport for FakeTransport {
        fn get<'a>(
            &'a self,
            url: &'a str,
            _cancellation: &'a HelperStatusCancellation,
        ) -> HelperHttpFuture<'a> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(url.to_string());
                let response = self
                    .responses
                    .lock()
                    .unwrap()
                    .get_mut(url)
                    .and_then(VecDeque::pop_front)
                    .unwrap_or(FakeResponse::Error("missing fake response"));
                match response {
                    FakeResponse::Response(status, body) => Ok(HelperHttpResponse {
                        status,
                        body: body.as_bytes().to_vec(),
                    }),
                    FakeResponse::Error(message) => Err(HelperHttpTransportError::new(message)),
                    FakeResponse::CancelThenError(message) => {
                        self.cancellation.as_ref().unwrap().cancel();
                        Err(HelperHttpTransportError::new(message))
                    }
                    FakeResponse::Hang => {
                        std::future::pending::<()>().await;
                        unreachable!()
                    }
                }
            })
        }
    }

    fn endpoint(id: &str) -> HelperEndpoint {
        HelperEndpoint::new(id, format!("https://{id}.example"))
    }

    fn url(id: &str) -> String {
        helper_share_status_url(&format!("https://{id}.example"), ROUND_ID, SHARE_ID).unwrap()
    }

    fn response(status: &'static str) -> FakeResponse {
        FakeResponse::Response(
            200,
            match status {
                "pending" => r#"{"status":"pending"}"#,
                "confirmed" => r#"{"status":"confirmed"}"#,
                _ => unreachable!(),
            },
        )
    }

    fn no_retries() -> HelperStatusRetryPolicy {
        HelperStatusRetryPolicy {
            attempt_timeout: Duration::from_secs(1),
            delays: vec![],
        }
    }

    #[test]
    fn transient_failure_policy_matches_vizor_http_policy() {
        assert!(is_transient_failure(
            &HelperAttemptResult::TransportFailure {
                message: "offline".to_string(),
            }
        ));
        for status in [429, 500, 502, 503, 504] {
            assert!(is_transient_failure(&HelperAttemptResult::HttpFailure {
                status,
                body: String::new(),
            }));
        }
        for status in [400, 404, 408, 501, 505] {
            assert!(!is_transient_failure(&HelperAttemptResult::HttpFailure {
                status,
                body: String::new(),
            }));
        }
        assert!(!is_transient_failure(
            &HelperAttemptResult::MalformedResponse {
                message: "unknown status".to_string(),
            }
        ));
    }

    #[tokio::test]
    async fn attempt_timeout_is_a_transient_failure() {
        let transport = FakeTransport::new([
            (url("a"), vec![FakeResponse::Hang]),
            (
                url("b"),
                vec![FakeResponse::Response(200, r#"{"status":"confirmed"}"#)],
            ),
        ]);
        let policy = HelperStatusRetryPolicy {
            attempt_timeout: Duration::from_millis(1),
            delays: vec![],
        };

        let report = confirmed_by_any_helper(
            &transport,
            &HelperHealthTracker::new(),
            &[HelperEndpoint::new("a", "https://a.example"), endpoint("b")],
            ROUND_ID,
            SHARE_ID,
            &policy,
            &HelperStatusCancellation::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            report.outcome,
            HelperConfirmationOutcome::Confirmed {
                endpoint_id: "b".to_string()
            }
        );
    }

    #[tokio::test]
    async fn four_confirmed_candidates_and_one_missing_identity_short_circuit() {
        let endpoints = ["missing", "one", "two", "three", "four"]
            .map(endpoint)
            .to_vec();
        let transport = FakeTransport::new([
            (url("missing"), vec![FakeResponse::Response(404, "missing")]),
            (url("one"), vec![response("confirmed")]),
            (url("two"), vec![response("confirmed")]),
            (url("three"), vec![response("confirmed")]),
            (url("four"), vec![response("confirmed")]),
        ]);

        let report = confirmed_by_any_helper(
            &transport,
            &HelperHealthTracker::new(),
            &endpoints,
            ROUND_ID,
            SHARE_ID,
            &no_retries(),
            &HelperStatusCancellation::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            report.outcome,
            HelperConfirmationOutcome::Confirmed {
                endpoint_id: "one".to_string()
            }
        );
        assert_eq!(report.diagnostics.len(), 2);
        assert_eq!(transport.request_count(), 2);
    }

    #[tokio::test]
    async fn early_confirmed_response_stops_later_candidates() {
        let transport = FakeTransport::new([
            (url("one"), vec![response("confirmed")]),
            (url("two"), vec![response("pending")]),
        ]);

        let report = confirmed_by_any_helper(
            &transport,
            &HelperHealthTracker::new(),
            &[endpoint("one"), endpoint("two")],
            ROUND_ID,
            SHARE_ID,
            &no_retries(),
            &HelperStatusCancellation::new(),
        )
        .await
        .unwrap();

        assert!(matches!(
            report.outcome,
            HelperConfirmationOutcome::Confirmed { .. }
        ));
        assert_eq!(transport.request_count(), 1);
    }

    #[tokio::test]
    async fn all_pending_is_a_successful_pending_outcome() {
        let transport = FakeTransport::new([
            (url("one"), vec![response("pending")]),
            (url("two"), vec![response("pending")]),
        ]);
        let health = HelperHealthTracker::with_policy(1, Duration::from_secs(60));

        let report = confirmed_by_any_helper(
            &transport,
            &health,
            &[endpoint("one"), endpoint("two")],
            ROUND_ID,
            SHARE_ID,
            &no_retries(),
            &HelperStatusCancellation::new(),
        )
        .await
        .unwrap();

        assert_eq!(report.outcome, HelperConfirmationOutcome::Pending);
        assert_eq!(
            health.candidates(&[endpoint("one"), endpoint("two")]),
            vec![endpoint("one"), endpoint("two")]
        );
    }

    #[tokio::test]
    async fn all_failed_returns_diagnostics_and_degrades() {
        let transport = FakeTransport::new([
            (url("one"), vec![FakeResponse::Error("offline")]),
            (url("two"), vec![FakeResponse::Response(503, "unavailable")]),
        ]);
        let health = HelperHealthTracker::with_policy(1, Duration::from_secs(60));

        let report = confirmed_by_any_helper(
            &transport,
            &health,
            &[endpoint("one"), endpoint("two")],
            ROUND_ID,
            SHARE_ID,
            &no_retries(),
            &HelperStatusCancellation::new(),
        )
        .await
        .unwrap();

        assert_eq!(report.outcome, HelperConfirmationOutcome::AllFailed);
        assert_eq!(report.diagnostics.len(), 2);
        // All-degraded fallback preserves all candidates in caller order.
        assert_eq!(
            health.candidates(&[endpoint("one"), endpoint("two")]),
            vec![endpoint("one"), endpoint("two")]
        );
    }

    #[test]
    fn cooldown_uses_identity_and_injected_clock() {
        let defaults = HelperHealthTracker::new();
        assert_eq!(defaults.failure_threshold(), 3);
        assert_eq!(defaults.cooldown(), Duration::from_secs(30));

        let now = Arc::new(AtomicU64::new(100));
        let clock_now = Arc::clone(&now);
        let health = HelperHealthTracker::with_clock(
            3,
            Duration::from_secs(30),
            Arc::new(move || Duration::from_secs(clock_now.load(Ordering::Relaxed))),
        );
        let endpoints = [endpoint("one"), endpoint("two")];

        for _ in 0..3 {
            health.record_failure("one");
        }
        assert_eq!(
            health.candidates(&endpoints),
            vec![endpoint("two"), endpoint("one")]
        );

        now.store(130, Ordering::Relaxed);
        let moved_url = HelperEndpoint::new("one", "https://replacement.example");
        assert_eq!(
            health.candidates(&[moved_url.clone(), endpoint("two")]),
            vec![moved_url, endpoint("two")]
        );
    }

    #[tokio::test]
    async fn bounded_retries_stop_after_success() {
        let transport = FakeTransport::new([(
            url("one"),
            vec![
                FakeResponse::Error("offline"),
                FakeResponse::Response(503, "unavailable"),
                response("confirmed"),
                response("pending"),
            ],
        )]);
        let policy = HelperStatusRetryPolicy {
            attempt_timeout: Duration::from_secs(1),
            delays: vec![Duration::ZERO, Duration::ZERO, Duration::ZERO],
        };

        let report = confirmed_by_any_helper(
            &transport,
            &HelperHealthTracker::new(),
            &[endpoint("one")],
            ROUND_ID,
            SHARE_ID,
            &policy,
            &HelperStatusCancellation::new(),
        )
        .await
        .unwrap();

        assert!(matches!(
            report.outcome,
            HelperConfirmationOutcome::Confirmed { .. }
        ));
        assert_eq!(report.diagnostics[0].attempts.len(), 3);
        assert_eq!(transport.request_count(), 3);
    }

    #[tokio::test]
    async fn non_transient_404_is_single_attempt() {
        let transport = FakeTransport::new([
            (url("one"), vec![FakeResponse::Response(404, "missing")]),
            (url("two"), vec![response("pending")]),
        ]);
        let health = HelperHealthTracker::with_policy(1, Duration::from_secs(60));
        let policy = HelperStatusRetryPolicy {
            attempt_timeout: Duration::from_secs(1),
            delays: vec![Duration::ZERO, Duration::ZERO],
        };

        let report = confirmed_by_any_helper(
            &transport,
            &health,
            &[endpoint("one"), endpoint("two")],
            ROUND_ID,
            SHARE_ID,
            &policy,
            &HelperStatusCancellation::new(),
        )
        .await
        .unwrap();

        assert_eq!(report.outcome, HelperConfirmationOutcome::Pending);
        assert_eq!(report.diagnostics[0].attempts.len(), 1);
        assert!(matches!(
            report.diagnostics[0].attempts[0].result,
            HelperAttemptResult::HttpFailure { status: 404, .. }
        ));
        assert_eq!(transport.request_count(), 2);
    }

    #[tokio::test]
    async fn malformed_status_is_single_attempt() {
        let transport = FakeTransport::new([
            (
                url("one"),
                vec![
                    FakeResponse::Response(200, r#"{"status":"unknown"}"#),
                    FakeResponse::Response(200, "not-json"),
                ],
            ),
            (url("two"), vec![response("pending")]),
        ]);
        let health = HelperHealthTracker::with_policy(1, Duration::from_secs(60));
        let policy = HelperStatusRetryPolicy {
            attempt_timeout: Duration::from_secs(1),
            delays: vec![Duration::ZERO],
        };

        let report = confirmed_by_any_helper(
            &transport,
            &health,
            &[endpoint("one"), endpoint("two")],
            ROUND_ID,
            SHARE_ID,
            &policy,
            &HelperStatusCancellation::new(),
        )
        .await
        .unwrap();

        assert_eq!(report.outcome, HelperConfirmationOutcome::Pending);
        assert_eq!(report.diagnostics[0].attempts.len(), 1);
        assert!(matches!(
            report.diagnostics[0].attempts[0].result,
            HelperAttemptResult::MalformedResponse { .. }
        ));
        assert_eq!(transport.request_count(), 2);
        assert_eq!(
            health.candidates(&[endpoint("one"), endpoint("two")]),
            vec![endpoint("two"), endpoint("one")]
        );
    }

    #[tokio::test]
    async fn transient_503_retries() {
        let transport = FakeTransport::new([(
            url("one"),
            vec![
                FakeResponse::Response(503, "unavailable"),
                response("confirmed"),
            ],
        )]);
        let policy = HelperStatusRetryPolicy {
            attempt_timeout: Duration::from_secs(1),
            delays: vec![Duration::ZERO],
        };

        let report = confirmed_by_any_helper(
            &transport,
            &HelperHealthTracker::new(),
            &[endpoint("one")],
            ROUND_ID,
            SHARE_ID,
            &policy,
            &HelperStatusCancellation::new(),
        )
        .await
        .unwrap();

        assert!(matches!(
            report.outcome,
            HelperConfirmationOutcome::Confirmed { .. }
        ));
        assert_eq!(report.diagnostics[0].attempts.len(), 2);
        assert_eq!(transport.request_count(), 2);
    }

    #[tokio::test]
    async fn transport_failure_retries() {
        let transport = FakeTransport::new([(
            url("one"),
            vec![FakeResponse::Error("offline"), response("pending")],
        )]);
        let policy = HelperStatusRetryPolicy {
            attempt_timeout: Duration::from_secs(1),
            delays: vec![Duration::ZERO],
        };

        let report = confirmed_by_any_helper(
            &transport,
            &HelperHealthTracker::new(),
            &[endpoint("one")],
            ROUND_ID,
            SHARE_ID,
            &policy,
            &HelperStatusCancellation::new(),
        )
        .await
        .unwrap();

        assert_eq!(report.outcome, HelperConfirmationOutcome::Pending);
        assert_eq!(report.diagnostics[0].attempts.len(), 2);
        assert_eq!(transport.request_count(), 2);
    }

    #[tokio::test]
    async fn cancellation_during_transport_does_not_degrade() {
        let cancellation = HelperStatusCancellation::new();
        let transport =
            FakeTransport::new([(url("one"), vec![FakeResponse::CancelThenError("cancelled")])])
                .with_cancellation(cancellation.clone());
        let health = HelperHealthTracker::with_policy(1, Duration::from_secs(60));

        let report = confirmed_by_any_helper(
            &transport,
            &health,
            &[endpoint("one"), endpoint("two")],
            ROUND_ID,
            SHARE_ID,
            &no_retries(),
            &cancellation,
        )
        .await
        .unwrap();

        assert_eq!(report.outcome, HelperConfirmationOutcome::Cancelled);
        assert_eq!(
            health.candidates(&[endpoint("one"), endpoint("two")]),
            vec![endpoint("one"), endpoint("two")]
        );
        assert_eq!(transport.request_count(), 1);
    }

    #[test]
    fn sdk_constructs_route_from_transport_base_path() {
        assert_eq!(
            helper_share_status_url("https://helper.example/proxy/", ROUND_ID, SHARE_ID).unwrap(),
            format!(
                "https://helper.example/proxy/shielded-vote/v1/share-status/{ROUND_ID}/{SHARE_ID}"
            )
        );
    }
}
