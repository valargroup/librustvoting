//! PIR endpoint selection and client re-exports.
//!
//! Wallets use this module to select an exact-height PIR snapshot endpoint
//! before delegation PIR precomputation. [`connect_pir_blocking`] /
//! [`connect_pir`] bind a caller-chosen URL to an explicit [`PirLayout`] for
//! the config/server handshake (tree split and YPIR degree). Neither path
//! hardcodes depth, tier-split, or YPIR degree constants, and neither checks
//! whether the URL appears in a resolved config's advertised endpoint list.

use std::{sync::mpsc, sync::Arc, thread};

use crate::backend::pasta_curves::pallas::Base as Fp;
use crate::config::{validate_and_convert_pir_layout, PirLayout};
use crate::http_transport::PirHttpFailure;
use crate::types::VotingError;

pub use crate::pir_snapshot::{
    classify_pir_snapshot_height, matching_pir_snapshot_endpoints, select_pir_snapshot_endpoint,
    PirSnapshotEndpointDiagnostic, PirSnapshotEndpointStatus, PirSnapshotResolution,
};

/// Candidate PIR endpoint URL.
pub type PirEndpoint = String;

/// Selects an exact-height PIR endpoint from already-probed diagnostics.
///
/// `match_index` lets callers inject deterministic or random selection without
/// making endpoint probing part of the core API.
pub fn select_pir_endpoint(
    diagnostics: &[PirSnapshotEndpointDiagnostic],
    snapshot_height: u64,
    match_index: u64,
) -> Result<PirSnapshotResolution, VotingError> {
    select_pir_snapshot_endpoint(diagnostics, snapshot_height, match_index)
}

pub use pir_client::{
    ImtProofData, PirClient, PirClientBlocking, Transport, TransportFuture, TransportResponse,
};
pub use pir_types::PirLayout as NegotiatedPirLayout;

/// Converts a wallet-config [`PirLayout`] into the PIR client's negotiated layout.
///
/// Rejects the legacy summary sentinel [`PirLayout::UNKNOWN`], inconsistent
/// geometry, and layouts below the YPIR row or item-size minima.
pub fn negotiated_pir_layout(layout: PirLayout) -> Result<NegotiatedPirLayout, VotingError> {
    if layout == PirLayout::UNKNOWN {
        return Err(VotingError::InvalidInput {
            message: "pir_layout is unknown; resolve a current dynamic voting config first"
                .to_string(),
        });
    }
    validate_and_convert_pir_layout(layout).map_err(|message| VotingError::InvalidInput {
        message: format!("invalid pir_layout: {message}"),
    })
}

/// Connects a blocking PIR client using an explicit layout and endpoint URL.
///
/// Does not check whether `endpoint_url` appears in a resolved config's
/// advertised endpoint list; callers pass a layout and URL they already chose
/// (for example after exact-height snapshot probing). Fails closed before any
/// private query when the layout is unknown or the config/server handshake
/// rejects the server. Layout mismatches (including `poly_len`) surface as
/// [`VotingError::InvalidInput`]; other connect failures surface as
/// [`VotingError::PirUnavailable`] with a typed retryability decision.
pub fn connect_pir_blocking(
    pir_layout: PirLayout,
    endpoint_url: &str,
    transport: Arc<dyn Transport>,
) -> Result<PirClientBlocking, VotingError> {
    let endpoint = normalize_endpoint_url(endpoint_url);
    if endpoint.is_empty() {
        return Err(VotingError::InvalidInput {
            message: "PIR endpoint URL must not be empty".to_string(),
        });
    }
    let expected_layout = negotiated_pir_layout(pir_layout)?;
    PirClientBlocking::with_transport(&endpoint, expected_layout, transport)
        .map_err(|err| map_pir_connect_error(&endpoint, err))
}

/// Connects an async PIR client using an explicit layout and endpoint URL.
///
/// See [`connect_pir_blocking`] for layout and URL rules.
pub async fn connect_pir(
    pir_layout: PirLayout,
    endpoint_url: &str,
    transport: Arc<dyn Transport>,
) -> Result<PirClient, VotingError> {
    let endpoint = normalize_endpoint_url(endpoint_url);
    if endpoint.is_empty() {
        return Err(VotingError::InvalidInput {
            message: "PIR endpoint URL must not be empty".to_string(),
        });
    }
    let expected_layout = negotiated_pir_layout(pir_layout)?;
    PirClient::with_transport(&endpoint, expected_layout, transport)
        .await
        .map_err(|err| map_pir_connect_error(&endpoint, err))
}

