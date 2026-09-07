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
/// Total wall-clock budget for one PIR request, across both attempts.
///
/// A PIR fetch that succeeds does so in a few seconds; one against a dead
/// endpoint does not slow down, it stalls and never returns. Measured against a
/// staging endpoint, successful fetches completed in 6-8 seconds while failures
/// ran the full deadline. A single long deadline therefore spends a minute
/// discovering what a few seconds already showed.
///
/// The fix is not a shorter deadline, though. A wall clock cannot tell a
/// stalled endpoint from a slow one, and cutting every request short to fail
/// fast turns a link that merely needs longer — a congested mobile network, a
/// distant endpoint, a multi-megabyte tier under [`MAX_PIR_RESPONSE_BYTES`] —
/// from slow into broken, because each attempt restarts the transfer from
/// zero. So the budget is split along two axes instead: a tight bound on
/// connection setup, where a dead endpoint reveals itself, and a generous
/// bound on the request as a whole, where a slow link needs room.
///
/// Two attempts share this one budget, so the worst case is unchanged from the
/// single 60-second deadline this replaces and no caller waits longer than
/// before. That ceiling matters because [`crate::pir::PirFleet`] failover
/// multiplies it by the endpoint count.
const PIR_REQUEST_BUDGET: Duration = Duration::from_secs(60);
/// Connection-setup bound for the first PIR attempt.
///
/// Short enough that a blackholed endpoint costs seconds rather than the whole
/// budget, and a retry still has room. Connection setup is the one phase whose
/// duration says nothing about how much work remains, so bounding it tightly
/// costs a slow-but-working link nothing.
const PIR_FIRST_ATTEMPT_CONNECT: Duration = Duration::from_secs(5);
/// Whole-request bound for the first PIR attempt.
///
/// Comfortably above the measured 6-8 second success path, so a healthy fetch
/// never reaches a second attempt, while a stalled one is abandoned with most
/// of the budget still unspent.
const PIR_FIRST_ATTEMPT_OVERALL: Duration = Duration::from_secs(15);
/// Connection-setup bound for the final PIR attempt.
///
/// Looser than the first: having already failed once, the cost of waiting a
/// little longer is lower than the cost of giving up on a slow connection.
const PIR_FINAL_ATTEMPT_CONNECT: Duration = Duration::from_secs(10);
/// Floor on what must remain of [`PIR_REQUEST_BUDGET`] to attempt again.
///
/// Guards against issuing a request with a deadline too short to be worth the
/// round trip, if a first attempt somehow consumed nearly the whole budget.
const PIR_MIN_RETRY_BUDGET: Duration = Duration::from_secs(1);
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
    /// Optional tighter bound on connection setup alone, when the caller wants
    /// to abandon an endpoint that never answers without also cutting short
    /// one that is merely slow. Connection setup is the one phase whose
    /// duration says nothing about how much work remains.
    ///
    /// An executor that enforces this must say so through
    /// [`RouteHttp::enforces_connect_timeout`] and must report the expiry as
    /// [`RoutePhase::BeforeDispatch`]; the SDK draws no conclusion from the
    /// clock otherwise. `None` leaves connection setup bounded only by
    /// `timeout`.
    pub connect_timeout: Option<Duration>,
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
/// - `connect_timeout` is optional to honor, because only the executor can see
///   connection setup. An executor that does bound setup by it says so through
///   [`RouteHttp::enforces_connect_timeout`], and only then does the SDK read
///   a pre-dispatch failure at or after that budget as the budget expiring
///   rather than as the endpoint's answer. An executor that ignores it is
///   bounded by `timeout` alone and loses nothing else.
/// - Report `phase` truthfully. It is consulted for failures the dispatch hook
///   cannot classify, such as a body-read failure after headers arrived. A
///   `BeforeDispatch` phase reported after the hook was called is not
///   honored: the hook already said bytes may have left, and the SDK keeps
///   the more conservative answer. The one exception is an executor whose
///   HTTP client fuses connection setup with the first write and therefore
///   must call the hook before it can tell that connection setup failed; it
///   says so through [`RouteHttp::hook_precedes_connection_setup`], and
///   only then is its `BeforeDispatch` honored after the hook.
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

    /// Whether this executor must call `on_dispatch` before it can tell that
    /// connection setup failed, because its client fuses connection setup
    /// with the first write. Only such an executor has a `BeforeDispatch`
    /// failure honored after the hook; it must then report `BeforeDispatch`
    /// only for failures its client attributes to connection setup, never
    /// for one that may have followed a write. The default is `false`: any
    /// failure after the hook is possibly dispatched.
    fn hook_precedes_connection_setup(&self) -> bool {
        false
    }

    /// Whether this executor bounds connection setup by
    /// [`RouteRequest::connect_timeout`] and reports its expiry as
    /// [`RoutePhase::BeforeDispatch`].
    ///
    /// The SDK cannot see connection setup, so it can only tell a budget that
    /// expired from an endpoint that refused by asking. An executor that
    /// declares this has its pre-dispatch failures at or after the budget read
    /// as "no answer yet", which a caller free to repeat the request may act
    /// on. The default is `false`: an executor that ignores the budget has its
    /// pre-dispatch failures read as the definite answers they are, so a
    /// refusal that happens to arrive late is never mistaken for a timeout.
    ///
    /// Declaring this without enforcing the budget is the one way to get a
    /// definite refusal repeated. It never makes an ambiguous failure look
    /// safe, though: only `BeforeDispatch` is eligible, and that already means
    /// no request byte left.
    fn enforces_connect_timeout(&self) -> bool {
        false
    }
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

