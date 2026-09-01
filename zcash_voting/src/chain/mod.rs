//! Vote-chain REST client.
//!
//! This module owns mutation and transaction-status protocol mapping. Durable
//! voting-state transitions live in [`crate::chain_submission`].

pub mod transport;

use std::{sync::Arc, time::Duration};

use serde::Deserialize;

#[cfg(test)]
use crate::wire::DelegationSubmissionWire;

use crate::{
    confirmation::TxEvent,
    helper::{transport::HelperTransportError, url::canonicalize_server_base_url},
    types::VotingError,
};
use transport::{ChainResponse, ChainTransport, MAX_CHAIN_RESPONSE_BYTES};

const API_PREFIX: [&str; 2] = ["shielded-vote", "v1"];
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest request deadline a host may configure.
///
/// An attempt reservation stays `attempting` from the moment it is journaled
/// until the response to its POST is classified, so this deadline bounds how long
/// one can be in flight. `chain_submission` relies on that bound to tell a
/// reservation another process may still be waiting on from one whose process is
/// gone, and it cannot see this configuration: the client is per-call, while the
/// database is shared. Capping the deadline here keeps the two in a fixed
/// relationship rather than leaving a host able to configure the distinction
/// away. Five minutes is far beyond any workable deadline for a single HTTP
/// request; the default is ten seconds.
pub(crate) const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const DEFAULT_RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(4)];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainEndpointSet {
    endpoints: Vec<String>,
}

impl ChainEndpointSet {
    pub fn new(endpoints: &[String]) -> Result<Self, VotingError> {
        if endpoints.is_empty() {
            return Err(VotingError::InvalidInput {
                message: "vote-chain endpoint set must not be empty".to_string(),
            });
        }
        let mut canonical = Vec::with_capacity(endpoints.len());
        for endpoint in endpoints {
            let endpoint = canonicalize_server_base_url(endpoint, "vote-chain")?;
            if canonical.contains(&endpoint) {
                return Err(VotingError::InvalidInput {
                    message: "vote-chain endpoint set contains duplicate canonical identities"
                        .to_string(),
                });
            }
            canonical.push(endpoint);
        }
        Ok(Self {
            endpoints: canonical,
        })
    }

    pub fn as_slice(&self) -> &[String] {
        &self.endpoints
    }
}

#[derive(Clone, Debug)]
pub struct ChainClientConfig {
    request_timeout: Duration,
    retry_delays: Vec<Duration>,
}

impl Default for ChainClientConfig {
    fn default() -> Self {
        Self {
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            retry_delays: DEFAULT_RETRY_DELAYS.to_vec(),
        }
    }
}

impl ChainClientConfig {
    pub fn with_request_timeout(mut self, timeout: Duration) -> Result<Self, VotingError> {
        validate_duration(timeout, "request_timeout")?;
        if timeout > MAX_REQUEST_TIMEOUT {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "request_timeout must be at most {} seconds",
                    MAX_REQUEST_TIMEOUT.as_secs()
                ),
            });
        }
        self.request_timeout = timeout;
        Ok(self)
    }

    pub fn with_retry_delays(mut self, delays: Vec<Duration>) -> Result<Self, VotingError> {
        if delays.len() > DEFAULT_RETRY_DELAYS.len() {
            return Err(VotingError::InvalidInput {
                message: "chain retry_delays supports at most two backoffs".to_string(),
            });
        }
        for (index, delay) in delays.iter().copied().enumerate() {
            validate_duration(delay, &format!("retry_delays[{index}]"))?;
        }
        self.retry_delays = delays;
        Ok(self)
    }
}

