//! HTTP client for submitting MsgRevealShare to the chain (or mock endpoint).

use crate::types::{BroadcastResult, MsgRevealShareJson};

/// Client for submitting MsgRevealShare transactions.
#[derive(Clone)]
pub struct ChainClient {
    client: reqwest::Client,
    submit_url: String,
}

impl ChainClient {
    pub fn new(submit_url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            submit_url,
        }
    }

    /// Submit a MsgRevealShare to the chain endpoint.
    ///
    /// The chain's REST API at `POST /zally/v1/reveal-share` accepts JSON
    /// (encoding/json format with base64-encoded byte fields) and returns
    /// a `BroadcastResult`.
    pub async fn submit_reveal_share(
        &self,
        msg: &MsgRevealShareJson,
    ) -> Result<BroadcastResult, String> {
        let url = format!("{}/zally/v1/reveal-share", self.submit_url);
        let resp = self
            .client
            .post(&url)
            .json(msg)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("chain returned {}: {}", status, body));
        }

        resp.json::<BroadcastResult>()
            .await
            .map_err(|e| format!("parse response: {}", e))
    }
}
