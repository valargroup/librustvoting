use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
    time::Duration,
};

use anyhow::Result;
use bytes::Bytes;
use http::{Method, Request};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{body::Incoming, Response};
use hyper_util::{
    client::legacy::{
        connect::{Connect, HttpConnector},
        Client, Error as HyperClientError,
    },
    rt::TokioExecutor,
};

use crate::chain_submission::{
    ChainHttpRequest, ChainHttpResponse, ChainPostDispatch, ChainTransport, ChainTransportError,
    ChainTransportFuture,
};
use crate::helper::transport::{
    HelperFuture, HelperResponse, HelperTransport, HelperTransportError, MAX_HELPER_RESPONSE_BYTES,
};

type RequestBody = Full<Bytes>;
type HyperRequestFuture<'a> = Pin<
    Box<dyn Future<Output = std::result::Result<Response<Incoming>, HyperClientError>> + Send + 'a>,
>;

/// Type-erased request boundary that keeps connector types out of the public
/// `HyperTransport` type while retaining Hyper's pooled client.
trait HyperRequestClient: Send + Sync {
    fn request<'a>(&'a self, request: Request<RequestBody>) -> HyperRequestFuture<'a>;
}

impl<C> HyperRequestClient for Client<C, RequestBody>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    fn request<'a>(&'a self, request: Request<RequestBody>) -> HyperRequestFuture<'a> {
        Box::pin(Client::request(self, request))
    }
}

// PIR responses are normally below 1 MiB after layout validation. Keep a
// generous fixed ceiling in the built-in transport so a server cannot force an
// unbounded allocation before the client validates the negotiated geometry,
// and bound the complete request so a slow or endless body cannot stall setup.
const MAX_PIR_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const PIR_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
// Tree pages are JSON encoded and can be larger than the compact state
// responses. Bound every tree response before buffering or parsing it, and
// cover connection setup plus the complete body read with one deadline.
const MAX_TREE_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const TREE_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
// The helper body ceiling is a protocol property, so it comes from the
// transport contract rather than being chosen here. The deadline stays
// caller-supplied, because helper retry cadence is a policy decision.

/// Where a PIR HTTP request failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PirHttpFailurePhase {
    /// The request could not be constructed.
    Build,
    /// No connection could be established.
    Connect,
    /// The request was sent but no response arrived.
    Send,
    /// The response body could not be read within the size limit.
    Body,
    /// The whole request exceeded the transport deadline.
    Timeout,
    /// The server answered with a non-success status.
    Status,
}

/// Typed failure the SDK transport attaches to PIR request errors.
///
/// PIR client errors are `anyhow` chains; this value sits inside that chain so
/// callers can classify retryability with
/// [`PirHttpFailure::from_error_chain`] instead of parsing text. A custom PIR
/// transport should attach the same value to its failures to get typed
/// retry decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("PIR HTTP {phase:?} failure{}", pir_status_suffix(.http_status))]
pub struct PirHttpFailure {
    pub phase: PirHttpFailurePhase,
    pub http_status: Option<u16>,
}

fn pir_status_suffix(status: &Option<u16>) -> String {
    status
        .map(|status| format!(" (status {status})"))
        .unwrap_or_default()
}

impl PirHttpFailure {
    /// Whether another endpoint or a later attempt may succeed.
    pub fn retryable(&self) -> bool {
        match self.phase {
            PirHttpFailurePhase::Connect
            | PirHttpFailurePhase::Send
            | PirHttpFailurePhase::Body
            | PirHttpFailurePhase::Timeout => true,
            PirHttpFailurePhase::Status => {
                matches!(self.http_status, Some(408 | 429) | Some(500..=599))
            }
            PirHttpFailurePhase::Build => false,
        }
    }

    /// Finds the typed failure anywhere in an `anyhow` error chain.
    pub fn from_error_chain(error: &anyhow::Error) -> Option<&Self> {
        error.chain().find_map(|cause| cause.downcast_ref::<Self>())
    }

    fn wrap(self, message: String) -> anyhow::Error {
        anyhow::Error::new(self).context(message)
    }
}

/// One HTTP request handed to a [`RouteHttp`] executor.
pub struct RouteRequest<'a> {
    pub method: Method,
    pub url: &'a str,
    /// Protocol headers the SDK requires for this request.
    pub headers: &'a [(String, String)],
    pub body: Vec<u8>,
    /// Deadline for the complete request: connection setup, dispatch, and
    /// body read. The SDK enforces the same deadline as a backstop.
    pub timeout: Duration,
    /// Response body ceiling. Executors stop reading at this size and fail
    /// with [`RoutePhase::ResponseRead`].
    pub max_response_bytes: usize,
}

/// Response produced by a [`RouteHttp`] executor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RouteResponse {
    fn content_type(&self) -> Option<String> {
        self.headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.clone())
    }
}

/// Where a [`RouteHttp`] request failed relative to dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoutePhase {
    /// No request byte reached a network stack that could deliver it.
    BeforeDispatch,
    /// The request may have been delivered; no response headers arrived.
    AfterDispatch,
    /// Response headers arrived, but the body could not be read completely.
    ResponseRead,
}