/// Canonical form of one endpoint URL, so equivalent spellings dedupe to one
/// fleet member: lowercase scheme and host, no default port, unreserved
/// percent escapes decoded, dot segments resolved, no trailing slashes. A string that does not parse as a URL keeps only the whitespace
/// and slash trimming; connecting to it reports the real problem.
fn normalize_endpoint_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let Ok(uri) = trimmed.parse::<http::Uri>() else {
        return trimmed.to_string();
    };
    let (Some(scheme), Some(host)) = (uri.scheme_str(), uri.host()) else {
        return trimmed.to_string();
    };
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    };
    let port = uri
        .port_u16()
        .filter(|port| Some(*port) != default_port)
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    let path = remove_dot_segments(&normalize_path_escapes(uri.path()));
    let path = path.trim_end_matches('/');
    let query = uri
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    format!(
        "{scheme}://{}{port}{path}{query}",
        host.to_ascii_lowercase()
    )
}

/// Decodes percent escapes of unreserved characters and uppercases the hex of
/// every other escape, so URI-equivalent paths such as `/%7Eoperator` and
/// `/~operator` compare equal. Malformed escapes are kept verbatim.
fn normalize_path_escapes(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = String::with_capacity(path.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() + 0 && index + 2 <= bytes.len() - 1 {
            let hex = &path[index + 1..index + 3];
            if let Ok(value) = u8::from_str_radix(hex, 16) {
                if value.is_ascii_alphanumeric() || b"-._~".contains(&value) {
                    out.push(value as char);
                } else {
                    out.push('%');
                    out.push_str(&hex.to_ascii_uppercase());
                }
                index += 3;
                continue;
            }
        }
        out.push(bytes[index] as char);
        index += 1;
    }
    out
}