/// The connection-setup deadline for a request, honoring an optional budget.
///
/// A caller-supplied budget only tightens the deadline. Relaxing it would let
/// connection setup outlive the backstop, and the classification the whole
/// dispatch model rests on depends on connect giving up first — so the derived
/// lead stays the upper bound whatever the caller asks for.
///
/// A tighter budget is what lets a caller distinguish an endpoint that never
/// answers from one that is merely slow: connection setup is bounded on its
/// own, so abandoning it early costs a slow-but-progressing transfer nothing.
fn connect_deadline(
    backstop: tokio::time::Instant,
    timeout: Duration,
    started: tokio::time::Instant,
    budget: Option<Duration>,
) -> tokio::time::Instant {
    let derived = direct_connect_deadline(backstop, timeout);
    // A budget too large to represent as an instant cannot tighten anything,
    // so it falls back to the derived lead rather than panicking. The budget
    // is caller-supplied through a public field, so it must not be trusted to
    // be addable to the current instant.
    match budget.and_then(|budget| started.checked_add(budget)) {
        Some(bounded) => derived.min(bounded),
        None => derived,
    }
}

/// Applies the in-flight request deadline to connection setup.
///
/// Wrapping the complete TCP+TLS connector matters: a stalled TLS handshake
/// has still not dispatched an HTTP request, so it must surface as a connect
/// failure rather than race the whole-request deadline into an ambiguous
/// outcome.
struct ConnectDeadlineConnector<C> {
    inner: C,
    /// Timer that wakes this connector when the current request's connect
    /// deadline passes, held across polls so its registration survives, and
    /// tagged with the deadline it was armed for.
    ///
    /// Checking the clock on entry to `poll_ready` is not enough on its own: a
    /// connector that withholds readiness is polled once and then only when it
    /// says so, which may be long after the deadline or never. Holding a timer
    /// that registered the caller's waker is what turns the bound into one
    /// this connector enforces rather than one it happens to notice.
    ///
    /// The deadline is kept alongside because a pooled connector outlives one
    /// request: a timer armed for an earlier request must not bound a later
    /// one, in either direction.
    readiness_timer: Option<(tokio::time::Instant, Pin<Box<tokio::time::Sleep>>)>,
}

