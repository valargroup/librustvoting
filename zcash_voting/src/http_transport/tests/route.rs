//! Routed transport behaviour: every SDK transport runs through one `RouteHttp`
//! executor, and failure classification is derived from its dispatch hook.

use std::{sync::Arc, time::Duration};

use http::Method;

use crate::chain_submission::{
    ChainHttpRequest, ChainPostDispatch, ChainTransport, ChainTransportFailureKind,
};
use crate::helper::transport::{HelperTransport, HelperTransportError};

use super::super::{
    HyperTransport, PirHttpFailure, PirHttpFailurePhase, RouteError, RouteFuture, RouteHttp,
    RouteRequest, RouteResponse,
};

/// Executor with a scripted outcome, so classification is tested without sockets.
struct ScriptedRoute {
    dispatch: bool,
    outcome: Result<u16, RouteError>,
    hang: bool,
}

impl RouteHttp for ScriptedRoute {
    fn execute<'a>(
        &'a self,
        _request: RouteRequest<'a>,
        on_dispatch: &'a (dyn Fn() + Send + Sync),
    ) -> RouteFuture<'a> {
        Box::pin(async move {
            if self.dispatch {
                on_dispatch();
            }
            if self.hang {
                std::future::pending::<()>().await;
            }
            match &self.outcome {
                Ok(status) => Ok(RouteResponse {
                    status: *status,
                    headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                    body: b"{}".to_vec(),
                }),
                Err(error) => Err(error.clone()),
            }
        })
    }
}

fn transport(dispatch: bool, outcome: Result<u16, RouteError>) -> HyperTransport<ScriptedRoute> {
    HyperTransport::with_route(ScriptedRoute {
        dispatch,
        outcome,
        hang: false,
    })
}

fn hanging(dispatch: bool) -> HyperTransport<ScriptedRoute> {
    HyperTransport::with_route(ScriptedRoute {
        dispatch,
        outcome: Ok(200),
        hang: true,
    })
}

fn chain_request() -> ChainHttpRequest {
    ChainHttpRequest::new(
        "https://chain.example/tx".to_string(),
        Vec::new(),
        Duration::from_millis(50),
        1024,
    )
}

#[tokio::test]
async fn helper_failures_classify_by_dispatch_and_phase() {
    let before = transport(
        false,
        Err(RouteError::before_dispatch("tor route unavailable")),
    );
    assert!(matches!(
        before.post_json("https://h", b"{}".to_vec(), Duration::from_secs(1)).await,
        Err(HelperTransportError::Transport(message)) if message.contains("tor route unavailable")
    ));

    let after = transport(true, Err(RouteError::after_dispatch("reset")));
    assert!(matches!(
        after
            .post_json("https://h", b"{}".to_vec(), Duration::from_secs(1))
            .await,
        Err(HelperTransportError::Ambiguous(_))
    ));

    let body = transport(true, Err(RouteError::response_read("too large")));
    assert!(matches!(
        body.get("https://h", Duration::from_secs(1)).await,
        Err(HelperTransportError::Response(_))
    ));

    // An executor that reports a post-dispatch phase without calling the
    // hook is still treated as dispatched: the phase is the executor's
    // own admission that bytes may have left.
    let admitted = transport(false, Err(RouteError::after_dispatch("reset")));
    assert!(matches!(
        admitted
            .post_json("https://h", b"{}".to_vec(), Duration::from_secs(1))
            .await,
        Err(HelperTransportError::Ambiguous(_))
    ));

    // A custom executor that called the hook and then reports a pre-dispatch
    // failure is still possibly dispatched: bytes may have left before it
    // decided on the phase, and only the direct route, whose client fuses
    // connection setup with the first write, has that phase honored.
    let hooked_then_before = transport(
        true,
        Err(RouteError::before_dispatch("proxy closed the stream")),
    );
    assert!(matches!(
        hooked_then_before
            .post_json("https://h", b"{}".to_vec(), Duration::from_secs(1))
            .await,
        Err(HelperTransportError::Ambiguous(_))
    ));

    let ok = transport(true, Ok(200));
    let response = ok.get("https://h", Duration::from_secs(1)).await.unwrap();
    assert!(response.is_success());
    assert_eq!(response.content_type(), Some("application/json"));
}

