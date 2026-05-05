use anyhow::{Context, Result};
use bytes::Bytes;
use http::{Method, Request};
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};

use crate::{Transport, TransportFuture, TransportResponse};

type RequestBody = Full<Bytes>;
type HyperClient = Client<HttpsConnector<HttpConnector>, RequestBody>;

/// Direct HTTP transport for PIR requests.
///
/// `zcash_voting` keeps PIR fetching behind the small `Transport` trait, but
/// the default `client-pir` feature also includes this Hyper/Rustls adapter for
/// consumers that want direct HTTPS without providing their own transport.
pub struct HyperTransport {
    client: HyperClient,
}

impl HyperTransport {
    pub fn new() -> Self {
        let mut connector = HttpConnector::new();
        connector.enforce_http(false);
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(connector);
        let client = Client::builder(TokioExecutor::new()).build(https);

        Self { client }
    }

    async fn request(&self, method: Method, url: &str, body: Vec<u8>) -> Result<TransportResponse> {
        let request = Request::builder()
            .method(method)
            .uri(url)
            .body(Full::new(Bytes::from(body)))
            .context("build PIR HTTP request")?;
        let response = self
            .client
            .request(request)
            .await
            .context("send PIR HTTP request")?;
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
        let body = response
            .into_body()
            .collect()
            .await
            .context("read PIR HTTP response body")?
            .to_bytes()
            .to_vec();

        Ok(TransportResponse {
            status,
            headers,
            body,
        })
    }
}

impl Default for HyperTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for HyperTransport {
    fn get<'a>(&'a self, url: &'a str) -> TransportFuture<'a> {
        Box::pin(async move { self.request(Method::GET, url, Vec::new()).await })
    }

    fn post<'a>(&'a self, url: &'a str, body: Vec<u8>) -> TransportFuture<'a> {
        Box::pin(async move { self.request(Method::POST, url, body).await })
    }
}