fn validate_duration(duration: Duration, name: &str) -> Result<(), VotingError> {
    if duration.is_zero() {
        return Err(VotingError::InvalidInput {
            message: format!("{name} must be nonzero"),
        });
    }
    if tokio::time::Instant::now().checked_add(duration).is_none() {
        return Err(VotingError::InvalidInput {
            message: format!("{name} is too large for Tokio's monotonic clock"),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainTxResult {
    pub tx_hash: String,
    pub code: u32,
    pub log: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainBroadcastOutcome {
    Accepted(ChainTxResult),
    Rejected(ChainTxResult),
    OutcomeUnknown { message: String },
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainTxConfirmation {
    pub height: u64,
    pub code: u32,
    pub log: String,
    pub events: Vec<TxEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainTxStatus {
    Pending,
    Committed(ChainTxConfirmation),
}

#[derive(Clone, Debug)]
pub enum ChainError {
    InvalidRequest(String),
    Transport(HelperTransportError),
    Status(u16),
    Decode(String),
    Cancelled,
}

impl ChainError {
    pub(crate) fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Transport(_) | Self::Status(429 | 500 | 502 | 503 | 504) | Self::Decode(_)
        )
    }

    pub(crate) fn is_ambiguous(&self) -> bool {
        matches!(
            self,
            Self::Transport(
                HelperTransportError::Timeout
                    | HelperTransportError::Ambiguous(_)
                    | HelperTransportError::Response(_)
            ) | Self::Status(500..=599)
                | Self::Decode(_)
        )
    }
}

impl std::fmt::Display for ChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(message) => write!(f, "invalid chain request: {message}"),
            Self::Transport(error) => write!(f, "chain transport failed: {error}"),
            Self::Status(status) => write!(f, "vote chain returned HTTP {status}"),
            Self::Decode(message) => write!(f, "vote-chain response was not usable: {message}"),
            Self::Cancelled => write!(f, "chain request cancelled"),
        }
    }
}

impl std::error::Error for ChainError {}

#[derive(Clone)]
pub struct ChainClient {
    transport: Arc<dyn ChainTransport>,
    endpoints: ChainEndpointSet,
    config: ChainClientConfig,
}

impl ChainClient {
    pub fn new(transport: Arc<dyn ChainTransport>, endpoints: ChainEndpointSet) -> Self {
        Self::with_config(transport, endpoints, ChainClientConfig::default())
    }

    pub fn with_config(
        transport: Arc<dyn ChainTransport>,
        endpoints: ChainEndpointSet,
        config: ChainClientConfig,
    ) -> Self {
        Self {
            transport,
            endpoints,
            config,
        }
    }

    #[cfg(test)]
    async fn submit_delegation(
        &self,
        submission: &DelegationSubmissionWire,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainBroadcastOutcome, ChainError> {
        let body = submission
            .to_json()
            .map_err(|error| ChainError::InvalidRequest(error.to_string()))?
            .into_bytes();
        self.broadcast("delegate-vote", body, cancel).await
    }

    pub async fn transaction_status(
        &self,
        tx_hash: &str,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainTxStatus, ChainError> {
        self.transaction_status_where(tx_hash, cancel, &|_| true)
            .await
    }

    /// [`transaction_status`](Self::transaction_status) with a caller-supplied
    /// check on a committed result.
    ///
    /// A committed response that parses is still only a candidate answer: its
    /// events may describe some other submission entirely, which a caller that
    /// knows the submission can tell and this client cannot. Rejecting one here
    /// rather than after the lookup keeps that judgement inside endpoint
    /// failover — otherwise the first endpoint to return a structurally valid
    /// but unrelated confirmation ends the search, and stable endpoint ordering
    /// repeats that outcome on every later call while another configured
    /// endpoint could have served the real one.
    ///
    /// `accepts` sees committed failures too, and should generally admit them:
    /// a nonzero code is definite evidence about the transaction whatever its
    /// events say.
    pub async fn transaction_status_where(
        &self,
        tx_hash: &str,
        cancel: &(dyn Fn() -> bool + Send + Sync),
        accepts: &(dyn Fn(&ChainTxConfirmation) -> bool + Send + Sync),
    ) -> Result<ChainTxStatus, ChainError> {
        let tx_hash = normalize_tx_hash(tx_hash)?;
        let mut saw_pending = false;
        let mut unusable_commit = None;
        let mut last_error = None;
        for endpoint in self.endpoints.as_slice() {
            if cancel() {
                return Err(ChainError::Cancelled);
            }
            let url = join_url(endpoint, &["tx", &tx_hash])?;
            match self.get(&url).await {
                // A 404 is protocol evidence that the transaction is not yet
                // committed, so it has to meet the same body-size and
                // content-type rules as any other response. A reverse proxy's
                // HTML wrong-route page would otherwise make a broken endpoint
                // look indefinitely uncommitted.
                Ok(response) if response.status() == 404 => {
                    match validate_json_response(&response) {
                        Ok(()) => saw_pending = true,
                        Err(error) => last_error = Some(error),
                    }
                }
                Ok(response) if response.status() == 200 || response.status() == 422 => {
                    let status = response.status();
                    match validate_json_response(&response)
                        .and_then(|()| parse_confirmation(response))
                        .and_then(|confirmation| {
                            // The protocol defines 422 as committed failure. A
                            // 422 body claiming success contradicts its own
                            // status, and an error response must never be able
                            // to mutate confirmed voting state.
                            if status == 422 && confirmation.code == 0 {
                                Err(ChainError::Decode(
                                    "HTTP 422 transaction result reported a success code"
                                        .to_string(),
                                ))
                            } else if !accepts(&confirmation) {
                                Err(ChainError::Decode(
                                    "transaction result does not describe this submission"
                                        .to_string(),
                                ))
                            } else {
                                Ok(confirmation)
                            }
                        }) {
                        Ok(confirmation) => return Ok(ChainTxStatus::Committed(confirmation)),
                        // An unusable response is not confirmation, and it is not
                        // a reason to stop looking. Keep failing over so one
                        // malformed endpoint cannot permanently hide a committed
                        // result that another endpoint can still serve.
                        Err(error) => unusable_commit.get_or_insert(error),
                    };
                }
                Ok(response) => last_error = Some(ChainError::Status(response.status())),
                Err(error) => last_error = Some(ChainError::Transport(error)),
            }
        }
        // A transaction-response endpoint that could not be decoded is stronger
        // evidence than another endpoint's 404: reporting `Pending` here would
        // assert "not yet committed" on an endpoint that said otherwise.
        if let Some(error) = unusable_commit {
            return Err(error);
        }
        if saw_pending {
            Ok(ChainTxStatus::Pending)
        } else {
            Err(last_error.unwrap_or_else(|| ChainError::Decode("no endpoint result".to_string())))
        }
    }

    pub(crate) fn retry_delays(&self) -> &[Duration] {
        &self.config.retry_delays
    }

    pub(crate) fn endpoint_count(&self) -> usize {
        self.endpoints.as_slice().len()
    }

    pub(crate) async fn post_once(
        &self,
        endpoint_index: usize,
        endpoint: &str,
        body: Vec<u8>,
    ) -> Result<ChainBroadcastOutcome, ChainError> {
        let base = &self.endpoints.as_slice()[endpoint_index % self.endpoint_count()];
        let url = join_url(base, &[endpoint])?;
        self.post_json(&url, body)
            .await
            .and_then(parse_broadcast_response)
    }

    #[cfg(test)]
    async fn broadcast(
        &self,
        endpoint: &str,
        body: Vec<u8>,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ChainBroadcastOutcome, ChainError> {
        let attempts = self.config.retry_delays.len() + 1;
        let mut last_error = None;
        for attempt in 0..attempts {
            if cancel() {
                return Ok(ChainBroadcastOutcome::Cancelled);
            }
            let base = &self.endpoints.as_slice()[attempt % self.endpoints.as_slice().len()];
            let url = join_url(base, &[endpoint])?;
            let result = self
                .post_json(&url, body.clone())
                .await
                .and_then(parse_broadcast_response);
            match result {
                Ok(outcome) => return Ok(outcome),
                Err(error) => {
                    let ambiguous = error.is_ambiguous();
                    let retryable = error.is_retryable();
                    if attempt + 1 == attempts || !retryable {
                        return if ambiguous {
                            Ok(ChainBroadcastOutcome::OutcomeUnknown {
                                message: error.to_string(),
                            })
                        } else {
                            Err(error)
                        };
                    }
                    last_error = Some(error);
                    if cancel() {
                        return Ok(ChainBroadcastOutcome::Cancelled);
                    }
                    tokio::time::sleep(self.config.retry_delays[attempt]).await;
                }
            }
        }
        Err(last_error.unwrap_or_else(|| ChainError::Decode("retry loop exited".to_string())))
    }

    async fn get(&self, url: &str) -> Result<ChainResponse, HelperTransportError> {
        let timeout = self.config.request_timeout;
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or(HelperTransportError::Timeout)?;
        tokio::time::timeout_at(deadline, self.transport.get(url, timeout))
            .await
            .map_err(|_| HelperTransportError::Timeout)?
    }

    async fn post_json(&self, url: &str, body: Vec<u8>) -> Result<ChainResponse, ChainError> {
        let timeout = self.config.request_timeout;
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or(ChainError::Transport(HelperTransportError::Timeout))?;
        tokio::time::timeout_at(deadline, self.transport.post_json(url, body, timeout))
            .await
            .map_err(|_| ChainError::Transport(HelperTransportError::Timeout))?
            .map_err(ChainError::Transport)
    }
}

#[derive(Deserialize)]
struct TxResultJson {
    #[serde(default)]
    tx_hash: String,
    code: u32,
    #[serde(default)]
    log: String,
}

#[derive(Deserialize)]
struct TxConfirmationJson {
    height: u64,
    code: u32,
    #[serde(default)]
    log: String,
    #[serde(default)]
    events: Vec<TxEvent>,
}

fn parse_broadcast_response(response: ChainResponse) -> Result<ChainBroadcastOutcome, ChainError> {
    if response.status() != 422 && !response.is_success() {
        return Err(ChainError::Status(response.status()));
    }
    let status = response.status();
    validate_json_response(&response)?;
    let parsed: TxResultJson = serde_json::from_slice(response.body())
        .map_err(|_| ChainError::Decode("invalid transaction result JSON".to_string()))?;
    // 422 denotes failure, so a body reporting success contradicts its own
    // status. Accepting it would journal an `accepted` attempt and stop retries
    // for a transaction that was never accepted. Same rule as the lookup path.
    if status == 422 && parsed.code == 0 {
        return Err(ChainError::Decode(
            "HTTP 422 transaction result reported a success code".to_string(),
        ));
    }
    // A hash the server returned is response data, not caller input, so an
    // unusable one is classified by what the rest of the response says.
    let tx_hash = match normalize_tx_hash(&parsed.tx_hash) {
        Ok(hash) => hash,
        // Acceptance without a usable hash leaves the outcome unknown: the
        // transaction may well be in the mempool under a hash we cannot learn.
        // Report an unusable response so retry and failover continue, rather
        // than a rejection that would stop them.
        Err(_) if parsed.code == 0 => {
            return Err(ChainError::Decode(
                "accepted transaction result did not return a usable tx_hash".to_string(),
            ))
        }
        // A rejection is definite on its own evidence, and a rejected
        // duplicate's hash never identified the earlier transaction anyway.
        // Drop the unusable hash and keep the code and log, which the
        // spent-nullifier classifier still needs.
        Err(_) => String::new(),
    };
    let result = ChainTxResult {
        tx_hash,
        code: parsed.code,
        log: parsed.log,
    };
    if result.code == 0 {
        Ok(ChainBroadcastOutcome::Accepted(result))
    } else {
        Ok(ChainBroadcastOutcome::Rejected(result))
    }
}

fn parse_confirmation(response: ChainResponse) -> Result<ChainTxConfirmation, ChainError> {
    let parsed: TxConfirmationJson = serde_json::from_slice(response.body())
        .map_err(|_| ChainError::Decode("invalid transaction confirmation JSON".to_string()))?;
    Ok(ChainTxConfirmation {
        height: parsed.height,
        code: parsed.code,
        log: parsed.log,
        events: parsed.events,
    })
}

fn validate_json_response(response: &ChainResponse) -> Result<(), ChainError> {
    if response.body().len() > MAX_CHAIN_RESPONSE_BYTES {
        return Err(ChainError::Decode(format!(
            "response exceeds {MAX_CHAIN_RESPONSE_BYTES} byte limit"
        )));
    }
    let is_json = response.content_type().is_some_and(|content_type| {
        content_type
            .split(';')
            .next()
            .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"))
    });
    if !is_json {
        return Err(ChainError::Decode(
            "response Content-Type must be application/json".to_string(),
        ));
    }
    Ok(())
}

/// Whether a value is exactly a chain transaction hash.
///
/// One rule, used by both the client and the storage boundary. Deliberately
/// exact: an earlier version trimmed surrounding whitespace here while the
/// storage canonicalizer did not, so a padded legacy row was rejected as opaque
/// at rest but accepted as a reconciliation candidate, and confirming it would
/// then conflict with the padded stored value instead of advancing the domain
/// row.
pub(crate) fn is_tx_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Validates a transaction hash and returns its canonical lowercase form.
pub fn normalize_tx_hash(value: &str) -> Result<String, ChainError> {
    if is_tx_hash(value) {
        Ok(value.to_ascii_lowercase())
    } else {
        Err(ChainError::InvalidRequest(
            "transaction hash must be exactly 64 hexadecimal characters".to_string(),
        ))
    }
}

/// Canonical storage form of a transaction hash.
///
/// Hexadecimal casing carries no meaning, so one transaction must not be stored,
/// compared, and reconciled as two. Anything that is not a chain transaction
/// hash is returned unchanged, so opaque legacy identifiers keep their exact
/// stored meaning; [`known_hashes`](crate::chain_submission) then skips them
/// rather than failing reconciliation.
pub(crate) fn canonical_tx_hash(value: &str) -> String {
    if is_tx_hash(value) {
        value.to_ascii_lowercase()
    } else {
        value.to_string()
    }
}

fn join_url(base: &str, segments: &[&str]) -> Result<String, ChainError> {
    let mut url = canonicalize_server_base_url(base, "vote-chain")
        .map_err(|error| ChainError::InvalidRequest(error.to_string()))?;
    for segment in API_PREFIX.iter().chain(segments.iter()) {
        url.push('/');
        url.push_str(segment);
    }
    Ok(url)
}

pub(crate) fn is_spent_nullifier_log(log: &str) -> bool {
    let lower = log.to_ascii_lowercase();
    lower.contains("nullifier already spent:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, sync::Mutex};

    #[derive(Default)]
    struct MockTransport {
        responses: Mutex<VecDeque<Result<ChainResponse, HelperTransportError>>>,
        posts: Mutex<Vec<(String, Vec<u8>)>>,
    }

    impl ChainTransport for MockTransport {
        fn get<'a>(&'a self, _url: &'a str, _timeout: Duration) -> transport::ChainFuture<'a> {
            Box::pin(async move {
                self.responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("mock response")
            })
        }

        fn post_json<'a>(
            &'a self,
            url: &'a str,
            body: Vec<u8>,
            _timeout: Duration,
        ) -> transport::ChainFuture<'a> {
            Box::pin(async move {
                self.posts.lock().unwrap().push((url.to_string(), body));
                self.responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("mock response")
            })
        }
    }

    fn response(status: u16, body: &str) -> ChainResponse {
        ChainResponse::new(
            status,
            body.as_bytes().to_vec(),
            Some("application/json".into()),
        )
    }

    fn delegation_wire() -> DelegationSubmissionWire {
        DelegationSubmissionWire {
            rk: "rk".into(),
            spend_auth_sig: "sig".into(),
            tx1_effects: "effects".into(),
            nf_signed: "nf".into(),
            cmx_new: "cmx".into(),
            gov_comm: "gov".into(),
            gov_nullifiers: vec!["n".into()],
            proof: "proof".into(),
            vote_round_id: "round".into(),
        }
    }

    #[tokio::test]
    async fn retries_send_byte_identical_canonical_json() {
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().extend([
            Ok(response(503, r#"{"message":"busy"}"#)),
            Ok(response(
                200,
                r#"{"tx_hash":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","code":0,"log":""}"#,
            )),
        ]);
        let endpoints = ChainEndpointSet::new(&[
            "https://one.example/".to_string(),
            "https://two.example".to_string(),
        ])
        .unwrap();
        let config = ChainClientConfig::default()
            .with_retry_delays(vec![Duration::from_millis(1)])
            .unwrap();
        let client = ChainClient::with_config(transport.clone(), endpoints, config);

        let outcome = client
            .submit_delegation(&delegation_wire(), &|| false)
            .await
            .unwrap();

        assert!(matches!(outcome, ChainBroadcastOutcome::Accepted(_)));
        let posts = transport.posts.lock().unwrap();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0].1, posts[1].1);
        assert!(posts[0].0.starts_with("https://one.example/"));
        assert!(posts[1].0.starts_with("https://two.example/"));
    }

    #[test]
    fn request_timeout_is_bounded_so_a_reservation_cannot_outlive_the_grace() {
        ChainClientConfig::default()
            .with_request_timeout(MAX_REQUEST_TIMEOUT)
            .expect("the cap itself is configurable");
        let error = ChainClientConfig::default()
            .with_request_timeout(MAX_REQUEST_TIMEOUT + Duration::from_secs(1))
            .unwrap_err();
        assert!(error.to_string().contains("at most 300 seconds"), "{error}");
        // `chain_submission` decides that a reservation untouched for longer
        // than its grace period cannot be in flight anywhere. That holds only
        // while no configurable deadline can keep one `attempting` for longer.
        assert!(
            i64::try_from(MAX_REQUEST_TIMEOUT.as_secs()).unwrap()
                < crate::chain_submission::interrupted_reservation_grace_secs(),
        );
    }

    #[test]
    fn endpoint_set_rejects_duplicate_canonical_identity() {
        let error = ChainEndpointSet::new(&[
            "https://vote.example".to_string(),
            "https://vote.example/".to_string(),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("duplicate canonical"));
    }

    #[test]
    fn spent_nullifier_classifier_is_narrow_and_case_insensitive() {
        assert!(is_spent_nullifier_log("Nullifier already spent: abcd"));
        assert!(!is_spent_nullifier_log("unrelated nullifier failure"));
    }

    const LOOKUP_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn typed_response(status: u16, body: &str, content_type: &str) -> ChainResponse {
        ChainResponse::new(
            status,
            body.as_bytes().to_vec(),
            Some(content_type.to_string()),
        )
    }

    fn two_endpoint_client(transport: Arc<MockTransport>) -> ChainClient {
        ChainClient::new(
            transport,
            ChainEndpointSet::new(&[
                "https://one.example".to_string(),
                "https://two.example".to_string(),
            ])
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn unusable_lookup_response_fails_over_to_next_endpoint() {
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().extend([
            Ok(typed_response(200, "<html>proxy error</html>", "text/html")),
            Ok(response(
                200,
                r#"{"height":9,"code":0,"log":"","events":[]}"#,
            )),
        ]);
        let client = two_endpoint_client(transport);

        let status = client
            .transaction_status(LOOKUP_HASH, &|| false)
            .await
            .unwrap();

        assert!(
            matches!(status, ChainTxStatus::Committed(confirmation) if confirmation.height == 9),
            "a malformed first endpoint must not hide a committed result"
        );
    }

    #[tokio::test]
    async fn unusable_lookup_response_is_not_reported_as_pending() {
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().extend([
            Ok(response(404, r#"{"message":"not indexed"}"#)),
            Ok(response(200, "{not json")),
        ]);
        let client = two_endpoint_client(transport);

        let error = client
            .transaction_status(LOOKUP_HASH, &|| false)
            .await
            .unwrap_err();

        // One endpoint answered with a transaction response we could not read.
        // Reporting `Pending` would assert "not yet committed" against that
        // endpoint's own evidence.
        assert!(matches!(error, ChainError::Decode(_)), "got {error:?}");
        assert!(error.is_ambiguous());
    }

    #[tokio::test]
    async fn a_malformed_404_is_not_accepted_as_protocol_evidence() {
        let transport = Arc::new(MockTransport::default());
        transport
            .responses
            .lock()
            .unwrap()
            .push_back(Ok(typed_response(
                404,
                "<html>bad route</html>",
                "text/html",
            )));
        let client = ChainClient::new(
            transport,
            ChainEndpointSet::new(&["https://one.example".to_string()]).unwrap(),
        );

        let error = client
            .transaction_status(LOOKUP_HASH, &|| false)
            .await
            .unwrap_err();

        // A reverse proxy's wrong-route page is not the chain saying "not yet
        // committed"; treating it as one would make a broken endpoint look
        // indefinitely uncommitted.
        assert!(matches!(error, ChainError::Decode(_)), "got {error:?}");
    }

    #[tokio::test]
    async fn a_422_lookup_claiming_success_is_unusable() {
        let transport = Arc::new(MockTransport::default());
        transport.responses.lock().unwrap().push_back(Ok(response(
            422,
            r#"{"height":9,"code":0,"log":"","events":[]}"#,
        )));
        let client = ChainClient::new(
            transport,
            ChainEndpointSet::new(&["https://one.example".to_string()]).unwrap(),
        );

        let error = client
            .transaction_status(LOOKUP_HASH, &|| false)
            .await
            .unwrap_err();

        // 422 is committed failure by protocol. A body claiming success
        // contradicts its own status, and an error response must never be able
        // to mutate confirmed voting state.
        assert!(matches!(error, ChainError::Decode(_)), "got {error:?}");
    }

    #[test]
    fn a_422_broadcast_claiming_success_is_unusable() {
        let error = parse_broadcast_response(response(
            422,
            &format!(r#"{{"tx_hash":"{LOOKUP_HASH}","code":0,"log":""}}"#),
        ))
        .unwrap_err();

        // Accepting it would journal an `accepted` attempt and stop retries for
        // a transaction the endpoint's own status says was not accepted.
        assert!(matches!(error, ChainError::Decode(_)), "got {error:?}");
        assert!(error.is_ambiguous());
    }

    #[test]
    fn the_hash_rule_is_exact_and_shared_with_storage() {
        let padded = format!(" {LOOKUP_HASH} ");
        // Trimming here while the storage boundary requires an exact length
        // would accept a padded legacy row as a candidate and then confirm a
        // hash that conflicts with the padded stored value.
        assert!(normalize_tx_hash(&padded).is_err());
        assert_eq!(canonical_tx_hash(&padded), padded);
        assert!(!is_tx_hash(&padded));

        assert_eq!(
            normalize_tx_hash(&LOOKUP_HASH.to_ascii_uppercase()).unwrap(),
            LOOKUP_HASH
        );
        assert_eq!(
            canonical_tx_hash(&LOOKUP_HASH.to_ascii_uppercase()),
            LOOKUP_HASH
        );
        assert_eq!(canonical_tx_hash("legacy-hash"), "legacy-hash");
    }

    #[test]
    fn accepted_result_with_unusable_hash_is_ambiguous() {
        let error = parse_broadcast_response(response(
            200,
            r#"{"tx_hash":"not-a-hash","code":0,"log":""}"#,
        ))
        .unwrap_err();

        assert!(matches!(error, ChainError::Decode(_)), "got {error:?}");
        assert!(error.is_ambiguous() && error.is_retryable());
    }

    #[test]
    fn rejected_result_with_unusable_hash_stays_definite_and_keeps_its_log() {
        let outcome = parse_broadcast_response(response(
            200,
            r#"{"tx_hash":"not-a-hash","code":9,"log":"nullifier already spent: ab"}"#,
        ))
        .unwrap();

        // The rejection is definite on its own evidence, and the log must
        // survive so the spent-nullifier classifier still runs.
        let ChainBroadcastOutcome::Rejected(result) = outcome else {
            panic!("expected a definite rejection, got {outcome:?}");
        };
        assert_eq!(result.code, 9);
        assert!(result.tx_hash.is_empty());
        assert!(is_spent_nullifier_log(&result.log));
    }
}
