//! The route executor that decides which helpers are reachable.
//!
//! Sits between the stall wrapper and the real executor and looks at one thing:
//! whether the URL names a synthetic helper. A request to any other host is
//! delegated untouched, so chain, PIR, and tree traffic pass through as if this
//! wrapper were not there.
//!
//! Rewriting rather than proxying is deliberate. A local proxy would need a
//! port, a certificate the wallet would refuse, and a second HTTP
//! implementation between the SDK and the helper; substituting the host inside
//! the request leaves TLS, SNI, and the response entirely the backend's own.
//! What the wallet writes down is still the synthetic URL, and that is the
//! identity every placement and recovery decision is made against.

use std::sync::Arc;

use zcash_voting::{RouteError, RouteFuture, RouteHttp, RouteRequest};

use crate::child::{CrashLog, Observation};
use crate::helper_fleet::{HelperAvailability, HelperFleetPlan};

/// Path a share submission is POSTed to.
///
/// Only share delivery is recorded. Readiness probes and status polls run to
/// every helper on every pass, and journalling each one would bury the handful
/// of records an assertion actually reads under thousands of fsynced lines —
/// while telling it nothing: placement is decided by where a share was *sent*.
const SHARES_ENDPOINT: &str = "/shielded-vote/v1/shares";

/// Wraps a real route executor and applies a helper fleet's availability.
pub struct HelperFleetRoute<R> {
    inner: R,
    plan: HelperFleetPlan,
    log: Arc<CrashLog>,
}

impl<R> HelperFleetRoute<R> {
    /// Wraps `inner`. An empty plan makes this a pass-through.
    pub fn new(inner: R, plan: HelperFleetPlan, log: Arc<CrashLog>) -> Self {
        Self { inner, plan, log }
    }
}

impl<R: RouteHttp> RouteHttp for HelperFleetRoute<R> {
    fn execute<'a>(
        &'a self,
        request: RouteRequest<'a>,
        on_dispatch: &'a (dyn Fn() + Send + Sync),
    ) -> RouteFuture<'a> {
        let Some((base, availability)) = self.plan.resolve(request.url) else {
            return self.inner.execute(request, on_dispatch);
        };
        let helper = base.to_string();
        let delivering = request.url.ends_with(SHARES_ENDPOINT);

        match availability {
            HelperAvailability::Refuses => {
                // Before dispatch, and truthfully so: nothing was built, let
                // alone sent. The SDK is entitled to treat this as a definite
                // failure and try another helper with no ambiguity to carry,
                // which is exactly what a refused connection means.
                if delivering {
                    self.log
                        .record(&Observation::HelperRefused { url: helper.clone() });
                }
                Box::pin(async move {
                    Err(RouteError::before_dispatch(format!(
                        "connection refused by {helper}"
                    )))
                })
            }
            HelperAvailability::NeverAnswers => {
                if delivering {
                    self.log
                        .record(&Observation::HelperUnanswered { url: helper });
                }
                Box::pin(async move {
                    // The hook fires first: a helper that accepted the
                    // connection and went quiet may well have processed the
                    // request, and the wallet must not be told otherwise.
                    on_dispatch();
                    std::future::pending::<()>().await;
                    unreachable!("a pending future does not complete")
                })
            }
            HelperAvailability::Answers => {
                let Some(routed) = self.plan.route_to_backend(request.url) else {
                    return self.inner.execute(request, on_dispatch);
                };
                let log = Arc::clone(&self.log);
                let RouteRequest {
                    method,
                    url: _,
                    headers,
                    body,
                    timeout,
                    connect_timeout,
                    max_response_bytes,
                } = request;
                Box::pin(async move {
                    let response = self
                        .inner
                        .execute(
                            RouteRequest {
                                method,
                                url: &routed,
                                headers,
                                body,
                                timeout,
                                connect_timeout,
                                max_response_bytes,
                            },
                            on_dispatch,
                        )
                        .await;
                    // Recorded against the synthetic URL, not the backend that
                    // served it. An assertion about re-sending to a helper that
                    // already accepted is an assertion about the identity the
                    // wallet holds, and every synthetic URL shares one backend.
                    if delivering {
                        if let Ok(response) = &response {
                            log.record(&Observation::HelperPost {
                                url: helper,
                                status: response.status,
                            });
                        }
                    }
                    response
                })
            }
        }
    }

    fn hook_precedes_connection_setup(&self) -> bool {
        self.inner.hook_precedes_connection_setup()
    }

    fn enforces_connect_timeout(&self) -> bool {
        self.inner.enforces_connect_timeout()
    }
}
