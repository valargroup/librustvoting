//! One PIR request splits a single budget across two bounded attempts.
//!
//! A PIR fetch that works completes in a few seconds; one against a dead
//! endpoint stalls until a deadline expires. A wall clock cannot tell that
//! apart from a link that is merely slow, so the budget is bounded on two axes
//! instead: connection setup tightly, where a dead endpoint reveals itself, and
//! the request as a whole generously, where a slow transfer needs room.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::super::{
    connect_deadline, HyperTransport, RouteError, RouteFuture, RouteHttp, RouteRequest,
    RouteResponse, DIRECT_CONNECT_DEADLINE_LEAD, PIR_FINAL_ATTEMPT_CONNECT,
    PIR_FIRST_ATTEMPT_CONNECT, PIR_FIRST_ATTEMPT_OVERALL, PIR_REQUEST_BUDGET,
};

/// What every attempt observed, and the scripted outcome it was answered with.
struct RecordingRoute {
    attempts: Mutex<Vec<Attempt>>,
    /// Outcomes in order; the last one repeats once exhausted.
    outcomes: Mutex<Vec<Outcome>>,
    /// Whether this route claims to bound connection setup by the request's
    /// `connect_timeout`, which is what lets the SDK read a late pre-dispatch
    /// failure as that budget expiring.
    enforces_connect_timeout: bool,
}

/// The deadline and body one attempt carried.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Attempt {
    timeout: Duration,
    connect_timeout: Option<Duration>,
    body: Vec<u8>,
}

#[derive(Clone, Copy)]
enum Outcome {
    /// The request hangs past any deadline, as a stalled endpoint does, so the
    /// caller's own backstop is what ends the attempt.
    Stall,
    Status(u16),
    /// A connection refused outright, before the connect deadline could
    /// expire. A definite answer about this endpoint.
    Refused,
    /// The same definite refusal, reported only after the connect budget would
    /// have expired — a slow resolver, or a proxy that takes its time saying
    /// no. Indistinguishable from a connect timeout by the clock alone.
    RefusedLate,
    /// Connection setup that ran out of time: no request byte left, but the
    /// endpoint never answered either. Not an answer, so it may be repeated.
    ConnectTimedOut,
}

impl RecordingRoute {
    fn new(outcomes: Vec<Outcome>) -> Arc<Self> {
        Arc::new(Self {
            attempts: Mutex::new(Vec::new()),
            outcomes: Mutex::new(outcomes),
            enforces_connect_timeout: true,
        })
    }

    /// A route that ignores `connect_timeout`, as a custom host executor that
    /// never opted in does.
    fn ignoring_connect_timeout(outcomes: Vec<Outcome>) -> Arc<Self> {
        Arc::new(Self {
            attempts: Mutex::new(Vec::new()),
            outcomes: Mutex::new(outcomes),
            enforces_connect_timeout: false,
        })
    }

    fn deadlines(&self) -> Vec<Duration> {
        self.attempts
            .lock()
            .unwrap()
            .iter()
            .map(|attempt| attempt.timeout)
            .collect()
    }

    fn connect_timeouts(&self) -> Vec<Option<Duration>> {
        self.attempts
            .lock()
            .unwrap()
            .iter()
            .map(|attempt| attempt.connect_timeout)
            .collect()
    }

    fn bodies(&self) -> Vec<Vec<u8>> {
        self.attempts
            .lock()
            .unwrap()
            .iter()
            .map(|attempt| attempt.body.clone())
            .collect()
    }
}

impl RouteHttp for RecordingRoute {
    fn enforces_connect_timeout(&self) -> bool {
        self.enforces_connect_timeout
    }

