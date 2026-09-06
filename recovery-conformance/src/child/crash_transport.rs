//! The chain transport that dies mid-POST.
//!
//! The broadcast boundary is the one crash point no event stream can express.
//! It lives inside a single transport call, between the instruction that
//! releases the request and the one that records what came back, and the SDK
//! draws its whole ambiguity model across it: everything before
//! `ChainPostDispatch::mark_possible` is definitely unsent, everything after is
//! possibly dispatched. Wrapping the real transport is therefore the only
//! honest way to test it — the wrapper delegates to the same HTTP client the
//! wallet would use, so the transaction that reaches staging is a real one.

use std::sync::Arc;

use zcash_voting::{ChainHttpRequest, ChainPostDispatch, ChainTransport, ChainTransportFuture};

use super::crash::{crash_now, CrashLog, Observation};
use crate::stages::{BroadcastPoint, CrashStage, CrashTrigger, SubmissionKind};

/// Wraps a real chain transport and kills the process at a broadcast point.
///
/// Only POSTs are intercepted. GETs — status polls and tree scans — pass
/// straight through, because a crash during a read changes no durable state
/// and would only test the harness.
pub struct CrashTransport<T> {
    inner: T,
    /// `None` for a run that should not crash in the transport, which is every
    /// run whose stage is event-triggered, plus the uncrashed control run.
    armed: Option<ArmedBroadcast>,
    log: Arc<CrashLog>,
}

#[derive(Clone, Copy)]
struct ArmedBroadcast {
    stage: CrashStage,
    submission: SubmissionKind,
    point: BroadcastPoint,
}

impl<T> CrashTransport<T> {
    /// Wraps `inner`, arming it only if `stage` is broadcast-triggered.
    pub fn new(inner: T, stage: Option<CrashStage>, log: Arc<CrashLog>) -> Self {
        let armed = stage.and_then(|stage| match stage.trigger() {
            CrashTrigger::Broadcast { submission, point } => Some(ArmedBroadcast {
                stage,
                submission,
                point,
            }),
            CrashTrigger::Event => None,
        });
        Self { inner, armed, log }
    }

    /// The armed point, if this POST is the one to die on.
    ///
    /// A round POSTs delegations and votes through one transport, so an armed
    /// stage that ignored the endpoint would fire on whichever submission
    /// happened to come first — which for a multi-bundle round is a different
    /// bundle on every run.
    fn armed_for(&self, url: &str) -> Option<ArmedBroadcast> {
        let armed = self.armed?;
        (submission_kind(url)? == armed.submission).then_some(armed)
    }
}

/// Which submission a POST URL addresses.
///
/// Matching the final path segment rather than a substring keeps `cast-vote`
/// from also claiming `cast-vote-batch`.
fn submission_kind(url: &str) -> Option<SubmissionKind> {
    let endpoint = url.rsplit('/').next()?;
    match endpoint {
        "delegate-vote" => Some(SubmissionKind::Delegation),
        "cast-vote" | "cast-vote-batch" => Some(SubmissionKind::Vote),
        _ => None,
    }
}

impl<T: ChainTransport> ChainTransport for CrashTransport<T> {
    fn chain_get<'a>(&'a self, request: ChainHttpRequest) -> ChainTransportFuture<'a> {
        self.inner.chain_get(request)
    }

    fn chain_post_json<'a>(
        &'a self,
        request: ChainHttpRequest,
        json: Vec<u8>,
    ) -> ChainTransportFuture<'a> {
        self.chain_post_json_with_dispatch(request, json, ChainPostDispatch::default())
    }

    fn chain_post_json_with_dispatch<'a>(
        &'a self,
        request: ChainHttpRequest,
        json: Vec<u8>,
        dispatch: ChainPostDispatch,
    ) -> ChainTransportFuture<'a> {
        let Some(armed) = self.armed_for(request.url()) else {
            return self
                .inner
                .chain_post_json_with_dispatch(request, json, dispatch);
        };
        let log = Arc::clone(&self.log);
        let url = request.url().to_string();

        Box::pin(async move {
            // Nothing has been released yet, and `dispatch` is still clear, so
            // the SDK would class this as definitely unsent had it observed
            // it. It never does: the process dies holding a `Submitting` row
            // it cannot later prove was never sent.
            if armed.point == BroadcastPoint::BeforeDispatch {
                crash_now(&log, armed.stage);
            }

            log.record(&Observation::PostDispatched { url: url.clone() });

            // Let the real POST complete so the transaction genuinely reaches
            // staging. Only then is the durable state interesting: the chain
            // holds a transaction the wallet has no hash for.
            let response = self
                .inner
                .chain_post_json_with_dispatch(request, json, dispatch)
                .await;

            if armed.point == BroadcastPoint::AfterResponse {
                if let Ok(response) = &response {
                    log.record(&Observation::PostResponse {
                        url,
                        status: response.status(),
                        body: String::from_utf8_lossy(response.body()).into_owned(),
                    });
                }
            }

            // `AfterDispatch` and `AfterResponse` leave identical durable
            // state — an unclassified POST either way. They differ only in
            // what the parent is told: `AfterResponse` hands it the real
            // transaction hash, which is what lets a test check the chain for
            // a second spend by identity rather than by counting.
            crash_now(&log, armed.stage);
        })
    }
}
