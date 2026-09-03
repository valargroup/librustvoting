//! Vote-chain endpoint construction and wire-protocol interpretation.

use std::{str::FromStr, time::Duration};

use serde::Deserialize;
use url::{Host, Url};

use crate::{
    confirmation::TxEvent,
    types::Network,
    wire::{DelegationSubmissionWire, VoteCommitmentWire},
};

use super::{
    CandidateTransactionHash, ChainHttpRequest, ChainHttpResponse, ChainPostDispatch,
    ChainSubmissionDiagnostic, ChainSubmissionDiagnosticKind, ChainTransport, ChainTransportError,
    ChainTransportFailureKind, MAX_CHAIN_HTTP_RESPONSE_BYTES,
};

const API_PREFIX: [&str; 2] = ["shielded-vote", "v1"];
const DELEGATION_ENDPOINT: &str = "delegate-vote";
const VOTE_ENDPOINT: &str = "cast-vote";
// CheckTx may use its complete 120-second server budget to verify a proof. The
// client default leaves time for connection setup and response delivery.
const MIN_CHAIN_POST_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_CHAIN_POST_TIMEOUT: Duration = Duration::from_secs(150);
const DEFAULT_CHAIN_LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CHAIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_CHAIN_HTTP_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_CHAIN_ENDPOINTS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ChainProtocolTiming {
    post_timeout: Duration,
    lookup_timeout: Duration,
}

impl ChainProtocolTiming {
    pub(super) fn new(
        post_timeout: Duration,
        lookup_timeout: Duration,
    ) -> Result<Self, ChainSubmissionDiagnostic> {
        if !(MIN_CHAIN_POST_TIMEOUT..=MAX_CHAIN_REQUEST_TIMEOUT).contains(&post_timeout) {
            return Err(invalid_protocol(
                "vote-chain POST timeout must be between 120 and 600 seconds",
            ));
        }
        if lookup_timeout.is_zero() || lookup_timeout > MAX_CHAIN_REQUEST_TIMEOUT {
            return Err(invalid_protocol(
                "vote-chain lookup timeout must be between 1 nanosecond and 600 seconds",
            ));
        }
        Ok(Self {
            post_timeout,
            lookup_timeout,
        })
    }
}