    fn execute<'a>(
        &'a self,
        request: RouteRequest<'a>,
        on_dispatch: &'a (dyn Fn() + Send + Sync),
    ) -> RouteFuture<'a> {
        self.attempts.lock().unwrap().push(Attempt {
            timeout: request.timeout,
            connect_timeout: request.connect_timeout,
            body: request.body,
        });
        let mut outcomes = self.outcomes.lock().unwrap();
        let outcome = if outcomes.len() > 1 {
            outcomes.remove(0)
        } else {
            outcomes.first().copied().unwrap_or(Outcome::Stall)
        };
        drop(outcomes);
        // A refusal and a connect timeout both happen before any byte leaves,
        // so neither marks dispatch. The other two reach the endpoint.
        if matches!(outcome, Outcome::Stall | Outcome::Status(_)) {
            on_dispatch();
        }
        Box::pin(async move {
            match outcome {
                Outcome::Stall => {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    unreachable!("the caller's deadline ends this attempt")
                }
                Outcome::Status(status) => Ok(RouteResponse {
                    status,
                    headers: Vec::new(),
                    body: b"{}".to_vec(),
                }),
                Outcome::Refused => Err(RouteError::before_dispatch("connection refused")),
                // A definite refusal that simply took a while to come back,
                // from an endpoint that resolved slowly or sits behind a
                // proxy. It arrives after the connect budget would have
                // expired, so only the route's own claim to enforce that
                // budget can tell it apart from one.
                Outcome::RefusedLate => {
                    tokio::time::sleep(PIR_FIRST_ATTEMPT_CONNECT + Duration::from_secs(2)).await;
                    Err(RouteError::before_dispatch("connection refused"))
                }
                // Sleep past the connect budget but stay inside the backstop,
                // so the transport sees a pre-dispatch failure at a point only
                // the connect deadline can explain. The real connector reports
                // exactly this, via Hyper's distinct connect error.
                Outcome::ConnectTimedOut => {
                    tokio::time::sleep(PIR_FIRST_ATTEMPT_CONNECT + Duration::from_secs(2)).await;
                    Err(RouteError::before_dispatch(
                        "send HTTP request: connection setup timed out before request dispatch",
                    ))
                }
            }
        })
    }
}

fn transport(outcomes: Vec<Outcome>) -> (Arc<RecordingRoute>, HyperTransport<RecordingRoute>) {
    let route = RecordingRoute::new(outcomes);
    let transport = HyperTransport::with_shared_route(Arc::clone(&route));
    (route, transport)
}

#[tokio::test(start_paused = true)]
async fn a_stalled_attempt_is_retried_with_the_remaining_budget() {
    let (route, transport) = transport(vec![Outcome::Stall, Outcome::Status(200)]);

    let result = pir_client::Transport::get(&transport, "http://pir.invalid/root").await;

    assert!(result.is_ok(), "the second attempt answered");
    assert_eq!(
        route.deadlines(),
        vec![PIR_FIRST_ATTEMPT_OVERALL, Duration::from_secs(45)],
        "the first attempt is bounded tightly; the second gets everything left \
         of the budget, so a link that is slow rather than dead still finishes"
    );
}

#[tokio::test(start_paused = true)]
async fn a_stalled_connect_is_retried() {
    // The case a whole-request deadline cannot see: connection setup ran out
    // of time, which is reported as a definite pre-dispatch failure and would
    // otherwise end the request on its first attempt.
    let (route, transport) = transport(vec![Outcome::ConnectTimedOut, Outcome::Status(200)]);

    let result = pir_client::Transport::get(&transport, "http://pir.invalid/root").await;

    assert!(result.is_ok(), "the second attempt answered");
    assert_eq!(
        route.deadlines().len(),
        2,
        "the stalled connect was retried"
    );
}

#[tokio::test(start_paused = true)]
async fn the_connect_budget_reaches_the_executor() {
    // The budget is only useful if the executor that owns connection setup can
    // see it. The SDK cannot enforce it from outside: it never observes the
    // connection being established.
    let (route, transport) = transport(vec![Outcome::Stall, Outcome::Status(200)]);

    let _ = pir_client::Transport::get(&transport, "http://pir.invalid/root").await;

    assert_eq!(
        route.connect_timeouts(),
        vec![
            Some(PIR_FIRST_ATTEMPT_CONNECT),
            Some(PIR_FINAL_ATTEMPT_CONNECT)
        ],
        "each attempt carries its own connect budget"
    );
}

#[tokio::test(start_paused = true)]
async fn a_late_refusal_from_a_route_that_ignores_the_budget_is_not_retried() {
    // An executor that never opted into the budget had no deadline to expire,
    // so a refusal it happens to report late is exactly what it says it is.
    // Reading the clock alone would manufacture a timeout out of a definite
    // answer and spend a second request discovering the same thing.
    let route = RecordingRoute::ignoring_connect_timeout(vec![Outcome::RefusedLate]);
    let transport = HyperTransport::with_shared_route(Arc::clone(&route));

    let result = pir_client::Transport::get(&transport, "http://pir.invalid/root").await;

    assert!(result.is_err());
    assert_eq!(
        route.deadlines().len(),
        1,
        "a definite refusal is not reinterpreted as a connect timeout"
    );
}