/// Failure reported by a [`RouteHttp`] executor.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct RouteError {
    pub phase: RoutePhase,
    pub message: String,
}

impl RouteError {
    pub fn before_dispatch(message: impl Into<String>) -> Self {
        Self {
            phase: RoutePhase::BeforeDispatch,
            message: message.into(),
        }
    }

    pub fn after_dispatch(message: impl Into<String>) -> Self {
        Self {
            phase: RoutePhase::AfterDispatch,
            message: message.into(),
        }
    }

    pub fn response_read(message: impl Into<String>) -> Self {
        Self {
            phase: RoutePhase::ResponseRead,
            message: message.into(),
        }
    }
}

/// Boxed future returned by [`RouteHttp::execute`].
pub type RouteFuture<'a> =
    Pin<Box<dyn Future<Output = std::result::Result<RouteResponse, RouteError>> + Send + 'a>>;

/// Host-owned request executor behind [`HyperTransport`].
///
/// The SDK owns protocol headers, deadlines, response limits, and the
/// definite-versus-ambiguous classification every transport trait needs. An
/// executor owns only how one request reaches the network, so a wallet that
/// routes voting traffic through Tor or a proxy implements this trait once and
/// gets PIR, tree-sync, helper, and vote-chain transports from it.
///
/// Contract:
///
/// - Call `on_dispatch` immediately before the first request byte can reach
///   a network stack able to deliver it. Every failure after that call is
///   classified as possibly delivered, so calling it earlier than necessary
///   only makes classification more conservative; never calling it before
///   dispatch would misreport an ambiguous POST as safe to retry.
/// - Fail closed. When the configured route is unavailable, return
///   [`RoutePhase::BeforeDispatch`]; never fall back to a direct connection.
/// - Honor `max_response_bytes`, and `timeout` where the executor can bound
///   work the SDK cannot cancel. The SDK enforces `timeout` around the whole
///   call and classifies its own deadline by whether the hook was called.
/// - Report `phase` truthfully. It is consulted for failures the dispatch hook
///   cannot classify, such as a body-read failure after headers arrived.
/// - Never follow redirects. Return a 3xx response as received. The SDK
///   records helper acceptance against the configured URL and rejects
///   vote-chain redirects; a client that followed a 307 or 308 would deliver
///   a share to an unconfigured endpoint and report it as accepted by the
///   configured one. [`DirectRoute`] does not follow redirects.
pub trait RouteHttp: Send + Sync + 'static {
    fn execute<'a>(
        &'a self,
        request: RouteRequest<'a>,
        on_dispatch: &'a (dyn Fn() + Send + Sync),
    ) -> RouteFuture<'a>;
}

tokio::task_local! {
    /// Absolute deadline of the request in flight, read by
    /// [`ConnectDeadlineConnector`] so connection setup (TCP and TLS) times
    /// out as a connect error, which Hyper reports distinctly and the route
    /// classifies as pre-dispatch.
    static DIRECT_CONNECT_DEADLINE: Option<tokio::time::Instant>;
}

/// Upper bound on how far ahead of the SDK backstop the direct route abandons
/// connection setup, so a stalled connect is classified before the backstop
/// can. See [`direct_connect_deadline`].
const DIRECT_CONNECT_DEADLINE_LEAD: Duration = Duration::from_millis(25);

/// The instant at which the direct route abandons connection setup for a
/// request whose backstop fires at `backstop` after `timeout`.
///
/// The lead is a quarter of the timeout, capped at
/// [`DIRECT_CONNECT_DEADLINE_LEAD`], so short test timeouts keep most of
/// their budget for the connection while production timeouts of seconds get
/// the full 25 ms.
fn direct_connect_deadline(
    backstop: tokio::time::Instant,
    timeout: Duration,
) -> tokio::time::Instant {
    let lead = DIRECT_CONNECT_DEADLINE_LEAD.min(timeout / 4);
    backstop.checked_sub(lead).unwrap_or(backstop)
}

/// Applies the in-flight request deadline to connection setup.
///
/// Wrapping the complete TCP+TLS connector matters: a stalled TLS handshake
/// has still not dispatched an HTTP request, so it must surface as a connect
/// failure rather than race the whole-request deadline into an ambiguous
/// outcome.
#[derive(Clone)]
struct ConnectDeadlineConnector<C> {
    inner: C,
}

impl<C, T, E> tower_service::Service<http::Uri> for ConnectDeadlineConnector<C>
where
    C: tower_service::Service<http::Uri, Response = T, Error = E> + Send,
    C::Future: Send + 'static,
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Response = T;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future = Pin<Box<dyn Future<Output = std::result::Result<T, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        self.inner
            .poll_ready(cx)
            .map(|result| result.map_err(Into::into))
    }

    fn call(&mut self, uri: http::Uri) -> Self::Future {
        let future = self.inner.call(uri);
        let deadline = DIRECT_CONNECT_DEADLINE
            .try_with(|deadline| *deadline)
            .ok()
            .flatten();
        Box::pin(async move {
            match deadline {
                Some(deadline) => tokio::time::timeout_at(deadline, future)
                    .await
                    .map_err(|_| {
                        Box::new(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "connection setup timed out before request dispatch",
                        )) as Self::Error
                    })?
                    .map_err(Into::into),
                None => future.await.map_err(Into::into),
            }
        })
    }
}