impl Default for ChainProtocolTiming {
    fn default() -> Self {
        Self {
            post_timeout: DEFAULT_CHAIN_POST_TIMEOUT,
            lookup_timeout: DEFAULT_CHAIN_LOOKUP_TIMEOUT,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PostAttemptOutcome {
    Accepted(CandidateTransactionHash),
    Rejected {
        code: u32,
        diagnostic: ChainSubmissionDiagnostic,
        candidate_transaction_hash: Option<CandidateTransactionHash>,
    },
    LocalFailure(ChainSubmissionDiagnostic),
    DefinitelyUnsent(ChainTransportError),
    PossiblyDispatched(ChainSubmissionDiagnostic),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CommittedTransaction {
    pub(super) height: u64,
    pub(super) code: u32,
    pub(super) events: Vec<TxEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TransactionStatusObservation {
    Pending,
    CommittedSuccess(CommittedTransaction),
    CommittedFailure(CommittedTransaction),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LookupFailure {
    Transport(ChainTransportError),
    Protocol(ChainSubmissionDiagnostic),
}

pub(super) struct ChainProtocolClient<T> {
    transport: T,
    endpoints: Vec<String>,
    timing: ChainProtocolTiming,
}

impl<T: ChainTransport> ChainProtocolClient<T> {
    pub(super) fn transport(&self) -> &T {
        &self.transport
    }

    pub(super) fn endpoints(&self) -> &[String] {
        &self.endpoints
    }
    pub(super) fn new(
        transport: T,
        network: Network,
        endpoints: &[String],
    ) -> Result<Self, ChainSubmissionDiagnostic> {
        Self::with_timing(
            transport,
            network,
            endpoints,
            ChainProtocolTiming::default(),
        )
    }

    pub(super) fn with_timing(
        transport: T,
        network: Network,
        endpoints: &[String],
        timing: ChainProtocolTiming,
    ) -> Result<Self, ChainSubmissionDiagnostic> {
        ChainProtocolTiming::new(timing.post_timeout, timing.lookup_timeout)?;
        if endpoints.is_empty() {
            return Err(invalid_protocol(
                "vote-chain endpoint set must not be empty",
            ));
        }
        if endpoints.len() > MAX_CHAIN_ENDPOINTS {
            return Err(invalid_protocol(format!(
                "vote-chain endpoint set exceeds {MAX_CHAIN_ENDPOINTS} endpoint limit"
            )));
        }

        let mut canonical = Vec::with_capacity(endpoints.len());
        for endpoint in endpoints {
            let endpoint = canonicalize_chain_base_url(endpoint, network)?;
            if canonical.contains(&endpoint) {
                return Err(invalid_protocol(
                    "vote-chain endpoint set contains a duplicate canonical endpoint",
                ));
            }
            canonical.push(endpoint);
        }

        Ok(Self {
            transport,
            endpoints: canonical,
            timing,
        })
    }

    /// Number of distinct canonical mutation endpoints available for failover.
    pub(super) fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }

    #[cfg(test)]
    pub(super) async fn submit_delegation(
        &self,
        endpoint_index: usize,
        submission: &DelegationSubmissionWire,
    ) -> PostAttemptOutcome {
        self.submit_delegation_with_dispatch(
            endpoint_index,
            submission,
            ChainPostDispatch::default(),
        )
        .await
    }

    pub(super) async fn submit_delegation_with_dispatch(
        &self,
        endpoint_index: usize,
        submission: &DelegationSubmissionWire,
        dispatch: ChainPostDispatch,
    ) -> PostAttemptOutcome {
        let body = match submission.to_json() {
            Ok(json) => json.into_bytes(),
            Err(error) => {
                return PostAttemptOutcome::LocalFailure(invalid_protocol(format!(
                    "serialize delegation request failed: {error}"
                )))
            }
        };
        self.post(endpoint_index, DELEGATION_ENDPOINT, body, dispatch)
            .await
    }

    #[cfg(test)]
    pub(super) async fn submit_vote(
        &self,
        endpoint_index: usize,
        submission: &VoteCommitmentWire,
    ) -> PostAttemptOutcome {
        self.submit_vote_with_dispatch(endpoint_index, submission, ChainPostDispatch::default())
            .await
    }

    pub(super) async fn submit_vote_with_dispatch(
        &self,
        endpoint_index: usize,
        submission: &VoteCommitmentWire,
        dispatch: ChainPostDispatch,
    ) -> PostAttemptOutcome {
        let body = match submission.to_json() {
            Ok(json) => json.into_bytes(),
            Err(error) => {
                return PostAttemptOutcome::LocalFailure(invalid_protocol(format!(
                    "serialize vote request failed: {error}"
                )))
            }
        };
        self.post(endpoint_index, VOTE_ENDPOINT, body, dispatch)
            .await
    }

    async fn post(
        &self,
        endpoint_index: usize,
        endpoint: &str,
        body: Vec<u8>,
        dispatch: ChainPostDispatch,
    ) -> PostAttemptOutcome {
        if body.len() > MAX_CHAIN_HTTP_REQUEST_BYTES {
            return PostAttemptOutcome::LocalFailure(invalid_protocol(format!(
                "vote-chain request exceeds {MAX_CHAIN_HTTP_REQUEST_BYTES} byte limit"
            )));
        }
        let Some(base_url) = self.endpoints.get(endpoint_index) else {
            return PostAttemptOutcome::LocalFailure(invalid_protocol(
                "vote-chain endpoint index is out of range",
            ));
        };
        let request = chain_request(
            join_chain_url(base_url, &[endpoint]),
            true,
            self.timing.post_timeout,
        );

        let response = tokio::time::timeout(
            self.timing.post_timeout,
            self.transport
                .chain_post_json_with_dispatch(request, body, dispatch),
        )
        .await;
        match response {
            Err(_) => PostAttemptOutcome::PossiblyDispatched(
                ChainSubmissionDiagnostic::from_redacted_message(
                    ChainSubmissionDiagnosticKind::AmbiguousDispatch,
                    "vote-chain submission timed out",
                ),
            ),
            Ok(Ok(response)) => parse_post_response(response),
            Ok(Err(error)) if error.kind() == ChainTransportFailureKind::DefinitelyUnsent => {
                PostAttemptOutcome::DefinitelyUnsent(error)
            }
            Ok(Err(error)) => PostAttemptOutcome::PossiblyDispatched(
                ChainSubmissionDiagnostic::from_redacted_message(
                    ChainSubmissionDiagnosticKind::AmbiguousDispatch,
                    error.message(),
                ),
            ),
        }
    }

    pub(super) async fn transaction_status(
        &self,
        transaction_hash: CandidateTransactionHash,
    ) -> Result<TransactionStatusObservation, LookupFailure> {
        let hash = transaction_hash.to_hex();
        let mut saw_pending = false;
        let mut last_failure = None;

        for base_url in &self.endpoints {
            let request = chain_request(
                join_chain_url(base_url, &["tx", &hash]),
                false,
                self.timing.lookup_timeout,
            );
            match tokio::time::timeout(
                self.timing.lookup_timeout,
                self.transport.chain_get(request),
            )
            .await
            {
                Ok(Ok(response)) => match parse_status_response(response) {
                    Ok(TransactionStatusObservation::Pending) => saw_pending = true,
                    Ok(committed) => return Ok(committed),
                    Err(error) => last_failure = Some(LookupFailure::Protocol(error)),
                },
                Ok(Err(error)) => last_failure = Some(LookupFailure::Transport(error)),
                Err(_) => {
                    last_failure = Some(LookupFailure::Transport(
                        ChainTransportError::possibly_dispatched(
                            "vote-chain transaction lookup timed out",
                        ),
                    ));
                }
            }
        }

        if saw_pending {
            Ok(TransactionStatusObservation::Pending)
        } else {
            Err(last_failure.unwrap_or_else(|| {
                LookupFailure::Protocol(invalid_protocol(
                    "vote-chain lookup produced no endpoint result",
                ))
            }))
        }
    }
}

fn chain_request(url: String, has_json_body: bool, timeout: Duration) -> ChainHttpRequest {
    let mut headers = vec![("accept".to_string(), "application/json".to_string())];
    if has_json_body {
        headers.push(("content-type".to_string(), "application/json".to_string()));
    }
    ChainHttpRequest::new(url, headers, timeout, MAX_CHAIN_HTTP_RESPONSE_BYTES)
}

fn parse_post_response(response: ChainHttpResponse) -> PostAttemptOutcome {
    if let Err(diagnostic) = validate_json_response(&response) {
        return PostAttemptOutcome::PossiblyDispatched(diagnostic);
    }
    let parsed: TransactionResultJson = match serde_json::from_slice(response.body()) {
        Ok(parsed) => parsed,
        Err(_) => {
            return PostAttemptOutcome::PossiblyDispatched(invalid_protocol(
                "vote-chain transaction response is malformed JSON",
            ))
        }
    };

    match (response.status(), parsed.code) {
        (200, 0) => match parsed.tx_hash.as_deref().and_then(normalize_candidate_hash) {
            Some(candidate) => PostAttemptOutcome::Accepted(candidate),
            None => PostAttemptOutcome::PossiblyDispatched(invalid_protocol(
                "accepted vote-chain response omitted a canonical transaction hash",
            )),
        },
        (422, code) if code != 0 => {
            let candidate_transaction_hash =
                match parsed.tx_hash.as_deref() {
                    None | Some("") => None,
                    Some(hash) => match normalize_candidate_hash(hash) {
                        Some(hash) => Some(hash),
                        None => return PostAttemptOutcome::PossiblyDispatched(invalid_protocol(
                            "rejected vote-chain response contained an invalid transaction hash",
                        )),
                    },
                };
            PostAttemptOutcome::Rejected {
                code,
                // The response log is server-controlled and may echo proofs,
                // signatures, or the complete request. Keep durable-safe
                // diagnostics limited to the stable numeric result.
                diagnostic: ChainSubmissionDiagnostic::from_redacted_message(
                    ChainSubmissionDiagnosticKind::ChainRejected,
                    format!("vote chain rejected transaction with code {code}"),
                ),
                candidate_transaction_hash,
            }
        }
        (300..=399, _) => PostAttemptOutcome::PossiblyDispatched(invalid_protocol(
            "vote-chain mutation redirect was rejected",
        )),
        (status, code) => PostAttemptOutcome::PossiblyDispatched(invalid_protocol(format!(
            "unsupported vote-chain mutation response status={status} code={code}"
        ))),
    }
}

fn parse_status_response(
    response: ChainHttpResponse,
) -> Result<TransactionStatusObservation, ChainSubmissionDiagnostic> {
    validate_json_response(&response)?;
    if response.status() == 404 {
        let pending: PendingTransactionJson = serde_json::from_slice(response.body())
            .map_err(|_| invalid_protocol("vote-chain pending response is malformed JSON"))?;
        if pending.error != "tx not found" {
            return Err(invalid_protocol(
                "vote-chain pending response has an unsupported error",
            ));
        }
        return Ok(TransactionStatusObservation::Pending);
    }
    if (300..=399).contains(&response.status()) {
        return Err(invalid_protocol(
            "vote-chain transaction lookup redirect was rejected",
        ));
    }
    if !matches!(response.status(), 200 | 422) {
        return Err(invalid_protocol(format!(
            "unsupported vote-chain lookup HTTP status {}",
            response.status()
        )));
    }

    let parsed: TransactionStatusJson = serde_json::from_slice(response.body())
        .map_err(|_| invalid_protocol("vote-chain transaction status is malformed JSON"))?;
    let height = parsed.height.parse::<u64>().map_err(|_| {
        invalid_protocol("vote-chain transaction height is not a u64 decimal string")
    })?;
    let committed = CommittedTransaction {
        height,
        code: parsed.code,
        events: parsed.events,
    };
    match (response.status(), committed.code) {
        (200, 0) => Ok(TransactionStatusObservation::CommittedSuccess(committed)),
        (422, code) if code != 0 => Ok(TransactionStatusObservation::CommittedFailure(committed)),
        (status, code) => Err(invalid_protocol(format!(
            "contradictory vote-chain lookup response status={status} code={code}"
        ))),
    }
}

fn validate_json_response(response: &ChainHttpResponse) -> Result<(), ChainSubmissionDiagnostic> {
    if response.body().len() > MAX_CHAIN_HTTP_RESPONSE_BYTES {
        return Err(invalid_protocol(format!(
            "vote-chain response exceeds {MAX_CHAIN_HTTP_RESPONSE_BYTES} byte limit"
        )));
    }
    let is_json = response.content_type().is_some_and(|content_type| {
        content_type
            .split(';')
            .next()
            .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"))
    });
    if !is_json {
        return Err(invalid_protocol(
            "vote-chain response Content-Type must be application/json",
        ));
    }
    Ok(())
}

fn normalize_candidate_hash(value: &str) -> Option<CandidateTransactionHash> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    CandidateTransactionHash::from_str(&value.to_ascii_lowercase()).ok()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TransactionResultJson {
    #[serde(default)]
    tx_hash: Option<String>,
    code: u32,
    #[serde(default)]
    #[serde(rename = "log")]
    _log: String,
}

#[derive(Deserialize)]
struct TransactionStatusJson {
    height: String,
    code: u32,
    #[serde(rename = "log")]
    _log: String,
    events: Vec<TxEvent>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingTransactionJson {
    error: String,
}

fn canonicalize_chain_base_url(
    value: &str,
    network: Network,
) -> Result<String, ChainSubmissionDiagnostic> {
    let trimmed = value.trim();
    let mut url = Url::parse(trimmed)
        .map_err(|_| invalid_protocol("vote-chain endpoint is not a valid URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid_protocol(
            "vote-chain endpoint must use HTTP or HTTPS",
        ));
    }
    if network == Network::Mainnet && url.scheme() != "https" {
        return Err(invalid_protocol(
            "production vote-chain endpoint must use HTTPS",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid_protocol(
            "vote-chain endpoint must not contain credentials",
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(invalid_protocol(
            "vote-chain endpoint must not contain a query or fragment",
        ));
    }
    if let Some(Host::Domain(domain)) = url.host() {
        if let Some(domain) = domain.strip_suffix('.') {
            let domain = domain.to_string();
            url.set_host(Some(&domain))
                .map_err(|_| invalid_protocol("vote-chain endpoint host is invalid"))?;
        }
    }
    let default_port = match url.scheme() {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    };
    if url.port() == default_port {
        url.set_port(None)
            .map_err(|_| invalid_protocol("vote-chain endpoint port is invalid"))?;
    }
    let path = normalize_percent_escapes(url.path())
        .map_err(|_| invalid_protocol("vote-chain endpoint path has an invalid percent escape"))?;
    url.set_path(&path);
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn normalize_percent_escapes(path: &str) -> Result<String, ()> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut normalized = String::with_capacity(path.len());
    let mut remaining = path;
    while let Some(index) = remaining.find('%') {
        normalized.push_str(&remaining[..index]);
        let bytes = remaining.as_bytes();
        let high = bytes
            .get(index + 1)
            .copied()
            .and_then(hex_value)
            .ok_or(())?;
        let low = bytes
            .get(index + 2)
            .copied()
            .and_then(hex_value)
            .ok_or(())?;
        let value = (high << 4) | low;
        if value.is_ascii_alphanumeric() || matches!(value, b'-' | b'.' | b'_' | b'~') {
            normalized.push(char::from(value));
        } else {
            normalized.push('%');
            normalized.push(char::from(HEX[usize::from(high)]));
            normalized.push(char::from(HEX[usize::from(low)]));
        }
        remaining = &remaining[index + 3..];
    }
    normalized.push_str(remaining);
    Ok(normalized)
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn join_chain_url(base_url: &str, segments: &[&str]) -> String {
    let mut url = base_url.to_string();
    for segment in API_PREFIX.iter().chain(segments) {
        url.push('/');
        url.push_str(segment);
    }
    url
}

fn invalid_protocol(message: impl AsRef<str>) -> ChainSubmissionDiagnostic {
    ChainSubmissionDiagnostic::from_redacted_message(
        ChainSubmissionDiagnosticKind::InvalidProtocolResponse,
        message,
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use super::*;
    use crate::chain_submission::{ChainTransportFuture, MAX_CHAIN_HTTP_RESPONSE_BYTES};

    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[derive(Default)]
    struct ScriptedTransport {
        responses: Mutex<VecDeque<Result<ChainHttpResponse, ChainTransportError>>>,
        calls: Mutex<Vec<(bool, ChainHttpRequest, Vec<u8>)>>,
    }

    struct NeverTransport;

    impl ChainTransport for NeverTransport {
        fn chain_get<'a>(&'a self, _request: ChainHttpRequest) -> ChainTransportFuture<'a> {
            Box::pin(std::future::pending())
        }

        fn chain_post_json<'a>(
            &'a self,
            _request: ChainHttpRequest,
            _json: Vec<u8>,
        ) -> ChainTransportFuture<'a> {
            Box::pin(std::future::pending())
        }
    }

    impl ScriptedTransport {
        fn queue(&self, response: Result<ChainHttpResponse, ChainTransportError>) {
            self.responses.lock().unwrap().push_back(response);
        }
    }

    impl ChainTransport for ScriptedTransport {
        fn chain_get<'a>(&'a self, request: ChainHttpRequest) -> ChainTransportFuture<'a> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push((false, request, Vec::new()));
                self.responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("scripted GET response")
            })
        }

        fn chain_post_json<'a>(
            &'a self,
            request: ChainHttpRequest,
            json: Vec<u8>,
        ) -> ChainTransportFuture<'a> {
            Box::pin(async move {
                self.calls.lock().unwrap().push((true, request, json));
                self.responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("scripted POST response")
            })
        }
    }

