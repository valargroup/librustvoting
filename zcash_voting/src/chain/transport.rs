//! Host-owned transport seam for vote-chain HTTP requests.

use std::{future::Future, pin::Pin, time::Duration};

pub use crate::helper::transport::{
    HelperResponse as ChainResponse, RequestTransportError as ChainTransportError,
};

/// Largest vote-chain response accepted by the SDK.
pub const MAX_CHAIN_RESPONSE_BYTES: usize = 256 * 1024;

pub type ChainFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ChainResponse, ChainTransportError>> + Send + 'a>>;

/// HTTP transport supplied by the host wallet.
///
/// A routed implementation must fail closed when its selected route is
/// unavailable. `timeout` covers connection setup through complete body read.
pub trait ChainTransport: Send + Sync {
    fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> ChainFuture<'a>;

    fn post_json<'a>(&'a self, url: &'a str, body: Vec<u8>, timeout: Duration) -> ChainFuture<'a>;
}

impl<T: ChainTransport + ?Sized> ChainTransport for std::sync::Arc<T> {
    fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> ChainFuture<'a> {
        (**self).get(url, timeout)
    }

    fn post_json<'a>(&'a self, url: &'a str, body: Vec<u8>, timeout: Duration) -> ChainFuture<'a> {
        (**self).post_json(url, body, timeout)
    }
}