/// SDK-owned direct HTTP/HTTPS executor with a pooled Hyper client.
///
/// Connection setup runs under the request deadline, so a TCP or TLS stall
/// is reported as a connect failure and classified as pre-dispatch.
pub struct DirectRoute {
    client: Box<dyn HyperRequestClient>,
}

impl DirectRoute {
    /// Creates the default direct HTTP/HTTPS executor.
    pub fn new() -> Self {
        let mut connector = HttpConnector::new();
        connector.enforce_http(false);
        Self::with_http_connector(connector)
    }

    /// Applies the SDK's standard Rustls configuration to a caller-supplied
    /// raw HTTP connector.
    ///
    /// This preserves WebPKI roots, HTTP/1 and HTTP/2 support, and cleartext
    /// HTTP compatibility while letting the host control how sockets are
    /// opened. Connection setup, TLS included, runs under the request
    /// deadline so a stall there is a connect failure. Hyper pools
    /// connections: a host whose route can change must close or invalidate
    /// already-open I/O when the old route is no longer permitted, or route
    /// every request through its own [`RouteHttp`].
    pub fn with_http_connector<C>(connector: C) -> Self
    where
        C: tower_service::Service<http::Uri> + Clone + Send + Sync + 'static,
        C::Response: hyper_util::client::legacy::connect::Connection
            + hyper::rt::Read
            + hyper::rt::Write
            + Unpin
            + Send
            + 'static,
        C::Future: Send + 'static,
        C::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        ensure_rustls_provider();
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(connector);
        Self::with_connector(ConnectDeadlineConnector { inner: https })
    }

    /// Uses a fully configured Hyper connector without adding TLS.
    ///
    /// The connector is used as given; only [`Self::with_http_connector`]
    /// and [`Self::new`] bound connection setup by the request deadline.
    pub fn with_connector<C>(connector: C) -> Self
    where
        C: Connect + Clone + Send + Sync + 'static,
    {
        let client = Client::builder(TokioExecutor::new()).build(connector);
        Self {
            client: Box::new(client),
        }
    }
}

impl Default for DirectRoute {
    fn default() -> Self {
        Self::new()
    }
}

impl RouteHttp for DirectRoute {
    fn execute<'a>(
        &'a self,
        request: RouteRequest<'a>,
        on_dispatch: &'a (dyn Fn() + Send + Sync),
    ) -> RouteFuture<'a> {
        Box::pin(async move {
            let mut builder = Request::builder().method(request.method).uri(request.url);
            for (name, value) in request.headers {
                builder = builder.header(name, value);
            }
            let hyper_request =
                builder
                    .body(Full::new(Bytes::from(request.body)))
                    .map_err(|error| {
                        RouteError::before_dispatch(format!("build HTTP request: {error}"))
                    })?;
            let max_response_bytes = request.max_response_bytes;
            // The SDK transport sets the connection-setup deadline before it
            // polls this future, so that connection setup fails as a connect
            // error before the SDK backstop fires. A caller driving the route
            // directly gets the same lead derived from `request.timeout`.
            let deadline = DIRECT_CONNECT_DEADLINE
                .try_with(|deadline| *deadline)
                .ok()
                .flatten()
                .or_else(|| {
                    tokio::time::Instant::now()
                        .checked_add(request.timeout)
                        .map(|backstop| direct_connect_deadline(backstop, request.timeout))
                });
            DIRECT_CONNECT_DEADLINE
                .scope(deadline, async move {
                    // Hyper offers no hook between connection setup and the first
                    // request byte, so dispatch is marked before the request is
                    // handed over. A connect failure is reported distinctly and
                    // reclassified as pre-dispatch below.
                    on_dispatch();
                    let response = self.client.request(hyper_request).await.map_err(|error| {
                        let message = format!("send HTTP request: {error}");
                        if error.is_connect() {
                            RouteError::before_dispatch(message)
                        } else {
                            RouteError::after_dispatch(message)
                        }
                    })?;
                    let status = response.status().as_u16();
                    let headers = response
                        .headers()
                        .iter()
                        .filter_map(|(name, value)| {
                            value
                                .to_str()
                                .ok()
                                .map(|value| (name.as_str().to_string(), value.to_string()))
                        })
                        .collect();
                    let body = Limited::new(response.into_body(), max_response_bytes)
                        .collect()
                        .await
                        .map_err(|error| {
                            RouteError::response_read(format!(
                            "read HTTP response body (limit {max_response_bytes} bytes): {error}"
                        ))
                        })?
                        .to_bytes()
                        .to_vec();
                    Ok(RouteResponse {
                        status,
                        headers,
                        body,
                    })
                })
                .await
        })
    }
}

