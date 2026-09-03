use std::{future::Future, pin::Pin, sync::OnceLock, time::Duration};

use anyhow::{Context, Result};
use bytes::Bytes;
use http::{Method, Request};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{body::Incoming, Response};
use hyper_rustls::HttpsConnector;
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

struct HyperResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Hyper HTTP transport for client-side network requests.
///
/// `zcash_voting` keeps PIR, tree-sync, helper, and vote-chain traffic behind
/// small transport traits, and includes this adapter for consumers that want
/// pooled HTTP traffic without implementing those protocol transports.
/// [`Self::new`] uses direct HTTP/HTTPS; hosts can instead inject a connector
/// for proxies, custom DNS, or route-lifecycle enforcement.
pub struct HyperTransport {
    client: Box<dyn HyperRequestClient>,
    runtime: BlockingRuntime,
}

impl HyperTransport {
    /// Creates the default direct HTTP/HTTPS transport.
    pub fn new() -> Self {
        let mut connector = HttpConnector::new();
        connector.enforce_http(false);
        Self::with_http_connector(connector)
    }

    /// Creates a transport by applying the SDK's standard Rustls configuration
    /// to a caller-supplied raw HTTP connector.
    ///
    /// This preserves WebPKI roots, HTTP/1 and HTTP/2 support, and cleartext
    /// HTTP compatibility while letting the host control how sockets are
    /// opened. In particular, a wallet can wrap returned I/O with its own
    /// proxy or route-lifecycle guard without reimplementing request handling.
    ///
    /// Hyper pools connections. A host whose route can change must ensure that
    /// already-open I/O is closed or made unusable when the old route is no
    /// longer permitted; selecting a route only when this connector is called
    /// is not sufficient for idle pooled connections.
    pub fn with_http_connector<C>(connector: C) -> Self
    where
        HttpsConnector<C>: Connect + Clone + Send + Sync + 'static,
    {
        ensure_rustls_provider();
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(connector);
        Self::with_connector(https)
    }

    /// Creates a transport from a fully configured Hyper connector.
    ///
    /// Unlike [`Self::with_http_connector`], this does not add TLS or install a
    /// Rustls crypto provider. The caller owns all scheme, TLS, trust-root, and
    /// routing behavior supplied by the connector. Request deadlines,
    /// response limits, response metadata, and ambiguous-outcome
    /// classification remain enforced for helper and vote-chain traffic.
    pub fn with_connector<C>(connector: C) -> Self
    where
        C: Connect + Clone + Send + Sync + 'static,
    {
        let client = Client::builder(TokioExecutor::new()).build(connector);
        Self {
            client: Box::new(client),
            runtime: BlockingRuntime::new(),
        }
    }

    async fn request(
        &self,
        method: Method,
        url: &str,
        body: Vec<u8>,
        max_response_bytes: usize,
    ) -> Result<HyperResponse> {
        let request = Request::builder()
            .method(method)
            .uri(url)
            .body(Full::new(Bytes::from(body)))
            .context("build HTTP request")?;
        let response = self
            .client
            .request(request)
            .await
            .context("send HTTP request")?;
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
                anyhow::anyhow!(
                    "read HTTP response body (limit {max_response_bytes} bytes): {error}"
                )
            })?
            .to_bytes()
            .to_vec();

        Ok(HyperResponse {
            status,
            headers,
            body,
        })
    }

    /// Performs one helper request under a caller-supplied deadline.
    ///
    /// A JSON content type is set for bodies because helper endpoints reject
    /// anything else. Timeouts are reported distinctly from other failures so
    /// higher-level clients can tell an ambiguous submission from a refused
    /// one.
    async fn helper_request(
        &self,
        method: Method,
        url: &str,
        body: Vec<u8>,
        timeout: Duration,
    ) -> std::result::Result<HelperResponse, HelperTransportError> {
        let has_body = !body.is_empty();
        let request = {
            let builder = Request::builder().method(method).uri(url);
            let builder = if has_body {
                builder.header(http::header::CONTENT_TYPE, "application/json")
            } else {
                builder
            };
            builder
                .body(Full::new(Bytes::from(body)))
                .map_err(|error| {
                    HelperTransportError::Transport(format!("build helper request: {error}"))
                })?
        };

        tokio::time::timeout(timeout, async {
            let response = self.client.request(request).await.map_err(|error| {
                let message = format!("send helper request: {error}");
                if error.is_connect() {
                    HelperTransportError::Transport(message)
                } else {
                    // Once dispatch has progressed past connection setup,
                    // Hyper cannot prove that a POST body was not received.
                    HelperTransportError::Ambiguous(message)
                }
            })?;

            let status = response.status().as_u16();
            let content_type = response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let body = Limited::new(response.into_body(), MAX_HELPER_RESPONSE_BYTES)
                .collect()
                .await
                .map_err(|error| {
                    HelperTransportError::Response(format!(
                        "read helper response body (limit {MAX_HELPER_RESPONSE_BYTES} bytes): {error}"
                    ))
                })?
                .to_bytes()
                .to_vec();

            Ok(HelperResponse::new(status, body, content_type))
        })
        .await
        .map_err(|_| HelperTransportError::Timeout)?
    }
}