/// Cloning drops any armed timer: a clone serves a different connection, whose
/// readiness runs under its own request deadline.
impl<C: Clone> Clone for ConnectDeadlineConnector<C> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            readiness_timer: None,
        }
    }
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

    /// Readiness is part of connection setup, so the deadline covers it too,
    /// by racing the inner connector against a timer rather than by sampling
    /// the clock.
    ///
    /// Wrapping only the future returned by `call` would leave a gap: a
    /// connector that withholds readiness past the deadline and then reports a
    /// failure would produce a pre-dispatch error arriving after the budget,
    /// which the SDK reads as the budget expiring because this route declares
    /// that it enforces the budget. Past the deadline the answer is that
    /// connection setup ran out of time, whatever the inner connector would
    /// have said next.
    ///
    /// The timer is what makes that a bound rather than an observation. A
    /// connector that returns `Pending` decides when this is polled again, so
    /// a clock check alone runs once and may never run after the deadline; the
    /// stall would then be caught only by the whole-request backstop, which is
    /// looser than the budget this route promises to enforce. Holding a
    /// `Sleep` that registered `cx.waker()` guarantees a poll at the deadline.
    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        let deadline = DIRECT_CONNECT_DEADLINE
            .try_with(|deadline| *deadline)
            .ok()
            .flatten();
        if let Some(deadline) = deadline {
            // Re-arm whenever the deadline is not the one the held timer was
            // armed for, so a pooled connector never bounds a request by a
            // previous request's deadline.
            if self
                .readiness_timer
                .as_ref()
                .is_none_or(|(armed_for, _)| *armed_for != deadline)
            {
                self.readiness_timer =
                    Some((deadline, Box::pin(tokio::time::sleep_until(deadline))));
            }
            let (_, timer) = self
                .readiness_timer
                .as_mut()
                .expect("the timer was just armed");
            // Polling the held timer both answers "has the deadline passed"
            // and registers this waker with the timer wheel, so a stalled
            // readiness is woken at the deadline instead of never.
            if timer.as_mut().poll(cx).is_ready() {
                self.readiness_timer = None;
                return std::task::Poll::Ready(Err(connect_timeout_error()));
            }
        }
        let readiness = self
            .inner
            .poll_ready(cx)
            .map(|result| result.map_err(Into::into));
        // The timer belongs to the request whose readiness is still pending.
        // Once the inner connector is ready the wait is over, so drop it
        // rather than let it outlive its deadline into the next request.
        if readiness.is_ready() {
            self.readiness_timer = None;
        }
        readiness
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
                    .map_err(|_| connect_timeout_error())?
                    .map_err(Into::into),
                None => future.await.map_err(Into::into),
            }
        })
    }
}

/// The failure both connection-setup phases report when the deadline expires.
///
/// Hyper attributes this to connection setup, so [`DirectRoute`] reports it as
/// [`RoutePhase::BeforeDispatch`] and the SDK reads it as the connect budget
/// expiring rather than as the endpoint's answer.
fn connect_timeout_error() -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "connection setup timed out before request dispatch",
    ))
}