/// Failure of one routed request with the SDK's own dispatch observation.
struct RoutedFailure {
    /// Whether the executor called the dispatch hook before failing.
    dispatched: bool,
    /// Whether the SDK backstop deadline fired.
    timed_out: bool,
    error: RouteError,
}

/// HTTP transport for client-side network requests.
///
/// `zcash_voting` keeps PIR, tree-sync, helper, and vote-chain traffic behind
/// small transport traits. This adapter implements all of them over one
/// [`RouteHttp`] executor: [`Self::new`] uses the SDK's direct HTTP/HTTPS
/// executor, and [`Self::with_route`] lets a host supply its own for Tor,
/// proxies, or route-lifecycle enforcement. Deadlines, response limits,
/// response metadata, and ambiguous-outcome classification are applied here,
/// once, regardless of the executor.
pub struct HyperTransport<R: RouteHttp = DirectRoute> {
    route: Arc<R>,
    runtime: BlockingRuntime,
}

impl HyperTransport<DirectRoute> {
    /// Creates the default direct HTTP/HTTPS transport.
    pub fn new() -> Self {
        Self::with_route(DirectRoute::new())
    }

    /// Creates a direct transport over a caller-supplied raw HTTP connector.
    ///
    /// See [`DirectRoute::with_http_connector`].
    pub fn with_http_connector<C>(connector: C) -> Self
    where
        C: tower_service::Service<http::Uri> + Clone + Send + Sync + 'static,
        C::Response: hyper_util::client::legacy::connect::Connection
            + hyper::rt::Read
            + hyper::rt::Write
            + Unpin
            + Send
            + 'static,
        C::Future: Send + 'static,
        C::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        Self::with_route(DirectRoute::with_http_connector(connector))
    }

    /// Creates a direct transport from a fully configured Hyper connector.
    ///
    /// See [`DirectRoute::with_connector`].
    pub fn with_connector<C>(connector: C) -> Self
    where
        C: Connect + Clone + Send + Sync + 'static,
    {
        Self::with_route(DirectRoute::with_connector(connector))
    }
}

impl<R: RouteHttp> HyperTransport<R> {
    /// Creates a transport over a host-owned request executor.
    pub fn with_route(route: R) -> Self {
        Self::with_shared_route(Arc::new(route))
    }

    /// Creates a transport over an executor shared with other transports.
    pub fn with_shared_route(route: Arc<R>) -> Self {
        Self {
            route,
            runtime: BlockingRuntime::new(),
        }
    }

    /// The executor this transport routes through.
    pub fn route(&self) -> &Arc<R> {
        &self.route
    }

    /// Runs one request through the executor under the SDK backstop
    /// deadline, observing the dispatch hook so classification does not
    /// depend on the executor's own reporting.
    async fn execute(
        &self,
        request: RouteRequest<'_>,
        external: Option<&(dyn Fn() + Send + Sync)>,
    ) -> std::result::Result<RouteResponse, RoutedFailure> {
        let dispatched = AtomicBool::new(false);
        let on_dispatch = || {
            dispatched.store(true, Ordering::Release);
            if let Some(external) = external {
                external();
            }
        };
        // One absolute deadline for the backstop and for the direct route's
        // connection setup, derived before the route is polled. The direct
        // route gives up on connection setup slightly before the backstop, so
        // a stalled connect is reported as a definite pre-dispatch failure
        // rather than reaching the backstop after the dispatch marker is set.
        let backstop = tokio::time::Instant::now() + request.timeout;
        let connect_deadline = direct_connect_deadline(backstop, request.timeout);
        let outcome = DIRECT_CONNECT_DEADLINE
            .scope(
                Some(connect_deadline),
                tokio::time::timeout_at(backstop, self.route.execute(request, &on_dispatch)),
            )
            .await;
        let dispatched = dispatched.load(Ordering::Acquire);
        match outcome {
            Ok(Ok(response)) => Ok(response),
            // The executor's phase is authoritative for its own failures: a
            // connect refusal reported as pre-dispatch stays definite even if
            // the executor had to call the hook early, as the direct route
            // does. The hook decides only failures the executor never saw.
            Ok(Err(error)) => Err(RoutedFailure {
                dispatched: error.phase != RoutePhase::BeforeDispatch,
                timed_out: false,
                error,
            }),
            Err(_) => Err(RoutedFailure {
                dispatched,
                timed_out: true,
                error: RouteError {
                    phase: if dispatched {
                        RoutePhase::AfterDispatch
                    } else {
                        RoutePhase::BeforeDispatch
                    },
                    message: "HTTP request timed out".to_string(),
                },
            }),
        }
    }

