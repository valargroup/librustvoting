//! Request-local diagnostics. Never change dispatch, deadlines, or retry policy.
use crate::observability::ObservationStage;
use crate::{ObservationOutcome, ObservationScope};
use std::{
    future::{poll_fn, Future},
    sync::{Arc, Mutex},
    task::{Context, Wake, Waker},
};

tokio::task_local! {
    static HTTP_SCOPE: ObservationScope;
}

pub(super) fn scope() -> ObservationScope {
    HTTP_SCOPE
        .try_with(Clone::clone)
        .unwrap_or_else(|_| ObservationScope::disabled())
}

struct RequestWake {
    parent: Mutex<Waker>,
    observations: ObservationScope,
    ready: Mutex<Option<ObservationStage>>,
}

impl Wake for RequestWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        {
            let mut ready = self.ready.lock().unwrap_or_else(|p| p.into_inner());
            if ready.is_none() {
                *ready = Some(self.observations.stage("helper.http.wake_to_poll"));
            }
        }
        let parent = self
            .parent
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        parent.wake_by_ref();
    }
}

/// Keep attribution attached while polling nested route futures. Measure both
/// I/O suspension and executor delay after a wake, without spawning any task.
pub(crate) async fn observe_helper_http<F: Future>(
    observations: ObservationScope,
    future: F,
) -> F::Output {
    if !observations.is_enabled() {
        return future.await;
    }
    let mut future = std::pin::pin!(future);
    let mut previous: Option<Arc<RequestWake>> = None;
    let mut suspended: Option<ObservationStage> = None;
    HTTP_SCOPE
        .scope(
            observations.clone(),
            poll_fn(move |context| {
                if let Some(previous) = previous.as_ref() {
                    if let Some(ready) = previous
                        .ready
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .take()
                    {
                        ready.finish(ObservationOutcome::Succeeded, None);
                    }
                }
                if let Some(suspended) = suspended.take() {
                    suspended.finish(ObservationOutcome::Succeeded, None);
                }
                let wake = previous
                    .get_or_insert_with(|| {
                        Arc::new(RequestWake {
                            parent: Mutex::new(context.waker().clone()),
                            observations: observations.clone(),
                            ready: Mutex::new(None),
                        })
                    })
                    .clone();
                *wake.parent.lock().unwrap_or_else(|p| p.into_inner()) = context.waker().clone();
                let waker = Waker::from(wake.clone());
                let poll = observations.stage("helper.http.poll");
                let outcome = future.as_mut().poll(&mut Context::from_waker(&waker));
                poll.finish(ObservationOutcome::Succeeded, None);
                if outcome.is_pending() {
                    suspended = Some(observations.stage("helper.http.between_polls"));
                }
                outcome
            }),
        )
        .await
}

/// Closed phase vocabulary for host-supplied HTTP routes; no URLs or payloads.
#[doc(hidden)]
#[derive(Clone, Copy)]
pub enum HttpObservationPhase {
    /// Waiting for the wallet's configured network route.
    RouteSelection,
    /// Complete request through the direct network route.
    DirectRequest,
    /// Complete request through the Tor route.
    TorRequest,
    /// Collecting a Tor response body after headers arrived.
    TorBody,
}

/// Request-local collector captured before crossing a host route callback.
/// Disabled outside an observed helper POST. It cannot finalize the report.
#[doc(hidden)]
#[derive(Clone)]
pub struct HttpObservationContext(ObservationScope);

impl HttpObservationContext {
    /// Capture the current helper identity without reading request data.
    pub fn capture() -> Self {
        Self(scope())
    }

    /// Measure a route phase while preserving its exact result and cancellation.
    pub async fn observe<F, T, E>(&self, phase: HttpObservationPhase, future: F) -> Result<T, E>
    where
        F: Future<Output = Result<T, E>>,
    {
        let name = match phase {
            HttpObservationPhase::RouteSelection => "helper.http.route_selection",
            HttpObservationPhase::DirectRequest => "helper.http.direct_request",
            HttpObservationPhase::TorRequest => "helper.http.tor_request",
            HttpObservationPhase::TorBody => "helper.http.tor_body",
        };
        let stage = self.0.stage(name);
        let result = future.await;
        stage.finish(
            if result.is_ok() {
                ObservationOutcome::Succeeded
            } else {
                ObservationOutcome::Failed
            },
            None,
        );
        result
    }
}