/// SDK-owned direct HTTP/HTTPS executor with a pooled Hyper client.
///
/// Built by [`Self::new`] or [`Self::with_http_connector`], connection setup
/// runs under the request deadline, so a TCP or TLS stall is reported as a
/// connect failure and classified as pre-dispatch. [`Self::with_connector`]
/// uses the connector exactly as given and so bounds nothing itself; a route
/// built that way is bounded by the whole-request deadline alone.
pub struct DirectRoute {
    client: Box<dyn HyperRequestClient>,
    /// Whether this route's connector stack includes
    /// [`ConnectDeadlineConnector`], and so actually bounds connection setup
    /// by [`RouteRequest::connect_timeout`]. Only a route that does may have
    /// its late pre-dispatch failures read as that budget expiring, so this
    /// tracks how the route was built rather than assuming the wrapper.
    enforces_connect_timeout: bool,
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
        Self::from_connector(
            ConnectDeadlineConnector {
                inner: https,
                readiness_timer: None,
            },
            true,
        )
    }

    /// Uses a fully configured Hyper connector without adding TLS.
    ///
    /// The connector is used as given, so this route bounds nothing itself:
    /// only [`Self::with_http_connector`] and [`Self::new`] wrap the connector
    /// in the request's connection-setup deadline. Such a route reports
    /// [`RouteHttp::enforces_connect_timeout`] as `false`, so a caller's
    /// `connect_timeout` has no effect on it and its pre-dispatch failures
    /// stay the definite answers they are, never re-read as a budget
    /// expiring. A host that wants both behaviors should wrap its connector
    /// in its own deadline and route through its own [`RouteHttp`], or use
    /// [`Self::with_http_connector`].
    pub fn with_connector<C>(connector: C) -> Self
    where
        C: Connect + Clone + Send + Sync + 'static,
    {
        Self::from_connector(connector, false)
    }

    /// Builds the route over `connector`, recording whether that connector
    /// stack bounds connection setup by the request's `connect_timeout`.
    ///
    /// The flag is passed rather than inferred because only the constructor
    /// that installs [`ConnectDeadlineConnector`] knows it is there.
    fn from_connector<C>(connector: C, enforces_connect_timeout: bool) -> Self
    where
        C: Connect + Clone + Send + Sync + 'static,
    {
        let client = Client::builder(TokioExecutor::new()).build(connector);
        Self {
            client: Box::new(client),
            enforces_connect_timeout,
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
            // directly gets the same deadline derived here instead, honoring
            // `connect_timeout` the same way.
            let connect_timeout = request.connect_timeout;
            let deadline = DIRECT_CONNECT_DEADLINE
                .try_with(|deadline| *deadline)
                .ok()
                .flatten()
                .or_else(|| {
                    let started = tokio::time::Instant::now();
                    started.checked_add(request.timeout).map(|backstop| {
                        connect_deadline(backstop, request.timeout, started, connect_timeout)
                    })
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

    /// Hyper's pooled client offers no hook between connection setup and the
    /// first write, so the hook is called before both and connect failures,
    /// which Hyper reports distinctly, are honored as pre-dispatch.
    fn hook_precedes_connection_setup(&self) -> bool {
        true
    }

    /// True only when this route was built with [`ConnectDeadlineConnector`]
    /// in its stack, which wraps the complete TCP+TLS connector in the
    /// request's connect deadline and reports its expiry as a connect error —
    /// surfaced distinctly by Hyper and reported here as pre-dispatch. For
    /// such a route, a connect failure at or after that deadline is the
    /// deadline, not a refusal that arrived late.
    ///
    /// [`Self::with_connector`] installs no such wrapper, so a route built
    /// that way claims nothing and keeps its pre-dispatch failures definite.
    fn enforces_connect_timeout(&self) -> bool {
        self.enforces_connect_timeout
    }
}

/// Failure of one routed request with the SDK's own dispatch observation.
struct RoutedFailure {
    /// Whether the executor called the dispatch hook before failing.
    dispatched: bool,
    /// Whether the SDK backstop deadline fired.
    timed_out: bool,
    /// Whether the connection-setup deadline expired rather than the endpoint
    /// answering. Distinct from `timed_out`, which is the whole-request
    /// backstop: a connect deadline ends the attempt before any request byte
    /// can have left, so the failure stays definite. A caller free to repeat
    /// the request can treat this as "no answer yet" rather than an answer.
    connect_deadline_expired: bool,
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
    /// The connector is used as given, so connection setup is bounded only by
    /// the whole-request deadline and a caller's `connect_timeout` has no
    /// effect. See [`DirectRoute::with_connector`].
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
        //
        // A caller-supplied connect budget only ever tightens that deadline,
        // never relaxes it, so the derived lead remains the upper bound and
        // connection setup still gives up before the backstop can fire.
        let started = tokio::time::Instant::now();
        let backstop = started + request.timeout;
        let connect_deadline =
            connect_deadline(backstop, request.timeout, started, request.connect_timeout);
        let outcome = DIRECT_CONNECT_DEADLINE
            .scope(
                Some(connect_deadline),
                tokio::time::timeout_at(backstop, self.route.execute(request, &on_dispatch)),
            )
            .await;
        let dispatched = dispatched.load(Ordering::Acquire);
        match outcome {
            Ok(Ok(response)) => Ok(response),
            // A post-dispatch phase is the executor's own admission that
            // bytes may have left, hook or not. A pre-dispatch phase after
            // the hook is trusted only from an executor whose client fuses
            // connection setup with the first write and so had to call the
            // hook early (the direct route): its connect failures are
            // reported distinctly and stay definite. Any other executor's
            // failure after the hook is possibly dispatched, as the hook
            // contract promises.
            Ok(Err(error)) => {
                // A pre-dispatch failure at or after the connect deadline is
                // that deadline expiring, not the endpoint refusing: the route
                // was still setting the connection up when its budget ran out.
                //
                // Only an executor that enforces the budget can be read this
                // way. One that ignores it never had a deadline to expire, so
                // a refusal it happens to report late is exactly what it says
                // it is, and the clock would only manufacture a timeout out of
                // a definite answer. Only pre-dispatch qualifies either way,
                // so this can never mark a possibly-dispatched failure as safe
                // to repeat.
                let connect_deadline_expired = error.phase == RoutePhase::BeforeDispatch
                    && self.route.enforces_connect_timeout()
                    && tokio::time::Instant::now() >= connect_deadline;
                let dispatched = match error.phase {
                    RoutePhase::BeforeDispatch => {
                        dispatched && !self.route.hook_precedes_connection_setup()
                    }
                    RoutePhase::AfterDispatch | RoutePhase::ResponseRead => true,
                };
                Err(RoutedFailure {
                    dispatched,
                    timed_out: false,
                    connect_deadline_expired,
                    error,
                })
            }
            Err(_) => Err(RoutedFailure {
                dispatched,
                timed_out: true,
                connect_deadline_expired: false,
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
        // Two attempts share one budget. Only a request that ran out of time
        // is repeated — either the whole-request backstop or the connect
        // deadline. Every other failure is a definite answer about this
        // endpoint: a refused connection, a protocol error, a non-success
        // status. Repeating one would neither change it nor be free.
        //
        // Repeating is safe here in a way it is not for helper or chain POSTs.
        // A PIR query is an idempotent read, so a second attempt cannot double
        // an effect, and re-sending the identical encrypted query tells the
        // server nothing it did not already have — which item is being fetched
        // is exactly what PIR hides. A connect timeout is definitely unsent,
        // and the backstop case only repeats a read.
        let started = tokio::time::Instant::now();
        // The first attempt gets the only copy of the body the retry needs;
        // the final attempt takes ownership, so one clone covers both.
        let first = self
            .execute(
                RouteRequest {
                    method: method.clone(),
                    url,
                    headers: &[],
                    body: body.clone(),
                    timeout: PIR_FIRST_ATTEMPT_OVERALL,
                    connect_timeout: Some(PIR_FIRST_ATTEMPT_CONNECT),
                    max_response_bytes: MAX_PIR_RESPONSE_BYTES,
                },
                None,
            )
            .await;
        let response = match first {
            Ok(response) => Ok(response),
            Err(failure) if failure.timed_out || failure.connect_deadline_expired => {
                let remaining = PIR_REQUEST_BUDGET.saturating_sub(started.elapsed());
                if remaining < PIR_MIN_RETRY_BUDGET {
                    Err(failure)
                } else {
                    self.execute(
                        RouteRequest {
                            method,
                            url,
                            headers: &[],
                            body,
                            timeout: remaining,
                            connect_timeout: Some(PIR_FINAL_ATTEMPT_CONNECT),
                            max_response_bytes: MAX_PIR_RESPONSE_BYTES,
                        },
                        None,
                    )
                    .await
                }
            }
            Err(failure) => Err(failure),
        };
        let response = response.map_err(|failure| {
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
                    connect_timeout: None,
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
                    connect_timeout: None,
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
                    connect_timeout: None,
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
    mod pir_retry;
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