    /// Performs one PIR request, attaching a [`PirHttpFailure`] to every
    /// failure so callers can classify retryability without parsing text.
    /// Non-success statuses are failures here rather than in the PIR client,
    /// which lets the status reach the classifier.
    async fn pir_request(
        &self,
        method: Method,
        url: &str,
        body: Vec<u8>,
    ) -> Result<pir_client::TransportResponse> {
        use PirHttpFailurePhase as Phase;
        // A URL the route cannot even build is a configuration error, not an
        // endpoint outage: classify it as `Build` here so the fleet does not
        // retry other endpoints on it or report it as unavailable.
        if let Err(error) = http::Uri::try_from(url) {
            return Err(PirHttpFailure {
                phase: Phase::Build,
                http_status: None,
            }
            .wrap(format!("build PIR request URL {url:?}: {error}")));
        }
        let response = self
            .execute(
                RouteRequest {
                    method,
                    url,
                    headers: &[],
                    body,
                    timeout: PIR_REQUEST_TIMEOUT,
                    max_response_bytes: MAX_PIR_RESPONSE_BYTES,
                },
                None,
            )
            .await
            .map_err(|failure| {
                let phase = if failure.timed_out {
                    Phase::Timeout
                } else {
                    match failure.error.phase {
                        RoutePhase::BeforeDispatch => Phase::Connect,
                        RoutePhase::AfterDispatch => Phase::Send,
                        RoutePhase::ResponseRead => Phase::Body,
                    }
                };
                let message = if failure.timed_out {
                    "PIR HTTP request timed out".to_string()
                } else {
                    failure.error.message
                };
                PirHttpFailure {
                    phase,
                    http_status: None,
                }
                .wrap(message)
            })?;
        let status = response.status;
        if !(200..300).contains(&status) {
            let preview: String = String::from_utf8_lossy(&response.body)
                .chars()
                .take(256)
                .collect();
            return Err(PirHttpFailure {
                phase: Phase::Status,
                http_status: Some(status),
            }
            .wrap(format!("PIR HTTP status {status} body={preview}")));
        }
        Ok(pir_client::TransportResponse {
            status,
            headers: response.headers,
            body: response.body,
        })
    }

    /// Performs one helper request under a caller-supplied deadline.
    ///
    /// A JSON content type is set for bodies because helper endpoints reject
    /// anything else. Failures before dispatch are definite; a deadline or
    /// connection loss after dispatch is ambiguous; a body-read failure after
    /// headers arrived is a response failure.
    async fn helper_request(
        &self,
        method: Method,
        url: &str,
        body: Vec<u8>,
        timeout: Duration,
    ) -> std::result::Result<HelperResponse, HelperTransportError> {
        let headers: Vec<(String, String)> = if body.is_empty() {
            Vec::new()
        } else {
            vec![("content-type".to_string(), "application/json".to_string())]
        };
        match self
            .execute(
                RouteRequest {
                    method,
                    url,
                    headers: &headers,
                    body,
                    timeout,
                    max_response_bytes: MAX_HELPER_RESPONSE_BYTES,
                },
                None,
            )
            .await
        {
            Ok(response) => {
                let content_type = response.content_type();
                Ok(HelperResponse::new(
                    response.status,
                    response.body,
                    content_type,
                ))
            }
            Err(failure) if !failure.dispatched => Err(HelperTransportError::Transport(format!(
                "helper request failed before dispatch: {}",
                failure.error.message
            ))),
            Err(failure) if failure.timed_out => Err(HelperTransportError::Timeout),
            Err(failure) if failure.error.phase == RoutePhase::ResponseRead => {
                Err(HelperTransportError::Response(format!(
                    "read helper response: {}",
                    failure.error.message
                )))
            }
            Err(failure) => Err(HelperTransportError::Ambiguous(format!(
                "send helper request: {}",
                failure.error.message
            ))),
        }
    }

    async fn chain_request(
        &self,
        method: Method,
        metadata: ChainHttpRequest,
        body: Vec<u8>,
        dispatch: Option<ChainPostDispatch>,
    ) -> std::result::Result<ChainHttpResponse, ChainTransportError> {
        let mark_possible = || {
            if let Some(dispatch) = dispatch.as_ref() {
                dispatch.mark_possible();
            }
        };
        match self
            .execute(
                RouteRequest {
                    method,
                    url: metadata.url(),
                    headers: metadata.headers(),
                    body,
                    timeout: metadata.timeout(),
                    max_response_bytes: metadata.max_response_bytes(),
                },
                Some(&mark_possible),
            )
            .await
        {
            Ok(response) => {
                let content_type = response.content_type();
                Ok(ChainHttpResponse::new(
                    response.status,
                    response.body,
                    content_type,
                    response.headers,
                ))
            }
            Err(failure) if !failure.dispatched => {
                Err(ChainTransportError::definitely_unsent(format!(
                    "vote-chain request failed before dispatch: {}",
                    failure.error.message
                )))
            }
            Err(failure) => Err(ChainTransportError::possibly_dispatched(format!(
                "vote-chain request failed: {}",
                failure.error.message
            ))),
        }
    }
}

fn ensure_rustls_provider() {
    static RUSTLS_PROVIDER: OnceLock<()> = OnceLock::new();
    RUSTLS_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

impl Default for HyperTransport<DirectRoute> {
    fn default() -> Self {
        Self::new()
    }
}

struct BlockingRuntime {
    inner: Option<tokio::runtime::Runtime>,
}

impl BlockingRuntime {
    fn new() -> Self {
        Self {
            inner: Some(
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("create tree-sync HTTP runtime"),
            ),
        }
    }

    fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.inner
            .as_ref()
            .expect("tree-sync HTTP runtime is unavailable")
            .block_on(future)
    }
}

