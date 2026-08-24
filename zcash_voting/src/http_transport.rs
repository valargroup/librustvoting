use std::{future::Future, sync::OnceLock, time::Duration};

use anyhow::{Context, Result};
use bytes::Bytes;
use http::{Method, Request};
use http_body_util::{BodyExt, Full, Limited};
use hyper_rustls::HttpsConnector;
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};

type RequestBody = Full<Bytes>;
type HyperClient = Client<HttpsConnector<HttpConnector>, RequestBody>;

// PIR responses are normally below 1 MiB after layout validation. Keep a
// generous fixed ceiling in the built-in transport so a server cannot force an
// unbounded allocation before the client validates the negotiated geometry,
// and bound the complete request so a slow or endless body cannot stall setup.
//
// The PIR pair is `pub(crate)` so every built-in transport enforces the same
// limits. A `Transport` impl is handed a bare URL and cannot tell `/tier0` from
// a query response, so the ceiling is not something an implementor can derive;
// leaving each one to rediscover it is how a transport ends up with no ceiling
// at all.
pub(crate) const MAX_PIR_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const PIR_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
// Tree pages are JSON encoded and can be larger than the compact state
// responses. Bound every tree response before buffering or parsing it, and
// cover connection setup plus the complete body read with one deadline.
const MAX_TREE_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const TREE_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

struct HyperResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Direct Hyper/Rustls HTTP transport for client-side network requests.
///
/// `zcash_voting` keeps PIR and tree-sync fetching behind small transport
/// traits, and includes this adapter for consumers that want direct
/// cleartext/HTTPS traffic without providing their own transport.
pub struct HyperTransport {
    client: HyperClient,
    /// Only the synchronous vote-tree trait needs this, so a PIR-only consumer
    /// never spawns a worker thread pool it will not use. Constructing this type
    /// is therefore cheap and thread-agnostic: callers may build it on an async
    /// worker and hand it to a blocking one.
    runtime: OnceLock<BlockingRuntime>,
}

impl HyperTransport {
    pub fn new() -> Self {
        ensure_rustls_provider();
        let mut connector = HttpConnector::new();
        connector.enforce_http(false);
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(connector);
        let client = Client::builder(TokioExecutor::new()).build(https);

        Self {
            client,
            runtime: OnceLock::new(),
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

/// Owns a runtime so a synchronous transport trait can drive async HTTP.
///
/// `vote_commitment_tree_client`'s `Transport::get` is synchronous, so some
/// runtime has to block. Callers must therefore stay off an async worker when
/// they reach a synchronous transport method.
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
            .get_or_init(BlockingRuntime::new)
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

#[cfg(test)]
mod tests {
    use super::{BlockingRuntime, HyperTransport};

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

    #[test]
    fn pir_only_use_never_builds_a_blocking_runtime() {
        let transport = HyperTransport::new();

        assert!(
            transport.runtime.get().is_none(),
            "constructing a transport must not spawn a thread pool the PIR path never uses"
        );
    }

    #[test]
    fn construct_and_drop_inside_tokio_context_does_not_panic() {
        // Lets a host build the transport on an async worker and hand it to a
        // blocking one, rather than deferring construction behind a closure.
        let outer = tokio::runtime::Runtime::new().unwrap();
        let result = std::panic::catch_unwind(|| {
            outer.block_on(async {
                drop(HyperTransport::new());
            });
        });

        assert!(result.is_ok());
    }
}