/// Resolves `.` and `..` segments (RFC 3986 section 5.2.4), so `/a/../api`
/// and `/api` name one endpoint. A `..` at the root is dropped, as a server
/// resolving the path would drop it. Runs after escape normalization so an
/// escaped dot segment is resolved too.
fn remove_dot_segments(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "." => {}
            ".." => {
                // The leading empty segment is the root and is never popped.
                if segments.len() > 1 {
                    segments.pop();
                }
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

fn map_pir_connect_error(endpoint: &str, err: anyhow::Error) -> VotingError {
    // A typed transport failure is authoritative: its retryability decides
    // whether the fleet moves to the next endpoint. The response body it
    // quotes may echo any text, including a layout-mismatch message, so text
    // matching applies only to errors that carry no typed failure.
    if PirHttpFailure::from_error_chain(&err).is_none() {
        let detail = format!("{err:#}");
        // Config/server layout or poly_len disagreement is a caller/config
        // incompatibility, not an endpoint availability problem.
        if detail.contains("PIR layout mismatch") || detail.contains("PIR poly_len mismatch") {
            return VotingError::InvalidInput {
                message: format!("PIR connect failed: {detail}"),
            };
        }
    }
    map_pir_fetch_error(Some(endpoint), "PIR connect failed", err)
}

/// Converts a PIR client failure into [`VotingError::PirUnavailable`].
///
/// Retryability and the HTTP status come from the typed
/// [`PirHttpFailure`] the SDK transport attaches; a foreign transport that
/// reports only text yields a non-retryable error.
pub(crate) fn map_pir_fetch_error(
    endpoint: Option<&str>,
    context: &str,
    err: anyhow::Error,
) -> VotingError {
    let typed = PirHttpFailure::from_error_chain(&err);
    VotingError::PirUnavailable {
        endpoint: endpoint.map(str::to_string),
        http_status: typed.and_then(|failure| failure.http_status),
        retryable: typed.is_some_and(PirHttpFailure::retryable),
        message: format!("{context}: {err:#}"),
    }
}

/// Source of PIR non-membership proofs used by delegation and cache warm-up.
///
/// [`PirClientBlocking`] implements this directly. [`PirSession`] implements it
/// without owning a Tokio runtime on the caller's thread, so proving code can
/// run inside `spawn_blocking` workers or any other thread that must not build
/// or block on a nested runtime.
pub trait PirProofSource: Send + Sync {
    /// The circuit root (PIR root padded to circuit depth) the server serves.
    fn circuit_root(&self) -> Fp;
    /// Fetches proofs for `nullifiers`, all against the same served root.
    fn fetch_proofs(&self, nullifiers: &[Fp]) -> anyhow::Result<Vec<ImtProofData>>;
}

impl PirProofSource for PirClientBlocking {
    fn circuit_root(&self) -> Fp {
        PirClientBlocking::circuit_root(self)
    }

    fn fetch_proofs(&self, nullifiers: &[Fp]) -> anyhow::Result<Vec<ImtProofData>> {
        PirClientBlocking::fetch_proofs(self, nullifiers)
    }
}

impl<T: PirProofSource + ?Sized> PirProofSource for Arc<T> {
    fn circuit_root(&self) -> Fp {
        (**self).circuit_root()
    }

    fn fetch_proofs(&self, nullifiers: &[Fp]) -> anyhow::Result<Vec<ImtProofData>> {
        (**self).fetch_proofs(nullifiers)
    }
}

enum PirSessionRequest {
    Fetch {
        nullifiers: Vec<Fp>,
        reply: mpsc::Sender<anyhow::Result<Vec<ImtProofData>>>,
    },
}

/// A connected PIR client serviced by its own OS thread.
///
/// The thread owns a single-threaded Tokio runtime and the async
/// [`PirClient`]; callers block on a channel for each fetch. That keeps the
/// runtime off the caller's thread, so a session can be used from a
/// `spawn_blocking` worker of another runtime, where [`PirClientBlocking`]
/// would panic or deadlock. Dropping the session ends the thread once any
/// in-flight fetch completes.
pub struct PirSession {
    requests: mpsc::Sender<PirSessionRequest>,
    circuit_root: Fp,
    endpoint: String,
    worker: Option<thread::JoinHandle<()>>,
}

impl PirSession {
    /// Connects to `endpoint_url` and performs the layout handshake.
    ///
    /// # Errors
    ///
    /// Returns [`VotingError::InvalidInput`] for an empty URL or an unusable
    /// layout, and [`VotingError::PirUnavailable`] when the endpoint cannot be
    /// reached or fails the handshake.
    pub fn connect(
        endpoint_url: &str,
        pir_layout: PirLayout,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, VotingError> {
        let endpoint = normalize_endpoint_url(endpoint_url);
        if endpoint.is_empty() {
            return Err(VotingError::InvalidInput {
                message: "PIR endpoint URL must not be empty".to_string(),
            });
        }
        let expected_layout = negotiated_pir_layout(pir_layout)?;
        let (ready_tx, ready_rx) = mpsc::channel::<Result<Fp, VotingError>>();
        let (request_tx, request_rx) = mpsc::channel::<PirSessionRequest>();
        let thread_endpoint = endpoint.clone();
        let worker = thread::Builder::new()
            .name("voting-pir-session".to_string())
            .spawn(move || {
                // The blocking client builds its own runtime; on this plain
                // OS thread that is safe, which is the whole point of the
                // session: callers never host that runtime themselves.
                let client = match PirClientBlocking::with_transport(
                    &thread_endpoint,
                    expected_layout,
                    transport,
                ) {
                    Ok(client) => client,
                    Err(error) => {
                        let _ = ready_tx.send(Err(map_pir_connect_error(&thread_endpoint, error)));
                        return;
                    }
                };
                if ready_tx.send(Ok(client.circuit_root())).is_err() {
                    return;
                }
                while let Ok(PirSessionRequest::Fetch { nullifiers, reply }) = request_rx.recv() {
                    let _ = reply.send(client.fetch_proofs(&nullifiers));
                }
            })
            .map_err(|error| VotingError::Internal {
                message: format!("failed to spawn PIR session thread: {error}"),
            })?;
        let circuit_root = match ready_rx.recv() {
            Ok(ready) => ready?,
            Err(_) => {
                let _ = worker.join();
                return Err(VotingError::Internal {
                    message: "PIR session thread exited before connecting".to_string(),
                });
            }
        };
        Ok(Self {
            requests: request_tx,
            circuit_root,
            endpoint,
            worker: Some(worker),
        })
    }

    /// Endpoint this session is connected to.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl PirProofSource for PirSession {
    fn circuit_root(&self) -> Fp {
        self.circuit_root
    }

    fn fetch_proofs(&self, nullifiers: &[Fp]) -> anyhow::Result<Vec<ImtProofData>> {
        let (reply_tx, reply_rx) = mpsc::channel();
        let ended = || PirHttpFailure {
            phase: crate::http_transport::PirHttpFailurePhase::Send,
            http_status: None,
        };
        self.requests
            .send(PirSessionRequest::Fetch {
                nullifiers: nullifiers.to_vec(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow::Error::new(ended()).context("PIR session ended"))?;
        reply_rx
            .recv()
            .map_err(|_| anyhow::Error::new(ended()).context("PIR session ended mid-fetch"))?
    }
}

impl Drop for PirSession {
    fn drop(&mut self) {
        let (detached, _) = mpsc::channel();
        drop(std::mem::replace(&mut self.requests, detached));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Ordered PIR endpoints for one snapshot, with failover on retryable failures.
///
/// Endpoints are tried in order. A connect or fetch failure that is
/// [`VotingError::retryable`] moves to the next endpoint; any other failure,
/// or exhausting the list, is returned to the caller.
#[derive(Clone)]
pub struct PirFleet {
    endpoints: Vec<String>,
    layout: PirLayout,
    transport: Arc<dyn Transport>,
}

impl PirFleet {
    /// Builds a fleet from ordered endpoint URLs, dropping duplicates.
    ///
    /// # Errors
    ///
    /// Returns [`VotingError::InvalidInput`] for an empty list, an empty URL,
    /// or an unusable layout.
    pub fn new(
        endpoints: &[String],
        layout: PirLayout,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, VotingError> {
        let mut normalized: Vec<String> = Vec::with_capacity(endpoints.len());
        for raw in endpoints {
            let url = normalize_endpoint_url(raw);
            if url.is_empty() {
                return Err(VotingError::InvalidInput {
                    message: "PIR server URLs must not contain an empty URL".to_string(),
                });
            }
            if !normalized.iter().any(|existing| *existing == url) {
                normalized.push(url);
            }
        }
        if normalized.is_empty() {
            return Err(VotingError::InvalidInput {
                message: "at least one PIR server URL is required".to_string(),
            });
        }
        negotiated_pir_layout(layout)?;
        Ok(Self {
            endpoints: normalized,
            layout,
            transport,
        })
    }

    /// Ordered, deduplicated endpoint URLs.
    pub fn endpoints(&self) -> &[String] {
        &self.endpoints
    }

    /// Layout every endpoint is expected to serve.
    pub fn layout(&self) -> PirLayout {
        self.layout
    }

    /// Connects to the first endpoint that completes the handshake.
    pub fn connect(&self) -> Result<PirSession, VotingError> {
        failover_over(
            &self.endpoints,
            |endpoint| PirSession::connect(endpoint, self.layout, Arc::clone(&self.transport)),
            |_| Ok(()),
        )
        .map(|(session, ())| session)
    }

    /// Runs `operation` against connected endpoints in order until it
    /// succeeds or fails with anything other than a retryable PIR failure.
    /// Local contention (`Busy`, `DbBusy`) is returned to the caller for an
    /// operation-level retry rather than repeated against other servers.
    ///
    /// The operation must be safe to repeat against another endpoint; proof
    /// fetches and cache warm-ups are, because every persisted proof is
    /// validated against the served root before it is stored.
    pub fn with_failover<T>(
        &self,
        operation: impl FnMut(&PirSession) -> Result<T, VotingError>,
    ) -> Result<T, VotingError> {
        failover_over(
            &self.endpoints,
            |endpoint| PirSession::connect(endpoint, self.layout, Arc::clone(&self.transport)),
            operation,
        )
        .map(|(_, value)| value)
    }
}

/// Whether a failure justifies trying the next PIR endpoint.
///
/// Only a retryable PIR transport failure does. Local contention such as
/// `Busy` or `DbBusy` is also retryable, but changing servers cannot fix it
/// and would repeat private PIR requests and database waits; it is returned
/// to the host for an operation-level retry instead.
fn moves_to_next_endpoint(error: &VotingError) -> bool {
    matches!(
        error,
        VotingError::PirUnavailable {
            retryable: true,
            ..
        }
    )
}

/// Shared failover policy over an ordered endpoint list; see
/// [`moves_to_next_endpoint`] for which failures advance.
fn failover_over<S, T>(
    endpoints: &[String],
    mut connect: impl FnMut(&str) -> Result<S, VotingError>,
    mut operation: impl FnMut(&S) -> Result<T, VotingError>,
) -> Result<(S, T), VotingError> {
    let last = endpoints.len().saturating_sub(1);
    for (index, endpoint) in endpoints.iter().enumerate() {
        let has_next = index < last;
        let session = match connect(endpoint) {
            Ok(session) => session,
            Err(error) if moves_to_next_endpoint(&error) && has_next => continue,
            Err(error) => return Err(error),
        };
        match operation(&session) {
            Ok(value) => return Ok((session, value)),
            Err(error) if moves_to_next_endpoint(&error) && has_next => continue,
            Err(error) => return Err(error),
        }
    }
    Err(VotingError::InvalidInput {
        message: "at least one PIR server URL is required".to_string(),
    })
}

#[cfg(test)]
mod tests {
    mod classification;
    mod fleet;

    use super::*;
    use crate::config::{
        resolve_dynamic_voting_config, resolve_static_voting_config, ResolveVotingConfigOptions,
    };
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
    use ed25519_dalek::{Signer, SigningKey};
    use pir_types::{
        RootInfo, YpirScenario, COMPILED_PIR_LAYOUT, DATASET_VERSION, DEFAULT_YPIR_POLY_LEN,
        NULLIFIER_POOL, PIR_DEPTH, TIER0_LAYERS,
    };
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Mutex;

    const ROUND_ID: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    const TEST_POLY_LEN: u32 = DEFAULT_YPIR_POLY_LEN as u32;

    #[derive(Default)]
    struct RecordingTransport {
        gets: Mutex<HashMap<String, TransportResponse>>,
        hits: Mutex<Vec<String>>,
        posts: Mutex<usize>,
    }

    impl RecordingTransport {
        fn matching_root() -> Self {
            let transport = Self::default();
            transport.insert_matching_assets(COMPILED_PIR_LAYOUT, DEFAULT_YPIR_POLY_LEN);
            transport
        }

        fn with_root_layout(layout: NegotiatedPirLayout) -> Self {
            let transport = Self::default();
            transport.insert_matching_assets(layout, layout.poly_len);
            transport
        }

        fn insert_matching_assets(&self, layout: NegotiatedPirLayout, poly_len: usize) {
            let rows = layout.tier1_rows().unwrap();
            let row_bytes = layout.tier1_row_bytes().unwrap();
            let item_bits = layout.tier1_item_bits().unwrap();
            let tier0_bytes = layout.tier0_bytes().unwrap();
            let root = RootInfo {
                zcash_network: pir_types::ZcashNetwork::Test,
                nullifier_pool: NULLIFIER_POOL.to_owned(),
                dataset_version: DATASET_VERSION,
                circuit_root: hex::encode([0u8; 32]),
                pir_root: hex::encode([0u8; 32]),
                num_ranges: 1,
                pir_layout: layout,
                pir_depth: layout.pir_depth,
                tier1_rows: rows,
                tier1_row_bytes: row_bytes,
                height: Some(100),
            };
            let tier1 = YpirScenario {
                num_items: rows,
                item_size_bits: item_bits,
                poly_len,
            };
            let mut gets = self.gets.lock().unwrap();
            gets.insert("/tier0".to_string(), response(vec![0; tier0_bytes]));
            gets.insert(
                "/params/tier1".to_string(),
                response(serde_json::to_vec(&tier1).unwrap()),
            );
            gets.insert(
                "/root".to_string(),
                response(serde_json::to_vec(&root).unwrap()),
            );
        }

        fn count_hits(&self, path: &str) -> usize {
            self.hits
                .lock()
                .unwrap()
                .iter()
                .filter(|hit| hit.as_str() == path)
                .count()
        }

        fn post_count(&self) -> usize {
            *self.posts.lock().unwrap()
        }
    }

    impl Transport for RecordingTransport {
        fn get<'a>(&'a self, url: &'a str) -> TransportFuture<'a> {
            Box::pin(async move {
                let path = request_path(url);
                self.hits.lock().unwrap().push(path.to_owned());
                self.gets
                    .lock()
                    .unwrap()
                    .get(path)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("unexpected GET {path}"))
            })
        }

        fn post<'a>(&'a self, url: &'a str, _body: Vec<u8>) -> TransportFuture<'a> {
            Box::pin(async move {
                let path = request_path(url);
                self.hits.lock().unwrap().push(path.to_owned());
                *self.posts.lock().unwrap() += 1;
                Err(anyhow::anyhow!(
                    "unexpected POST {path}; layout handshake must fail before queries"
                ))
            })
        }
    }

    fn response(body: Vec<u8>) -> TransportResponse {
        TransportResponse {
            status: 200,
            headers: Vec::new(),
            body,
        }
    }

    fn request_path(url: &str) -> &str {
        let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
        without_scheme
            .find('/')
            .map(|idx| &without_scheme[idx..])
            .unwrap_or("/")
    }

    fn expect_connect_err(result: Result<PirClientBlocking, VotingError>) -> VotingError {
        match result {
            Ok(_) => panic!("expected PIR connect failure"),
            Err(err) => err,
        }
    }

    fn compiled_wallet_layout() -> PirLayout {
        PirLayout {
            pir_depth: u32::try_from(COMPILED_PIR_LAYOUT.pir_depth).unwrap(),
            tier0_layers: u32::try_from(COMPILED_PIR_LAYOUT.tier0_layers).unwrap(),
            tier1_layers: u32::try_from(COMPILED_PIR_LAYOUT.tier1_layers).unwrap(),
            poly_len: TEST_POLY_LEN,
        }
    }

    #[test]
    fn connect_rejects_unknown_layout_before_transport() {
        let transport = Arc::new(RecordingTransport::matching_root());

        let err = expect_connect_err(connect_pir_blocking(
            PirLayout::UNKNOWN,
            "https://pir.example.com",
            transport.clone(),
        ));

        assert!(matches!(err, VotingError::InvalidInput { .. }), "{err}");
        assert!(err.to_string().contains("pir_layout is unknown"), "{err}");
        assert_eq!(transport.count_hits("/root"), 0);
        assert_eq!(transport.post_count(), 0);
    }

    #[test]
    fn connect_rejects_unusable_ypir_layouts_before_transport() {
        for (layout, expected) in [
            (
                PirLayout {
                    pir_depth: 19,
                    tier0_layers: 10,
                    tier1_layers: 9,
                    poly_len: 4096,
                },
                "Tier 1 rows 1024 below YPIR minimum 2048",
            ),
            (
                PirLayout {
                    pir_depth: 19,
                    tier0_layers: 14,
                    tier1_layers: 5,
                    poly_len: 4096,
                },
                "Tier 1 item bits 24576 below YPIR minimum 28672",
            ),
            (
                PirLayout {
                    pir_depth: 19,
                    tier0_layers: 0,
                    tier1_layers: 19,
                    poly_len: 4096,
                },
                "PIR layout tiers must be non-zero",
            ),
        ] {
            let transport = Arc::new(RecordingTransport::matching_root());
            let err = expect_connect_err(connect_pir_blocking(
                layout,
                "https://pir.example.com",
                transport.clone(),
            ));

            assert!(matches!(err, VotingError::InvalidInput { .. }), "{err}");
            assert!(err.to_string().contains(expected), "{err}");
            assert_eq!(transport.count_hits("/tier0"), 0);
            assert_eq!(transport.count_hits("/params/tier1"), 0);
            assert_eq!(transport.count_hits("/root"), 0);
            assert_eq!(transport.post_count(), 0);
        }
    }

    #[test]
    fn connect_rejects_server_layout_mismatch_before_query() {
        let mismatched = NegotiatedPirLayout {
            pir_depth: PIR_DEPTH,
            tier0_layers: TIER0_LAYERS - 1,
            tier1_layers: COMPILED_PIR_LAYOUT.tier1_layers + 1,
            poly_len: DEFAULT_YPIR_POLY_LEN,
        };
        let transport = Arc::new(RecordingTransport::with_root_layout(mismatched));

        let err = expect_connect_err(connect_pir_blocking(
            compiled_wallet_layout(),
            "https://pir.example.com/",
            transport.clone(),
        ));

        assert!(matches!(err, VotingError::InvalidInput { .. }), "{err}");
        assert!(err.to_string().contains("PIR layout mismatch"), "{err}");
        assert_eq!(transport.post_count(), 0);
    }

    #[test]
    fn connect_rejects_config_layout_mismatch_before_query() {
        let layout = PirLayout {
            pir_depth: 18,
            tier0_layers: 11,
            tier1_layers: 7,
            poly_len: 4096,
        };
        let transport = Arc::new(RecordingTransport::matching_root());

        let err = expect_connect_err(connect_pir_blocking(
            layout,
            "https://pir.example.com",
            transport.clone(),
        ));

        assert!(matches!(err, VotingError::InvalidInput { .. }), "{err}");
        assert!(err.to_string().contains("PIR layout mismatch"), "{err}");
        assert_eq!(transport.count_hits("/root"), 1);
        assert_eq!(transport.count_hits("/tier0"), 0);
        assert_eq!(transport.count_hits("/params/tier1"), 0);
        assert_eq!(transport.post_count(), 0);
    }

    #[test]
    fn connect_succeeds_for_matching_alternate_layouts() {
        for (pir_depth, tier0_layers, tier1_layers, poly_len) in
            [(19, 11, 8, 2048), (19, 13, 6, 4096), (20, 12, 8, 4096)]
        {
            let negotiated = NegotiatedPirLayout {
                pir_depth,
                tier0_layers,
                tier1_layers,
                poly_len,
            };
            let wallet_layout = PirLayout {
                pir_depth: u32::try_from(negotiated.pir_depth).unwrap(),
                tier0_layers: u32::try_from(negotiated.tier0_layers).unwrap(),
                tier1_layers: u32::try_from(negotiated.tier1_layers).unwrap(),
                poly_len: u32::try_from(negotiated.poly_len).unwrap(),
            };
            let transport = Arc::new(RecordingTransport::with_root_layout(negotiated));

            let _client = connect_pir_blocking(
                wallet_layout,
                "https://pir.example.com/",
                transport.clone(),
            )
            .unwrap_or_else(|err| {
                panic!(
                    "matching layout {pir_depth}/{tier0_layers}/{tier1_layers}/{poly_len} failed: {err}"
                )
            });

            assert_eq!(transport.count_hits("/tier0"), 1);
            assert_eq!(transport.count_hits("/params/tier1"), 1);
            assert_eq!(transport.count_hits("/root"), 1);
            assert_eq!(transport.post_count(), 0);
        }
    }

    #[test]
    fn connect_rejects_poly_len_mismatch_before_query() {
        // poly_len is part of PirLayout, so /root rejects before /params/tier1.
        let transport = Arc::new(RecordingTransport::matching_root());

        let err = expect_connect_err(connect_pir_blocking(
            PirLayout {
                poly_len: 2048,
                ..compiled_wallet_layout()
            },
            "https://pir.example.com",
            transport.clone(),
        ));

        assert!(matches!(err, VotingError::InvalidInput { .. }), "{err}");
        assert!(err.to_string().contains("PIR layout mismatch"), "{err}");
        assert_eq!(transport.count_hits("/root"), 1);
        assert_eq!(transport.count_hits("/params/tier1"), 0);
        assert_eq!(transport.post_count(), 0);
    }

    #[test]
    fn connect_succeeds_when_config_and_server_layouts_match() {
        let transport = Arc::new(RecordingTransport::matching_root());

        let _client = connect_pir_blocking(
            compiled_wallet_layout(),
            "https://pir.example.com/",
            transport.clone(),
        )
        .unwrap();

        assert_eq!(transport.count_hits("/tier0"), 1);
        assert_eq!(transport.count_hits("/params/tier1"), 1);
        assert_eq!(transport.count_hits("/root"), 1);
        assert_eq!(transport.post_count(), 0);
    }

    #[test]
    fn connect_accepts_caller_chosen_endpoint_url() {
        let transport = Arc::new(RecordingTransport::matching_root());

        let _client = connect_pir_blocking(
            compiled_wallet_layout(),
            "https://unlisted-pir.example.com/",
            transport.clone(),
        )
        .unwrap();

        assert_eq!(transport.count_hits("/tier0"), 1);
        assert_eq!(transport.count_hits("/params/tier1"), 1);
        assert_eq!(transport.count_hits("/root"), 1);
        assert_eq!(transport.post_count(), 0);
    }

    #[test]
    fn connect_rejects_empty_url() {
        let transport = Arc::new(RecordingTransport::matching_root());
        let err = expect_connect_err(connect_pir_blocking(
            compiled_wallet_layout(),
            "   ",
            transport.clone(),
        ));
        assert!(err.to_string().contains("must not be empty"), "{err}");
        assert_eq!(transport.count_hits("/root"), 0);
    }

    #[test]
    fn signed_static_to_dynamic_to_pir_connect_handshake() {
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let pubkey = signing_key.verifying_key().to_bytes();
        let static_bytes = json!({
            "static_config_version": 1,
            "dynamic_config_url": "https://example.com/dynamic.json",
            "trusted_keys": [{
                "key_id": "k1",
                "alg": "ed25519",
                "pubkey": BASE64.encode(pubkey),
                "notes": null
            }]
        })
        .to_string()
        .into_bytes();
        let source = "https://example.com/static.json";
        let resolved_static = resolve_static_voting_config(source, &static_bytes).unwrap();

        let ea_pk = [7u8; 32];
        let wallet_layout = compiled_wallet_layout();
        let mut preimage = b"zcash-shielded-vote:round-auth:v2".to_vec();
        preimage.extend_from_slice(&hex::decode(ROUND_ID).unwrap());
        preimage.extend_from_slice(&ea_pk);
        preimage.extend_from_slice(&wallet_layout.pir_depth.to_le_bytes());
        preimage.extend_from_slice(&wallet_layout.tier0_layers.to_le_bytes());
        preimage.extend_from_slice(&wallet_layout.tier1_layers.to_le_bytes());
        preimage.extend_from_slice(&TEST_POLY_LEN.to_le_bytes());
        let sig = signing_key.sign(&preimage).to_bytes();
        let dynamic_bytes = json!({
            "config_version": 1,
            "vote_servers": [{"url": "https://vote.example.com", "label": "vote"}],
            "pir_endpoints": [{"url": "https://pir.example.com", "label": "pir"}],
            "pir_layout": {
                "pir_depth": COMPILED_PIR_LAYOUT.pir_depth,
                "tier0_layers": COMPILED_PIR_LAYOUT.tier0_layers,
                "tier1_layers": COMPILED_PIR_LAYOUT.tier1_layers,
                "poly_len": TEST_POLY_LEN
            },
            "supported_versions": {
                "pir": ["v0"],
                "vote_protocol": "v1",
                "tally": "v0",
                "vote_server": "v1"
            },
            "rounds": {
                ROUND_ID: {
                    "auth_version": 2,
                    "ea_pk": BASE64.encode(ea_pk),
                    "signatures": [{
                        "key_id": "k1",
                        "alg": "ed25519",
                        "sig": BASE64.encode(sig)
                    }]
                }
            }
        })
        .to_string()
        .into_bytes();
        let resolved = resolve_dynamic_voting_config(
            resolved_static,
            &dynamic_bytes,
            ResolveVotingConfigOptions::default(),
        )
        .unwrap();

        assert_eq!(resolved.pir_layout, compiled_wallet_layout());
        assert_eq!(resolved.pir_endpoints[0].url, "https://pir.example.com");

        let matching = Arc::new(RecordingTransport::matching_root());
        let _client = connect_pir_blocking(
            resolved.pir_layout,
            "https://pir.example.com",
            matching.clone(),
        )
        .unwrap();
        assert_eq!(matching.post_count(), 0);

        let mismatched = Arc::new(RecordingTransport::with_root_layout(NegotiatedPirLayout {
            pir_depth: 18,
            tier0_layers: 11,
            tier1_layers: 7,
            poly_len: DEFAULT_YPIR_POLY_LEN,
        }));
        let err = expect_connect_err(connect_pir_blocking(
            resolved.pir_layout,
            "https://pir.example.com",
            mismatched.clone(),
        ));
        assert!(matches!(err, VotingError::InvalidInput { .. }), "{err}");
        assert!(err.to_string().contains("PIR layout mismatch"), "{err}");
        assert_eq!(mismatched.post_count(), 0);
    }
}