impl Drop for BlockingRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.inner.take() {
            runtime.shutdown_background();
        }
    }
}

impl<R: RouteHttp> pir_client::Transport for HyperTransport<R> {
    fn get<'a>(&'a self, url: &'a str) -> pir_client::TransportFuture<'a> {
        Box::pin(self.pir_request(Method::GET, url, Vec::new()))
    }

    fn post<'a>(&'a self, url: &'a str, body: Vec<u8>) -> pir_client::TransportFuture<'a> {
        Box::pin(self.pir_request(Method::POST, url, body))
    }
}

impl<R: RouteHttp> vote_commitment_tree_client::transport::Transport for HyperTransport<R> {
    fn get(
        &self,
        url: &str,
    ) -> std::result::Result<
        vote_commitment_tree_client::transport::TransportResponse,
        vote_commitment_tree_client::transport::TransportError,
    > {
        self.runtime
            .block_on(self.execute(
                RouteRequest {
                    method: Method::GET,
                    url,
                    headers: &[],
                    body: Vec::new(),
                    timeout: TREE_REQUEST_TIMEOUT,
                    max_response_bytes: MAX_TREE_RESPONSE_BYTES,
                },
                None,
            ))
            .map(
                |response| vote_commitment_tree_client::transport::TransportResponse {
                    status: response.status,
                    body: response.body,
                },
            )
            .map_err(|failure| {
                let message = if failure.timed_out {
                    "vote-tree HTTP request timed out".to_string()
                } else {
                    failure.error.message
                };
                vote_commitment_tree_client::transport::TransportError::Request(message)
            })
    }
}

impl<R: RouteHttp> HelperTransport for HyperTransport<R> {
    fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> HelperFuture<'a> {
        Box::pin(async move {
            self.helper_request(Method::GET, url, Vec::new(), timeout)
                .await
        })
    }

    fn post_json<'a>(&'a self, url: &'a str, body: Vec<u8>, timeout: Duration) -> HelperFuture<'a> {
        Box::pin(async move { self.helper_request(Method::POST, url, body, timeout).await })
    }
}

impl<R: RouteHttp> ChainTransport for HyperTransport<R> {
    fn chain_get<'a>(&'a self, request: ChainHttpRequest) -> ChainTransportFuture<'a> {
        Box::pin(async move {
            self.chain_request(Method::GET, request, Vec::new(), None)
                .await
        })
    }

    fn chain_post_json<'a>(
        &'a self,
        request: ChainHttpRequest,
        json: Vec<u8>,
    ) -> ChainTransportFuture<'a> {
        Box::pin(async move { self.chain_request(Method::POST, request, json, None).await })
    }

    fn chain_post_json_with_dispatch<'a>(
        &'a self,
        request: ChainHttpRequest,
        json: Vec<u8>,
        dispatch: ChainPostDispatch,
    ) -> ChainTransportFuture<'a> {
        Box::pin(async move {
            self.chain_request(Method::POST, request, json, Some(dispatch))
                .await
        })
    }
}

#[cfg(test)]
mod tests {
    mod pir_request;
    mod route;
    mod typed_pir_failure;

    use std::{
        future::Future,
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        pin::Pin,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        task::{Context, Poll},
        thread,
        time::Duration,
    };

    use http::{Method, Uri};
    use hyper_util::client::legacy::connect::HttpConnector;
    use tower_service::Service;

    use crate::chain_submission::{ChainHttpRequest, ChainPostDispatch, ChainTransportFailureKind};

    use super::{
        BlockingRuntime, HelperTransport, HelperTransportError, HyperTransport,
        MAX_HELPER_RESPONSE_BYTES,
    };

    #[derive(Clone)]
    struct ObservedConnector {
        inner: HttpConnector,
        called: Arc<AtomicBool>,
    }

    #[derive(Clone)]
    struct BlockedConnector {
        called: Arc<AtomicBool>,
    }

    impl Service<Uri> for ObservedConnector {
        type Response = <HttpConnector as Service<Uri>>::Response;
        type Error = <HttpConnector as Service<Uri>>::Error;
        type Future =
            Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Service::poll_ready(&mut self.inner, cx)
        }

