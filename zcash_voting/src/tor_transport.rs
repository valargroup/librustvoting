//! Tor HTTP transport for PIR traffic.
//!
//! [`crate::HyperTransport`] is the built-in direct transport; this is its Tor
//! counterpart, for a wallet that routes foreground traffic through an `arti`
//! client. It lives in this crate rather than in `pir-client` so that both
//! built-in transports share one set of response ceilings and deadlines, and
//! because the eventual vote-tree counterpart has to satisfy a second trait that
//! only this crate can see.
//!
//! Vote-tree sync is **not** covered yet. That trait is synchronous, so a Tor
//! implementation would have to block on an `arti` future, and
//! `precompute::sync_vote_tree` reaches its `VoteTreeSync` through a process
//! registry that a host cannot supply a transport to. Both need solving together
//! or the code is unreachable; see this file's history in the prototype notes.
//!
//! Route **policy** stays with the host. This type takes an already-usable
//! client and never decides whether Tor is the desired route, whether a
//! half-bootstrapped client may be used, or whether a circuit should be
//! isolated — that state belongs to the app's lifecycle, not to an SDK.

use std::time::Duration;

use anyhow::{Context, Result};
use bytes::{Buf, Bytes};
use http::{Method, Uri};
use http_body_util::{BodyExt, Full};

use crate::backend::zcash_client_backend::tor::{Client as TorClient, Error as TorError};
use crate::http_transport::{MAX_PIR_RESPONSE_BYTES, PIR_REQUEST_TIMEOUT};

struct TorResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// HTTP transport that sends PIR requests over a Tor client.
///
/// Construct one per route decision and reuse it: the wrapped client already
/// pools circuits, so a fresh transport per request buys nothing.
pub struct TorTransport {
    client: TorClient,
}

impl TorTransport {
    /// Wraps an already-usable Tor client.
    ///
    /// The caller is responsible for having decided that Tor is the route to
    /// use and that this client is bootstrapped. Pass an isolated client if the
    /// traffic warrants its own circuit; this type will not choose for you.
    pub fn new(client: TorClient) -> Self {
        Self { client }
    }

    async fn request(
        &self,
        method: Method,
        url: &str,
        body: Option<Vec<u8>>,
        max_response_bytes: usize,
        timeout: Duration,
    ) -> Result<TorResponse> {
        let uri: Uri = url.parse().with_context(|| format!("invalid URL {url}"))?;
        let response = tokio::time::timeout(timeout, async {
            match body {
                Some(body) => {
                    self.client
                        .http_post(
                            uri,
                            |builder| builder,
                            Full::new(Bytes::from(body)),
                            move |incoming| collect_capped(incoming, max_response_bytes),
                            0,
                            |_| None,
                        )
                        .await
                }
                None => {
                    self.client
                        .http_get(
                            uri,
                            |builder| builder,
                            move |incoming| collect_capped(incoming, max_response_bytes),
                            0,
                            |_| None,
                        )
                        .await
                }
            }
        })
        .await
        .with_context(|| format!("{method} over Tor timed out after {}s", timeout.as_secs()))?
        .with_context(|| format!("{method} over Tor failed"))?;

        Ok(TorResponse {
            status: response.status().as_u16(),
            headers: response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_string(), value.to_string()))
                })
                .collect(),
            body: response.into_body(),
        })
    }
}

/// Reads a response body, refusing one over `max_bytes` before it is buffered
/// whole. `TorError` has no size variant, so this reports as an IO error.
///
/// Generic over the body so the ceiling is testable without a live connection;
/// callers always pass hyper's `Incoming`.
async fn collect_capped<B>(mut body: B, max_bytes: usize) -> Result<Vec<u8>, TorError>
where
    B: hyper::body::Body + Unpin,
    B::Error: std::fmt::Display,
{
    let mut collected = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|error| {
            TorError::Io(std::io::Error::other(format!(
                "read response body: {error}"
            )))
        })?;
        if let Ok(mut data) = frame.into_data() {
            if collected.len() + data.remaining() > max_bytes {
                return Err(TorError::Io(std::io::Error::other(format!(
                    "response exceeds {max_bytes} bytes"
                ))));
            }
            while data.has_remaining() {
                let chunk_len = {
                    let chunk = data.chunk();
                    collected.extend_from_slice(chunk);
                    chunk.len()
                };
                data.advance(chunk_len);
            }
        }
    }
    Ok(collected)
}

impl pir_client::Transport for TorTransport {
    fn get<'a>(&'a self, url: &'a str) -> pir_client::TransportFuture<'a> {
        Box::pin(async move {
            self.request(
                Method::GET,
                url,
                None,
                MAX_PIR_RESPONSE_BYTES,
                PIR_REQUEST_TIMEOUT,
            )
            .await
            .map(pir_transport_response)
        })
    }

    fn post<'a>(&'a self, url: &'a str, body: Vec<u8>) -> pir_client::TransportFuture<'a> {
        Box::pin(async move {
            self.request(
                Method::POST,
                url,
                Some(body),
                MAX_PIR_RESPONSE_BYTES,
                PIR_REQUEST_TIMEOUT,
            )
            .await
            .map(pir_transport_response)
        })
    }
}

fn pir_transport_response(response: TorResponse) -> pir_client::TransportResponse {
    pir_client::TransportResponse {
        status: response.status,
        headers: response.headers,
        body: response.body,
    }
}

#[cfg(test)]
mod tests {
    use super::collect_capped;
    use bytes::Bytes;
    use http_body_util::Full;

    #[tokio::test]
    async fn body_within_the_ceiling_is_returned_whole() {
        let body = Full::new(Bytes::from(vec![7u8; 64]));

        let collected = collect_capped(body, 64).await.expect("64 bytes fits in 64");

        assert_eq!(collected, vec![7u8; 64]);
    }

    #[tokio::test]
    async fn body_over_the_ceiling_is_refused() {
        let body = Full::new(Bytes::from(vec![7u8; 65]));

        let error = collect_capped(body, 64)
            .await
            .expect_err("65 bytes must not pass a 64 byte ceiling");

        assert!(error.to_string().contains("exceeds 64 bytes"), "{error}");
    }
}