    fn protocol_client<T: ChainTransport>(
        transport: T,
        network: Network,
        endpoints: &[&str],
    ) -> ChainProtocolClient<T> {
        ChainProtocolClient::new(
            transport,
            network,
            &endpoints
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    fn delegation() -> DelegationSubmissionWire {
        DelegationSubmissionWire {
            rk: "rk".to_string(),
            spend_auth_sig: "signature".to_string(),
            tx1_effects: "effects".to_string(),
            nf_signed: "nullifier".to_string(),
            cmx_new: "cmx".to_string(),
            gov_comm: "van".to_string(),
            gov_nullifiers: vec!["governance-nullifier".to_string()],
            proof: "proof".to_string(),
            vote_round_id: "round".to_string(),
        }
    }

    fn vote() -> VoteCommitmentWire {
        VoteCommitmentWire {
            van_nullifier: "nullifier".to_string(),
            vote_authority_note_new: "successor".to_string(),
            vote_commitment: "commitment".to_string(),
            proposal_id: 3,
            proof: "proof".to_string(),
            vote_round_id: "round".to_string(),
            anchor_height: 42,
            r_vpk: "verification-key".to_string(),
            vote_auth_sig: "signature".to_string(),
        }
    }

    fn json(status: u16, body: impl AsRef<[u8]>) -> ChainHttpResponse {
        ChainHttpResponse::json(status, body.as_ref().to_vec())
    }

    #[tokio::test]
    async fn constructs_exact_mounted_url_headers_timeout_and_json() {
        let transport = Arc::new(ScriptedTransport::default());
        transport.queue(Ok(json(
            200,
            format!(r#"{{"tx_hash":"{}","code":0}}"#, HASH.to_ascii_uppercase()),
        )));
        let client = protocol_client(
            transport.clone(),
            Network::Mainnet,
            &[" HTTPS://Vote.Example:443/mount/// "],
        );
        let wire = delegation();

        assert_eq!(
            client.submit_delegation(0, &wire).await,
            PostAttemptOutcome::Accepted(CandidateTransactionHash::from_str(HASH).unwrap())
        );
        let calls = transport.calls.lock().unwrap();
        let (is_post, request, body) = &calls[0];
        assert!(*is_post);
        assert_eq!(
            request.url(),
            "https://vote.example/mount/shielded-vote/v1/delegate-vote"
        );
        assert_eq!(
            request.headers(),
            &[
                ("accept".to_string(), "application/json".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ]
        );
        assert_eq!(request.timeout(), Duration::from_secs(150));
        assert_eq!(request.max_response_bytes(), MAX_CHAIN_HTTP_RESPONSE_BYTES);
        assert_eq!(body, wire.to_json().unwrap().as_bytes());
    }

    #[tokio::test]
    async fn constructs_exact_singleton_vote_url_and_json() {
        let transport = Arc::new(ScriptedTransport::default());
        transport.queue(Ok(json(200, format!(r#"{{"tx_hash":"{HASH}","code":0}}"#))));
        let client = protocol_client(
            transport.clone(),
            Network::Testnet,
            &["https://vote.example"],
        );
        let wire = vote();

        assert!(matches!(
            client.submit_vote(0, &wire).await,
            PostAttemptOutcome::Accepted(_)
        ));
        let calls = transport.calls.lock().unwrap();
        assert_eq!(
            calls[0].1.url(),
            "https://vote.example/shielded-vote/v1/cast-vote"
        );
        assert_eq!(calls[0].2, wire.to_json().unwrap().as_bytes());
    }

    #[tokio::test]
    async fn lookup_uses_its_shorter_bounded_deadline() {
        let transport = Arc::new(ScriptedTransport::default());
        transport.queue(Ok(json(404, r#"{"error":"tx not found"}"#)));
        let client = protocol_client(
            transport.clone(),
            Network::Testnet,
            &["https://vote.example"],
        );

        assert_eq!(
            client
                .transaction_status(CandidateTransactionHash::from_str(HASH).unwrap())
                .await
                .unwrap(),
            TransactionStatusObservation::Pending
        );
        assert_eq!(
            transport.calls.lock().unwrap()[0].1.timeout(),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn protocol_timing_rejects_unbounded_or_inadequate_deadlines() {
        assert!(
            ChainProtocolTiming::new(Duration::from_secs(120), Duration::from_nanos(1)).is_ok()
        );
        assert!(
            ChainProtocolTiming::new(Duration::from_secs(600), Duration::from_secs(600)).is_ok()
        );

        for (post_timeout, lookup_timeout) in [
            (Duration::from_secs(119), Duration::from_secs(10)),
            (Duration::from_secs(601), Duration::from_secs(10)),
            (Duration::from_secs(150), Duration::ZERO),
            (Duration::from_secs(150), Duration::from_secs(601)),
        ] {
            assert!(ChainProtocolTiming::new(post_timeout, lookup_timeout).is_err());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn sdk_deadline_bounds_a_post_transport_that_ignores_metadata() {
        let client = protocol_client(
            Arc::new(NeverTransport),
            Network::Testnet,
            &["https://vote.example"],
        );
        let task = tokio::spawn(async move { client.submit_delegation(0, &delegation()).await });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(151)).await;

        assert!(matches!(
            task.await.unwrap(),
            PostAttemptOutcome::PossiblyDispatched(diagnostic)
                if diagnostic.kind() == ChainSubmissionDiagnosticKind::AmbiguousDispatch
                    && diagnostic.message() == "vote-chain submission timed out"
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn sdk_deadline_bounds_a_lookup_transport_that_ignores_metadata() {
        let client = protocol_client(
            Arc::new(NeverTransport),
            Network::Testnet,
            &["https://vote.example"],
        );
        let task = tokio::spawn(async move {
            client
                .transaction_status(CandidateTransactionHash::from_str(HASH).unwrap())
                .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(11)).await;

        assert!(matches!(
            task.await.unwrap(),
            Err(LookupFailure::Transport(error))
                if error.kind() == ChainTransportFailureKind::PossiblyDispatched
                    && error.message() == "vote-chain transaction lookup timed out"
        ));
    }

    #[test]
    fn endpoint_validation_enforces_identity_and_production_https() {
        for endpoint in [
            "http://vote.example",
            "https://user@vote.example",
            "https://vote.example?x=1",
            "https://vote.example#fragment",
            "file:///tmp/vote",
        ] {
            assert!(
                ChainProtocolClient::new(
                    Arc::new(ScriptedTransport::default()),
                    Network::Mainnet,
                    &[endpoint.to_string()],
                )
                .is_err(),
                "{endpoint}"
            );
        }
        assert!(ChainProtocolClient::new(
            Arc::new(ScriptedTransport::default()),
            Network::Testnet,
            &[
                "HTTPS://vote.example:443/mount/".to_string(),
                "https://vote.example/mount".to_string(),
            ],
        )
        .is_err());
        assert!(ChainProtocolClient::new(
            Arc::new(ScriptedTransport::default()),
            Network::Regtest,
            &["http://127.0.0.1:8080".to_string()],
        )
        .is_ok());
        let maximum_endpoint_set = (0..MAX_CHAIN_ENDPOINTS)
            .map(|index| format!("https://vote-{index}.example"))
            .collect::<Vec<_>>();
        assert!(ChainProtocolClient::new(
            Arc::new(ScriptedTransport::default()),
            Network::Testnet,
            &maximum_endpoint_set,
        )
        .is_ok());
        assert!(ChainProtocolClient::new(
            Arc::new(ScriptedTransport::default()),
            Network::Testnet,
            &(0..=MAX_CHAIN_ENDPOINTS)
                .map(|index| format!("https://vote-{index}.example"))
                .collect::<Vec<_>>(),
        )
        .is_err());
    }

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
            diagnostic,
            candidate_transaction_hash,
        } = outcome
        else {
            panic!("expected deterministic rejection");
        };
        assert_eq!(code, 7);
        assert_eq!(
            diagnostic.kind(),
            ChainSubmissionDiagnosticKind::ChainRejected
        );
        assert_eq!(
            candidate_transaction_hash,
            Some(CandidateTransactionHash::from_str(HASH).unwrap())
        );
        assert_eq!(
            diagnostic.message(),
            "vote chain rejected transaction with code 7"
        );
        assert!(!diagnostic.message().contains("round closed"));
    }

    #[tokio::test]
    async fn rejection_diagnostic_does_not_retain_server_log_material() {
        let transport = Arc::new(ScriptedTransport::default());
        let secret = "proof-and-signature-from-submitted-request";
        transport.queue(Ok(json(422, format!(r#"{{"code":7,"log":"{secret}"}}"#))));
        let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);

        let PostAttemptOutcome::Rejected { diagnostic, .. } =
            client.submit_delegation(0, &delegation()).await
        else {
            panic!("expected deterministic rejection");
        };
        assert!(!diagnostic.message().contains(secret));
        assert_eq!(
            diagnostic.message(),
            "vote chain rejected transaction with code 7"
        );
    }

    #[tokio::test]
    async fn every_uncertain_post_result_stops_after_one_request() {
        let cases = [
            Ok(json(200, r#"{"code":0}"#)),
            Ok(json(422, r#"{"code":0}"#)),
            Ok(json(302, r#"{"location":"elsewhere"}"#)),
            Ok(json(200, b"not-json")),
            Ok(ChainHttpResponse::json(
                200,
                vec![b'x'; MAX_CHAIN_HTTP_RESPONSE_BYTES + 1],
            )),
            Ok(ChainHttpResponse::new(
                200,
                br#"{"tx_hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","code":0}"#.to_vec(),
                Some("text/plain".to_string()),
                Vec::new(),
            )),
            Err(ChainTransportError::possibly_dispatched("timeout")),
        ];

        for response in cases {
            let transport = Arc::new(ScriptedTransport::default());
            transport.queue(response);
            let client = protocol_client(
                transport.clone(),
                Network::Testnet,
                &["https://one.example", "https://two.example"],
            );
            assert!(matches!(
                client.submit_delegation(0, &delegation()).await,
                PostAttemptOutcome::PossiblyDispatched(_)
            ));
            assert_eq!(transport.calls.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn oversized_request_is_rejected_before_transport() {
        let transport = Arc::new(ScriptedTransport::default());
        let client = protocol_client(
            transport.clone(),
            Network::Testnet,
            &["https://vote.example"],
        );
        let mut wire = delegation();
        wire.proof = "x".repeat(MAX_CHAIN_HTTP_REQUEST_BYTES + 1);

        assert!(matches!(
            client.submit_delegation(0, &wire).await,
            PostAttemptOutcome::LocalFailure(_)
        ));
        assert!(transport.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn only_transport_can_report_definitely_unsent() {
        let transport = Arc::new(ScriptedTransport::default());
        transport.queue(Err(ChainTransportError::definitely_unsent(
            "privacy route unavailable",
        )));
        let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);

        assert!(matches!(
            client.submit_delegation(0, &delegation()).await,
            PostAttemptOutcome::DefinitelyUnsent(error) if error.is_definitely_unsent()
        ));
    }

    #[tokio::test]
    async fn lookup_prefers_later_committed_evidence_over_pending() {
        let transport = Arc::new(ScriptedTransport::default());
        transport.queue(Ok(json(404, r#"{"error":"tx not found"}"#)));
        transport.queue(Ok(json(
            200,
            r#"{"height":"42","code":0,"log":"","events":[{"type":"delegate_vote","attributes":[{"key":"leaf_index","value":"7","index":true}]}]}"#,
        )));
        let client = protocol_client(
            transport.clone(),
            Network::Testnet,
            &["https://one.example", "https://two.example"],
        );
        let hash = CandidateTransactionHash::from_str(HASH).unwrap();

        let result = client.transaction_status(hash).await.unwrap();
        let TransactionStatusObservation::CommittedSuccess(committed) = result else {
            panic!("expected committed success");
        };
        assert_eq!(committed.height, 42);
        assert_eq!(committed.events[0].event_type, "delegate_vote");
        assert_eq!(committed.events[0].attributes[0].value, "7");
        let calls = transport.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0].1.url(),
            format!("https://one.example/shielded-vote/v1/tx/{HASH}")
        );
    }

    #[tokio::test]
    async fn lookup_accepts_committed_failure_and_rejects_contradictions() {
        let transport = Arc::new(ScriptedTransport::default());
        transport.queue(Ok(json(
            422,
            r#"{"height":"43","code":9,"log":"execution failed","events":[]}"#,
        )));
        let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);
        let hash = CandidateTransactionHash::from_str(HASH).unwrap();
        assert!(matches!(
            client.transaction_status(hash).await.unwrap(),
            TransactionStatusObservation::CommittedFailure(CommittedTransaction {
                height: 43,
                code: 9,
                ..
            })
        ));

        let transport = Arc::new(ScriptedTransport::default());
        transport.queue(Ok(json(
            200,
            r#"{"height":"44","code":8,"log":"contradiction","events":[]}"#,
        )));
        let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);
        assert!(matches!(
            client.transaction_status(hash).await,
            Err(LookupFailure::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn malformed_or_oversized_lookup_response_is_not_evidence() {
        let oversized = vec![b'x'; MAX_CHAIN_HTTP_RESPONSE_BYTES + 1];
        for response in [
            json(200, r#"{"height":42,"code":0,"log":"","events":[]}"#),
            json(200, r#"{"height":"42","code":0,"log":""}"#),
            json(404, "null"),
            json(404, r#"{"error":"not found"}"#),
            json(404, r#"{"error":"tx not found","extra":true}"#),
            ChainHttpResponse::json(200, oversized.clone()),
            ChainHttpResponse::new(200, b"not json".to_vec(), None, Vec::new()),
            json(307, r#"{"redirect":true}"#),
        ] {
            let transport = Arc::new(ScriptedTransport::default());
            transport.queue(Ok(response));
            let client = protocol_client(transport, Network::Testnet, &["https://vote.example"]);
            assert!(matches!(
                client
                    .transaction_status(CandidateTransactionHash::from_str(HASH).unwrap())
                    .await,
                Err(LookupFailure::Protocol(_))
            ));
        }
    }
}
