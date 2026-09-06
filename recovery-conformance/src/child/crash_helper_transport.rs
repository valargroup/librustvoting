//! The helper transport that dies mid-share-POST.
//!
//! The two share crash points sit either side of one helper POST, and neither
//! is announced by the driver: `share_tracking` journals the helper, sends, and
//! resolves the outcome inside a single step. Only the transport sees the gap.
//!
//! That gap is the whole reason `attempting_urls` exists. A helper is written
//! there before any byte is sent, so a process killed in between leaves durable
//! evidence that this helper was tried and its answer is unknown — which is
//! what stops recovery from either re-sending blindly or writing the attempt
//! off as never made.

use std::sync::Arc;
use std::time::Duration;

use zcash_voting::helper::transport::{HelperFuture, HelperTransport};

use super::crash::{crash_now, CrashLog, Observation};
use crate::stages::CrashStage;

/// Wraps a real helper transport and kills the process around a share POST.
///
/// GETs pass through: helper status polling changes nothing durable.
pub struct CrashHelperTransport<T> {
    inner: T,
    armed: Option<CrashStage>,
    log: Arc<CrashLog>,
}

impl<T> CrashHelperTransport<T> {
    /// Wraps `inner`, arming only for the two share-POST stages.
    pub fn new(inner: T, stage: Option<CrashStage>, log: Arc<CrashLog>) -> Self {
        let armed = stage.filter(|stage| {
            matches!(
                stage,
                CrashStage::BeforeSharePost | CrashStage::AfterSharePost
            )
        });
        Self { inner, armed, log }
    }
}

impl<T: HelperTransport> HelperTransport for CrashHelperTransport<T> {
    fn get<'a>(&'a self, url: &'a str, timeout: Duration) -> HelperFuture<'a> {
        self.inner.get(url, timeout)
    }

    fn post_json<'a>(&'a self, url: &'a str, body: Vec<u8>, timeout: Duration) -> HelperFuture<'a> {
        let Some(armed) = self.armed else {
            return self.inner.post_json(url, body, timeout);
        };
        let log = Arc::clone(&self.log);
        let recorded = url.to_string();

        Box::pin(async move {
            // The share is already journaled in `attempting_urls`: journalling
            // happens before dispatch, so reaching the transport at all means
            // the durable marker exists. Dying here is the untried-helper case
            // that must still be treated as outcome-unknown.
            if armed == CrashStage::BeforeSharePost {
                crash_now(&log, armed);
            }

            log.record(&Observation::PostDispatched {
                url: recorded.clone(),
            });
            let response = self.inner.post_json(url, body, timeout).await;

            if let Ok(response) = &response {
                log.record(&Observation::PostResponse {
                    url: recorded,
                    status: response.status(),
                    body: response.body_text(),
                });
            }

            // The helper answered and the wallet never wrote the outcome down.
            // Indistinguishable, from the sidecar, from a POST that never
            // returned — which is precisely why resume must treat it as
            // ambiguous rather than as a failure it can retry freely.
            crash_now(&log, armed);
        })
    }
}