        fn call(&mut self, uri: Uri) -> Self::Future {
            self.called.store(true, Ordering::Release);
            Box::pin(Service::call(&mut self.inner, uri))
        }
    }

    impl Service<Uri> for BlockedConnector {
        type Response = <HttpConnector as Service<Uri>>::Response;
        type Error = std::io::Error;
        type Future =
            Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _uri: Uri) -> Self::Future {
            self.called.store(true, Ordering::Release);
            Box::pin(async {
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionRefused,
                    "privacy route unavailable",
                ))
            })
        }
    }

    fn read_request_headers(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut chunk = [0u8; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let bytes_read = stream.read(&mut chunk).unwrap();
            assert_ne!(bytes_read, 0, "request ended before its headers");
            request.extend_from_slice(&chunk[..bytes_read]);
        }
        String::from_utf8_lossy(&request).into_owned()
    }

    #[tokio::test]
    async fn helper_transport_uses_injected_http_connector() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_headers(&mut stream);
            assert!(request.starts_with("GET "));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
                )
                .unwrap();
        });

        let called = Arc::new(AtomicBool::new(false));
        let mut inner = HttpConnector::new();
        inner.enforce_http(false);
        let transport = HyperTransport::with_http_connector(ObservedConnector {
            inner,
            called: called.clone(),
        });
        let response = transport
            .get(&format!("http://{address}"), Duration::from_secs(1))
            .await
            .unwrap();

        assert!(called.load(Ordering::Acquire));
        assert_eq!(response.body(), b"{}");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn helper_refused_connection_is_definite() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let transport = HyperTransport::new();
        let result = transport
            .post_json(
                &format!("http://{address}"),
                br#"{"share_index":0}"#.to_vec(),
                Duration::from_secs(1),
            )
            .await;

        assert!(matches!(result, Err(HelperTransportError::Transport(_))));
    }

    #[tokio::test]
    async fn helper_timeout_covers_headers_and_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(100));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n")
                .unwrap();
            thread::sleep(Duration::from_millis(100));
            let _ = stream.write_all(b"ok");
        });

        let transport = HyperTransport::new();
        let result = transport
            .helper_request(
                Method::GET,
                &format!("http://{address}"),
                Vec::new(),
                Duration::from_millis(150),
            )
            .await;

        assert!(matches!(result, Err(HelperTransportError::Timeout)));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn helper_body_failure_after_headers_is_ambiguous() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let bytes_read = stream.read(&mut request).unwrap();
            assert!(request[..bytes_read].starts_with(b"POST "));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nok")
                .unwrap();
        });

        let transport = HyperTransport::new();
        let result = transport
            .helper_request(
                Method::POST,
                &format!("http://{address}"),
                br#"{"share_index":0}"#.to_vec(),
                Duration::from_secs(1),
            )
            .await;

        assert!(matches!(result, Err(HelperTransportError::Response(_))));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn helper_post_closed_before_headers_is_ambiguous() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let bytes_read = stream.read(&mut request).unwrap();
            assert!(request[..bytes_read].starts_with(b"POST "));
            // Close without response headers after receiving the request.
        });

        let transport = HyperTransport::new();
        let result = transport
            .helper_request(
                Method::POST,
                &format!("http://{address}"),
                br#"{"share_index":0}"#.to_vec(),
                Duration::from_secs(1),
            )
            .await;

        assert!(matches!(result, Err(HelperTransportError::Ambiguous(_))));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn helper_response_body_limit_boundary_is_enforced() {
        for body_len in [MAX_HELPER_RESPONSE_BYTES, MAX_HELPER_RESPONSE_BYTES + 1] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request).unwrap();
                let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\n\r\n");
                stream.write_all(header.as_bytes()).unwrap();
                let _ = stream.write_all(&vec![b'x'; body_len]);
            });

            let transport = HyperTransport::new();
            let result = transport
                .get(&format!("http://{address}"), Duration::from_secs(1))
                .await;

            if body_len == MAX_HELPER_RESPONSE_BYTES {
                assert_eq!(result.unwrap().body().len(), MAX_HELPER_RESPONSE_BYTES);
            } else {
                assert!(matches!(result, Err(HelperTransportError::Response(_))));
            }
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn helper_post_sets_json_content_type_and_preserves_response_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_headers(&mut stream).to_ascii_lowercase();
            assert!(request.starts_with("post "));
            assert!(request.contains("content-type: application/json\r\n"));
            stream
                .write_all(
                    b"HTTP/1.1 201 Created\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: 2\r\n\r\nok",
                )
                .unwrap();
        });

        let transport = HyperTransport::new();
        let response = transport
            .post_json(
                &format!("http://{address}"),
                br#"{"share_index":0}"#.to_vec(),
                Duration::from_secs(1),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 201);
        assert_eq!(response.body(), b"ok");
        assert_eq!(
            response.content_type(),
            Some("application/json; charset=utf-8")
        );
        server.join().unwrap();
    }

    fn chain_request(
        url: String,
        timeout: Duration,
        max_response_bytes: usize,
    ) -> ChainHttpRequest {
        ChainHttpRequest::new(
            url,
            vec![
                ("accept".to_string(), "application/json".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            timeout,
            max_response_bytes,
        )
    }

    #[tokio::test]
    async fn chain_transport_applies_sdk_metadata_and_preserves_response_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_headers(&mut stream).to_ascii_lowercase();
            assert!(request.starts_with("post /shielded-vote/v1/cast-vote "));
            assert!(request.contains("accept: application/json\r\n"));
            assert!(request.contains("content-type: application/json\r\n"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nX-Chain: vote\r\nContent-Length: 2\r\n\r\n{}",
                )
                .unwrap();
        });

        let transport = HyperTransport::new();
        let dispatch = ChainPostDispatch::default();
        let response = crate::chain_submission::ChainTransport::chain_post_json_with_dispatch(
            &transport,
            chain_request(
                format!("http://{address}/shielded-vote/v1/cast-vote"),
                Duration::from_secs(1),
                1024,
            ),
            br#"{"vote":"yes"}"#.to_vec(),
            dispatch.clone(),
        )
        .await
        .unwrap();

        assert!(dispatch.is_possible());
        assert_eq!(response.status(), 200);
        assert_eq!(response.body(), b"{}");
        assert_eq!(
            response.content_type(),
            Some("application/json; charset=utf-8")
        );
        assert!(response
            .headers()
            .iter()
            .any(|(name, value)| name == "x-chain" && value == "vote"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn chain_dispatch_marker_stays_clear_when_request_build_fails() {
        let transport = HyperTransport::new();
        let dispatch = ChainPostDispatch::default();

        let error = crate::chain_submission::ChainTransport::chain_post_json_with_dispatch(
            &transport,
            chain_request("\n".to_string(), Duration::from_secs(1), 1024),
            b"{}".to_vec(),
            dispatch.clone(),
        )
        .await
        .unwrap_err();

        assert_eq!(error.kind(), ChainTransportFailureKind::DefinitelyUnsent);
        assert!(!dispatch.is_possible());
    }

    #[tokio::test]
    async fn chain_refused_connection_is_definitely_unsent() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let transport = HyperTransport::new();

        let error = crate::chain_submission::ChainTransport::chain_post_json(
            &transport,
            chain_request(
                format!("http://{address}/shielded-vote/v1/delegate-vote"),
                Duration::from_secs(1),
                1024,
            ),
            b"{}".to_vec(),
        )
        .await
        .unwrap_err();

        assert_eq!(error.kind(), ChainTransportFailureKind::DefinitelyUnsent);
    }

    #[tokio::test]
    async fn chain_timeout_and_response_limit_are_possibly_dispatched() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let timeout_server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_request_headers(&mut stream);
            thread::sleep(Duration::from_millis(100));
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
            );
        });
        let transport = HyperTransport::new();
        let timeout = crate::chain_submission::ChainTransport::chain_post_json(
            &transport,
            chain_request(
                format!("http://{address}/shielded-vote/v1/delegate-vote"),
                Duration::from_millis(25),
                1024,
            ),
            b"{}".to_vec(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            timeout.kind(),
            ChainTransportFailureKind::PossiblyDispatched
        );
        timeout_server.join().unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let limit_server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_request_headers(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 3\r\n\r\n{}x",
                )
                .unwrap();
        });
        let limit = crate::chain_submission::ChainTransport::chain_post_json(
            &transport,
            chain_request(
                format!("http://{address}/shielded-vote/v1/delegate-vote"),
                Duration::from_secs(1),
                2,
            ),
            b"{}".to_vec(),
        )
        .await
        .unwrap_err();
        assert_eq!(limit.kind(), ChainTransportFailureKind::PossiblyDispatched);
        limit_server.join().unwrap();
    }

    #[tokio::test]
    async fn truncated_chain_response_is_possibly_dispatched() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_request_headers(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10\r\n\r\n{}",
                )
                .unwrap();
        });
        let transport = HyperTransport::new();

        let error = crate::chain_submission::ChainTransport::chain_post_json(
            &transport,
            chain_request(
                format!("http://{address}/shielded-vote/v1/delegate-vote"),
                Duration::from_secs(1),
                1024,
            ),
            b"{}".to_vec(),
        )
        .await
        .unwrap_err();

        assert_eq!(error.kind(), ChainTransportFailureKind::PossiblyDispatched);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn chain_transport_returns_redirect_without_following_it() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_request_headers(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:1/leak\r\nContent-Length: 0\r\n\r\n",
                )
                .unwrap();
        });
        let transport = HyperTransport::new();

        let response = crate::chain_submission::ChainTransport::chain_post_json(
            &transport,
            chain_request(
                format!("http://{address}/shielded-vote/v1/delegate-vote"),
                Duration::from_secs(1),
                1024,
            ),
            b"{}".to_vec(),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), 307);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn failing_injected_privacy_connector_never_falls_back_to_direct_io() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let called = Arc::new(AtomicBool::new(false));
        let transport = HyperTransport::with_http_connector(BlockedConnector {
            called: called.clone(),
        });

        let error = crate::chain_submission::ChainTransport::chain_post_json(
            &transport,
            chain_request(
                format!("http://{address}/shielded-vote/v1/delegate-vote"),
                Duration::from_secs(1),
                1024,
            ),
            b"{}".to_vec(),
        )
        .await
        .unwrap_err();

        assert!(called.load(Ordering::Acquire));
        assert_eq!(error.kind(), ChainTransportFailureKind::DefinitelyUnsent);
        assert!(matches!(
            listener.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }

    #[test]
    fn blocking_runtime_drop_does_not_panic_inside_tokio_context() {
        let outer = tokio::runtime::Runtime::new().unwrap();
        let result = std::panic::catch_unwind(|| {
            outer.block_on(async {
                let runtime = BlockingRuntime::new();
                drop(runtime);
            });
        });

        assert!(result.is_ok());
    }
}