fn ensure_rustls_provider() {
    static RUSTLS_PROVIDER: OnceLock<()> = OnceLock::new();
    RUSTLS_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

impl Default for HyperTransport {
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

impl pir_client::Transport for HyperTransport {
    fn get<'a>(&'a self, url: &'a str) -> pir_client::TransportFuture<'a> {
        Box::pin(async move {
            tokio::time::timeout(
                PIR_REQUEST_TIMEOUT,
                self.request(Method::GET, url, Vec::new(), MAX_PIR_RESPONSE_BYTES),
            )
            .await
            .context("PIR HTTP request timed out")?
            .map(|response| pir_client::TransportResponse {
                status: response.status,
                headers: response.headers,
                body: response.body,
            })
        })
    }

    fn post<'a>(&'a self, url: &'a str, body: Vec<u8>) -> pir_client::TransportFuture<'a> {
        Box::pin(async move {
            tokio::time::timeout(
                PIR_REQUEST_TIMEOUT,
                self.request(Method::POST, url, body, MAX_PIR_RESPONSE_BYTES),
            )
            .await
            .context("PIR HTTP request timed out")?
            .map(|response| pir_client::TransportResponse {
                status: response.status,
                headers: response.headers,
                body: response.body,
            })
        })
    }
}

impl vote_commitment_tree_client::transport::Transport for HyperTransport {
    fn get(
        &self,
        url: &str,
    ) -> std::result::Result<
        vote_commitment_tree_client::transport::TransportResponse,
        vote_commitment_tree_client::transport::TransportError,
    > {
        self.runtime
            .block_on(async {
                tokio::time::timeout(
                    TREE_REQUEST_TIMEOUT,
                    self.request(Method::GET, url, Vec::new(), MAX_TREE_RESPONSE_BYTES),
                )
                .await
                .context("vote-tree HTTP request timed out")?
            })
            .map(
                |response| vote_commitment_tree_client::transport::TransportResponse {
                    status: response.status,
                    body: response.body,
                },
            )
            .map_err(|e| {
                vote_commitment_tree_client::transport::TransportError::Request(e.to_string())
            })
    }
}

impl HelperTransport for HyperTransport {
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

impl HyperTransport {
    async fn chain_request(
        &self,
        method: Method,
        metadata: ChainHttpRequest,
        body: Vec<u8>,
        dispatch: Option<ChainPostDispatch>,
    ) -> std::result::Result<ChainHttpResponse, ChainTransportError> {
        let mut builder = Request::builder().method(method).uri(metadata.url());
        for (name, value) in metadata.headers() {
            builder = builder.header(name, value);
        }
        let request = builder
            .body(Full::new(Bytes::from(body)))
            .map_err(|error| {
                ChainTransportError::definitely_unsent(format!(
                    "build vote-chain request failed: {error}"
                ))
            })?;

        tokio::time::timeout(metadata.timeout(), async {
            if let Some(dispatch) = dispatch {
                dispatch.mark_possible();
            }
            let response = self.client.request(request).await.map_err(|error| {
                let message = format!("send vote-chain request failed: {error}");
                if error.is_connect() {
                    ChainTransportError::definitely_unsent(message)
                } else {
                    ChainTransportError::possibly_dispatched(message)
                }
            })?;
            let status = response.status().as_u16();
            let content_type = response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
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
            let body = Limited::new(response.into_body(), metadata.max_response_bytes())
                .collect()
                .await
                .map_err(|error| {
                    ChainTransportError::possibly_dispatched(format!(
                        "read vote-chain response body (limit {} bytes) failed: {error}",
                        metadata.max_response_bytes()
                    ))
                })?
                .to_bytes()
                .to_vec();
            Ok(ChainHttpResponse::new(status, body, content_type, headers))
        })
        .await
        .map_err(|_| ChainTransportError::possibly_dispatched("vote-chain request timed out"))?
    }
}

impl ChainTransport for HyperTransport {
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