#[tokio::test]
async fn helper_deadline_before_dispatch_is_definite_and_after_is_a_timeout() {
    let stalled_route = hanging(false);
    assert!(matches!(
        stalled_route
            .post_json("https://h", b"{}".to_vec(), Duration::from_millis(30))
            .await,
        Err(HelperTransportError::Transport(_))
    ));
    let stalled_server = hanging(true);
    assert!(matches!(
        stalled_server
            .post_json("https://h", b"{}".to_vec(), Duration::from_millis(30))
            .await,
        Err(HelperTransportError::Timeout)
    ));
}

#[tokio::test]
async fn direct_tls_stall_is_a_definite_pre_dispatch_failure() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (_stream, _) = listener.accept().unwrap();
        // Accept TCP but never complete TLS.
        std::thread::sleep(Duration::from_millis(150));
    });
    let transport = HyperTransport::new();
    let result = transport
        .post_json(
            &format!("https://{address}/shielded-vote/v1/shares"),
            b"{}".to_vec(),
            Duration::from_millis(40),
        )
        .await;
    assert!(
        matches!(result, Err(HelperTransportError::Transport(_))),
        "{result:?}"
    );
    server.join().unwrap();
}

#[tokio::test]
async fn chain_failures_classify_by_dispatch_and_mark_the_dispatch_handle() {
    let before = transport(false, Err(RouteError::before_dispatch("blocked")));
    let error = before
        .chain_post_json(chain_request(), b"{}".to_vec())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ChainTransportFailureKind::DefinitelyUnsent);

    let after = transport(true, Err(RouteError::after_dispatch("reset")));
    let dispatch = ChainPostDispatch::default();
    let error = after
        .chain_post_json_with_dispatch(chain_request(), b"{}".to_vec(), dispatch.clone())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ChainTransportFailureKind::PossiblyDispatched);
    assert!(dispatch.is_possible());

    let stalled_route = hanging(false);
    let dispatch = ChainPostDispatch::default();
    let error = stalled_route
        .chain_post_json_with_dispatch(chain_request(), b"{}".to_vec(), dispatch.clone())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ChainTransportFailureKind::DefinitelyUnsent);
    assert!(!dispatch.is_possible());

    let ok = transport(true, Ok(200));
    let response = ok.chain_get(chain_request()).await.unwrap();
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn pir_failures_carry_typed_phase_and_status() {
    use pir_client::Transport;

    let before = transport(false, Err(RouteError::before_dispatch("blocked")));
    let error = Transport::get(&before, "https://pir/root")
        .await
        .err()
        .expect("PIR request should fail");
    let typed = PirHttpFailure::from_error_chain(&error).unwrap();
    assert_eq!(typed.phase, PirHttpFailurePhase::Connect);
    assert!(typed.retryable());

    let unavailable = transport(true, Ok(503));
    let error = Transport::get(&unavailable, "https://pir/root")
        .await
        .err()
        .expect("PIR request should fail");
    let typed = PirHttpFailure::from_error_chain(&error).unwrap();
    assert_eq!(typed.phase, PirHttpFailurePhase::Status);
    assert_eq!(typed.http_status, Some(503));
    assert!(typed.retryable());

    let missing = transport(true, Ok(404));
    let error = Transport::get(&missing, "https://pir/root")
        .await
        .err()
        .expect("PIR request should fail");
    assert!(!PirHttpFailure::from_error_chain(&error)
        .unwrap()
        .retryable());

    let ok = transport(true, Ok(200));
    assert_eq!(
        Transport::post(&ok, "https://pir/query", vec![1])
            .await
            .unwrap()
            .status,
        200
    );
}

#[test]
fn tree_transport_reports_route_failures_as_request_errors() {
    use vote_commitment_tree_client::transport::{Transport, TransportError};

    let before = Arc::new(transport(
        false,
        Err(RouteError::before_dispatch("blocked")),
    ));
    assert!(matches!(
        Transport::get(&*before, "https://node/tree"),
        Err(TransportError::Request(message)) if message.contains("blocked")
    ));
    let ok = transport(true, Ok(200));
    assert_eq!(
        Transport::get(&ok, "https://node/tree").unwrap().status,
        200
    );
    let _ = Method::GET;
}