#[tokio::test(start_paused = true)]
async fn a_late_refusal_from_a_route_that_enforces_the_budget_is_a_connect_timeout() {
    // The other side of the same coin: an executor that does bound connection
    // setup can only be reporting the budget expiring at this point, so the
    // request is repeated rather than abandoned.
    let (route, transport) = transport(vec![Outcome::RefusedLate, Outcome::Status(200)]);

    let result = pir_client::Transport::get(&transport, "http://pir.invalid/root").await;

    assert!(result.is_ok(), "the second attempt answered");
    assert_eq!(route.deadlines().len(), 2, "the expired budget was retried");
}

#[tokio::test(start_paused = true)]
async fn an_immediate_refusal_is_not_retried() {
    // Refused before the connect deadline could expire, so the endpoint gave a
    // definite answer. Repeating it would neither change the answer nor be
    // free: a PIR request carries a query the endpoint gets to observe.
    let (route, transport) = transport(vec![Outcome::Refused]);

    let result = pir_client::Transport::get(&transport, "http://pir.invalid/root").await;

    assert!(result.is_err());
    assert_eq!(route.deadlines().len(), 1, "a definite failure ends it");
}

#[tokio::test(start_paused = true)]
async fn a_non_success_status_is_not_retried() {
    let (route, transport) = transport(vec![Outcome::Status(404)]);

    let result = pir_client::Transport::get(&transport, "http://pir.invalid/root").await;

    assert!(result.is_err());
    assert_eq!(route.deadlines().len(), 1, "the status is the answer");
}

#[tokio::test(start_paused = true)]
async fn the_budget_is_finite_and_no_longer_than_one_long_deadline() {
    let (route, transport) = transport(vec![Outcome::Stall]);

    let result = pir_client::Transport::get(&transport, "http://pir.invalid/root").await;

    assert!(result.is_err(), "every attempt stalled");
    assert_eq!(route.deadlines().len(), 2, "the schedule is finite");
    let total: Duration = route.deadlines().iter().sum();
    assert_eq!(
        total, PIR_REQUEST_BUDGET,
        "both attempts share one budget, so no caller waits longer than the \
         single deadline this replaced"
    );
}

#[tokio::test(start_paused = true)]
async fn a_retried_post_carries_the_identical_body() {
    // Re-sending the identical encrypted query is what makes the retry safe to
    // reason about: the server learns nothing from a repeat, because which
    // item is fetched is exactly what PIR hides. A mutated body would be a
    // different query and would break that argument.
    let (route, transport) = transport(vec![Outcome::Stall, Outcome::Status(200)]);
    let query = vec![0x5A; 4096];

    let result =
        pir_client::Transport::post(&transport, "http://pir.invalid/tier1/query", query.clone())
            .await;

    assert!(result.is_ok(), "the second attempt answered");
    assert_eq!(
        route.bodies(),
        vec![query.clone(), query],
        "both attempts sent the same query"
    );
}

#[test]
fn a_connect_budget_only_ever_tightens_the_deadline() {
    // Connection setup must still give up before the backstop, whatever the
    // caller asks for, because the dispatch classification depends on it.
    let started = tokio::time::Instant::now();
    let timeout = Duration::from_secs(15);
    let backstop = started + timeout;

    let derived = connect_deadline(backstop, timeout, started, None);
    assert_eq!(
        backstop - derived,
        DIRECT_CONNECT_DEADLINE_LEAD,
        "without a budget the derived lead is unchanged"
    );

    let tightened = connect_deadline(backstop, timeout, started, Some(Duration::from_secs(5)));
    assert_eq!(
        tightened,
        started + Duration::from_secs(5),
        "a tighter budget wins, so a dead endpoint is abandoned early"
    );

    let relaxed = connect_deadline(backstop, timeout, started, Some(Duration::from_secs(600)));
    assert_eq!(
        relaxed, derived,
        "a budget beyond the backstop cannot outlive the derived lead"
    );
}
