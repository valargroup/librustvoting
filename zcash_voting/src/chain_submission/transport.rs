//! Host-owned HTTP transport seam for vote-chain requests.

use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use super::MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES;

/// Largest vote-chain response accepted by the SDK.
pub const MAX_CHAIN_HTTP_RESPONSE_BYTES: usize = 256 * 1024;

/// Dispatch certainty attached to a transport failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChainTransportFailureKind {
    /// No request byte reached a network stack capable of delivering it.
    DefinitelyUnsent,
    /// The request may have reached the configured vote-chain endpoint.
    PossiblyDispatched,
}

/// Failure of one vote-chain HTTP request.
///
/// Only a transport implementation can produce `DefinitelyUnsent`. Timeout,
/// cancellation after dispatch, response-read failure, and response limits are
/// all `PossiblyDispatched`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainTransportError {
    kind: ChainTransportFailureKind,
    redacted_message: String,
}

impl ChainTransportError {
    /// Reports a failure known to have happened before dispatch.
    pub fn definitely_unsent(redacted_message: impl AsRef<str>) -> Self {
        Self::new(
            ChainTransportFailureKind::DefinitelyUnsent,
            redacted_message,
        )
    }

    /// Reports a failure after dispatch could no longer be excluded.
    pub fn possibly_dispatched(redacted_message: impl AsRef<str>) -> Self {
        Self::new(
            ChainTransportFailureKind::PossiblyDispatched,
            redacted_message,
        )
    }

    fn new(kind: ChainTransportFailureKind, redacted_message: impl AsRef<str>) -> Self {
        Self {
            kind,
            redacted_message: bounded_redacted_text(redacted_message.as_ref()),
        }
    }

    pub fn kind(&self) -> ChainTransportFailureKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.redacted_message
    }

    pub fn is_definitely_unsent(&self) -> bool {
        self.kind == ChainTransportFailureKind::DefinitelyUnsent
    }
}

impl std::fmt::Display for ChainTransportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.redacted_message)
    }
}

impl std::error::Error for ChainTransportError {}

fn bounded_redacted_text(message: &str) -> String {
    let mut bounded =
        String::with_capacity(message.len().min(MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES));
    for character in message.chars() {
        let escaped_length = character
            .escape_default()
            .map(char::len_utf8)
            .sum::<usize>();
        if bounded.len() + escaped_length > MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES {
            break;
        }
        bounded.extend(character.escape_default());
    }
    bounded
}

/// Complete SDK-authored metadata for one vote-chain request.
///
/// The transport must use the supplied URL and headers exactly, apply the
/// timeout to connection setup through complete body read, and stop buffering
/// after `max_response_bytes`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainHttpRequest {
    url: String,
    headers: Vec<(String, String)>,
    timeout: Duration,
    max_response_bytes: usize,
}

impl ChainHttpRequest {
    pub(crate) fn new(
        url: String,
        headers: Vec<(String, String)>,
        timeout: Duration,
        max_response_bytes: usize,
    ) -> Self {
        Self {
            url,
            headers,
            timeout,
            max_response_bytes,
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn max_response_bytes(&self) -> usize {
        self.max_response_bytes
    }
}

/// Bounded HTTP response returned to the chain protocol layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainHttpResponse {
    status: u16,
    body: Vec<u8>,
    content_type: Option<String>,
    headers: Vec<(String, String)>,
}

impl ChainHttpResponse {
    /// Constructs a response for a host transport implementation.
    pub fn new(
        status: u16,
        body: Vec<u8>,
        content_type: Option<String>,
        headers: Vec<(String, String)>,
    ) -> Self {
        Self {
            status,
            body,
            content_type,
            headers,
        }
    }

    /// Constructs an `application/json` response with no additional headers.
    pub fn json(status: u16, body: Vec<u8>) -> Self {
        Self::new(
            status,
            body,
            Some("application/json".to_string()),
            Vec::new(),
        )
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }
}

/// Boxed future returned by [`ChainTransport`].
pub type ChainTransportFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ChainHttpResponse, ChainTransportError>> + Send + 'a>>;

/// Shared marker for the point at which a POST may reach the network.
///
/// A transport marks this immediately before handing the request to a network
/// stack capable of delivering it. Cancellation before the marker is set is
/// definitely unsent; cancellation after it is conservatively ambiguous.
#[derive(Clone, Debug, Default)]
pub struct ChainPostDispatch {
    possible: Arc<AtomicBool>,
}

impl ChainPostDispatch {
    /// Marks that the POST may be delivered even if its future is cancelled.
    pub fn mark_possible(&self) {
        self.possible.store(true, Ordering::Release);
    }

    /// Returns whether the POST crossed the transport handoff boundary.
    pub fn is_possible(&self) -> bool {
        self.possible.load(Ordering::Acquire)
    }
}

/// Host-supplied HTTP mechanism for vote-chain requests.
///
/// A privacy-routed implementation must fail closed when its route is
/// unavailable. It must never fall back to a direct connection.
pub trait ChainTransport: Send + Sync {
    /// Performs one vote-chain lookup request.
    ///
    /// The chain-specific name avoids colliding with other host transport
    /// traits when wallet integrations import the crate prelude.
    fn chain_get<'a>(&'a self, request: ChainHttpRequest) -> ChainTransportFuture<'a>;

    /// Performs one vote-chain mutation request with a canonical JSON body.
    fn chain_post_json<'a>(
        &'a self,
        request: ChainHttpRequest,
        json: Vec<u8>,
    ) -> ChainTransportFuture<'a>;

    /// Performs a mutation request and reports its dispatch boundary.
    ///
    /// Existing transports inherit a conservative implementation that marks
    /// dispatch as soon as their POST future is polled. Implementations that
    /// own a more precise handoff point should override this method and mark it
    /// immediately before releasing the request to their network stack.
    ///
    /// The marker must be set from within the returned future's own poll, not
    /// from a detached task: the SDK enforces its POST deadline around this
    /// future and reads the marker after dropping it, classifying a timeout
    /// with a clear marker as definitely unsent. A marker set later would
    /// turn a request that did reach the network into a silent redispatch.
    fn chain_post_json_with_dispatch<'a>(
        &'a self,
        request: ChainHttpRequest,
        json: Vec<u8>,
        dispatch: ChainPostDispatch,
    ) -> ChainTransportFuture<'a> {
        Box::pin(async move {
            dispatch.mark_possible();
            self.chain_post_json(request, json).await
        })
    }
}

impl<T: ChainTransport + ?Sized> ChainTransport for std::sync::Arc<T> {
    fn chain_get<'a>(&'a self, request: ChainHttpRequest) -> ChainTransportFuture<'a> {
        (**self).chain_get(request)
    }

    fn chain_post_json<'a>(
        &'a self,
        request: ChainHttpRequest,
        json: Vec<u8>,
    ) -> ChainTransportFuture<'a> {
        (**self).chain_post_json(request, json)
    }

    fn chain_post_json_with_dispatch<'a>(
        &'a self,
        request: ChainHttpRequest,
        json: Vec<u8>,
        dispatch: ChainPostDispatch,
    ) -> ChainTransportFuture<'a> {
        (**self).chain_post_json_with_dispatch(request, json, dispatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_diagnostics_are_escaped_and_bounded() {
        let error = ChainTransportError::possibly_dispatched(format!(
            "private response\n{}",
            "x".repeat(MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES)
        ));

        assert_eq!(error.kind(), ChainTransportFailureKind::PossiblyDispatched);
        assert!(!error.message().contains('\n'));
        assert!(error.message().contains("\\n"));
        assert!(error.message().len() <= MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES);
    }
}