#[test]
fn the_connect_deadline_leads_the_backstop_by_a_bounded_fraction_of_the_timeout() {
    use super::super::{direct_connect_deadline, DIRECT_CONNECT_DEADLINE_LEAD};
    let backstop = tokio::time::Instant::now() + Duration::from_secs(60);

    let long = direct_connect_deadline(backstop, Duration::from_secs(5));
    assert_eq!(backstop - long, DIRECT_CONNECT_DEADLINE_LEAD);

    let short = direct_connect_deadline(backstop, Duration::from_millis(24));
    assert_eq!(backstop - short, Duration::from_millis(6));

    assert!(long < backstop && short < backstop);
}

#[test]
fn only_a_deadline_wrapped_direct_route_claims_to_enforce_the_connect_budget() {
    use super::super::DirectRoute;
    use hyper_util::client::legacy::connect::HttpConnector;

    // `new` and `with_http_connector` install `ConnectDeadlineConnector`, so
    // their connect failures at or after the budget really are the budget.
    assert!(DirectRoute::new().enforces_connect_timeout());
    assert!(DirectRoute::with_http_connector(HttpConnector::new()).enforces_connect_timeout());

    // `with_connector` uses the connector as given and installs no deadline,
    // so it must claim nothing: a refusal this route reports late is a
    // refusal, and re-reading it as a timeout would repeat a definite answer.
    assert!(!DirectRoute::with_connector(HttpConnector::new()).enforces_connect_timeout());
}

#[test]
fn an_unrepresentable_connect_budget_leaves_the_derived_deadline_unchanged() {
    use super::super::{connect_deadline, DIRECT_CONNECT_DEADLINE_LEAD};
    let started = tokio::time::Instant::now();
    let timeout = Duration::from_secs(15);
    let backstop = started + timeout;

    // `connect_timeout` is a public field, so a budget too large to add to the
    // current instant must fall back to the derived lead rather than panic.
    let huge = connect_deadline(backstop, timeout, started, Some(Duration::MAX));

    assert_eq!(backstop - huge, DIRECT_CONNECT_DEADLINE_LEAD);
}

/// Connector that never becomes ready, so readiness alone decides the outcome.
#[derive(Clone)]
struct NeverReadyConnector;

impl tower_service::Service<http::Uri> for NeverReadyConnector {
    type Response = ();
    type Error = std::io::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = std::result::Result<(), std::io::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        std::task::Poll::Pending
    }

    fn call(&mut self, _uri: http::Uri) -> Self::Future {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test(start_paused = true)]
async fn a_stalled_connector_readiness_is_abandoned_at_the_connect_deadline() {
    use super::super::{ConnectDeadlineConnector, DIRECT_CONNECT_DEADLINE};
    use tower_service::Service;

    // Readiness is part of connection setup, and the bound has to hold for a
    // connector that never wakes anyone: it decides when it is polled again,
    // so a clock sampled on entry may never be read after the deadline. This
    // awaits readiness the way Hyper does rather than re-polling by hand, so
    // only a timer that registered the waker can end the wait.
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_secs(5);
    let ready = DIRECT_CONNECT_DEADLINE
        .scope(Some(deadline), async {
            let mut connector = ConnectDeadlineConnector {
                inner: NeverReadyConnector,
                readiness_timer: None,
            };
            std::future::poll_fn(|cx| connector.poll_ready(cx)).await
        })
        .await;

    let error = ready.expect_err("the deadline ended the wait");
    assert!(
        error.to_string().contains("timed out"),
        "unexpected error: {error}"
    );
    assert_eq!(
        tokio::time::Instant::now() - started,
        Duration::from_secs(5),
        "the wait ended at the connect deadline, not at some looser bound"
    );
}

#[tokio::test(start_paused = true)]
async fn connector_readiness_within_the_deadline_defers_to_the_inner_connector() {
    use super::super::{ConnectDeadlineConnector, DIRECT_CONNECT_DEADLINE};
    use tower_service::Service;

    // The bound must not pre-empt a connector that is simply not ready yet.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    DIRECT_CONNECT_DEADLINE
        .scope(Some(deadline), async {
            let mut connector = ConnectDeadlineConnector {
                inner: NeverReadyConnector,
                readiness_timer: None,
            };
            let polled =
                std::future::poll_fn(|cx| std::task::Poll::Ready(connector.poll_ready(cx))).await;
            assert!(
                polled.is_pending(),
                "before the deadline the inner connector decides"
            );
        })
        .await;
}
