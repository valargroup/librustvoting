//! The route executor that stops answering.
//!
//! Wraps the real executor and, for one named request class, returns a future
//! that never resolves. Nothing else about the request changes: it is built by
//! the SDK, carries the SDK's own deadline, and every request outside the armed
//! class is delegated untouched.
//!
//! Below the SDK's deadlines on purpose. `RouteHttp` is the lowest HTTP seam
//! the SDK exposes, and every bound the suite is trying to check — the chain
//! post timeout, the helper fan-out budget, the PIR request budget, the tree
//! request timeout — is applied *above* it. A stall injected here is therefore
//! a stall the SDK is supposed to end, and a run that never ends is the finding.

use std::sync::Arc;

use zcash_voting::{RouteFuture, RouteHttp, RouteRequest};

use crate::child::{CrashLog, Observation};
use crate::stall::{RequestClassifier, StallPlan, StallPoint};

/// Wraps a real route executor and hangs one class of request.
///
/// An empty plan makes this a pass-through, which is what keeps a control run
/// and a stalled run on the same code path rather than on two implementations
/// whose agreement would prove nothing.
pub struct StallingRoute<R> {
    inner: R,
    plan: StallPlan,
    classifier: RequestClassifier,
    log: Arc<CrashLog>,
}

impl<R> StallingRoute<R> {
    /// Wraps `inner`, arming it for whatever `plan` names.
    pub fn new(inner: R, plan: StallPlan, classifier: RequestClassifier, log: Arc<CrashLog>) -> Self {
        Self {
            inner,
            plan,
            classifier,
            log,
        }
    }
}

impl<R: RouteHttp> RouteHttp for StallingRoute<R> {
    fn execute<'a>(
        &'a self,
        request: RouteRequest<'a>,
        on_dispatch: &'a (dyn Fn() + Send + Sync),
    ) -> RouteFuture<'a> {
        let Some(target) = self.plan.armed_target() else {
            return self.inner.execute(request, on_dispatch);
        };
        let classified = self
            .classifier
            .classify(request.method.as_str(), request.url);
        if classified != Some(target) {
            return self.inner.execute(request, on_dispatch);
        }

        let log = Arc::clone(&self.log);
        let point = self.plan.point;
        let url = request.url.to_string();
        // Recorded before the future is even polled, and fsynced, because the
        // process that is about to hang may be killed by its budget rather than
        // returning: the evidence that the stall fired has to be on disk before
        // the hanging begins.
        log.record(&Observation::RequestStalled {
            target: target.name().to_string(),
            url,
            point_after_dispatch: point == StallPoint::AfterDispatch,
            timeout_milliseconds: request.timeout.as_millis() as u64,
        });

        Box::pin(async move {
            if point == StallPoint::AfterDispatch {
                // The hook is the SDK's whole basis for calling a failure
                // possibly-delivered. Calling it and then never answering is
                // exactly the state a real half-open connection leaves.
                on_dispatch();
            }
            // Never resolves. Not a long sleep: a sleep has a duration the SDK
            // might outlast by accident, and the claim under test is that the
            // SDK ends this itself.
            std::future::pending::<()>().await;
            unreachable!("a pending future does not complete")
        })
    }

    /// Reported from the wrapped executor.
    ///
    /// Both of these describe how the *real* executor behaves, and a stalled
    /// request is still executed by it for every class this run did not arm.
    /// Answering for ourselves would change how the SDK classifies traffic that
    /// has nothing to do with the stall.
    fn hook_precedes_connection_setup(&self) -> bool {
        self.inner.hook_precedes_connection_setup()
    }

    fn enforces_connect_timeout(&self) -> bool {
        self.inner.enforces_connect_timeout()
    }
}
