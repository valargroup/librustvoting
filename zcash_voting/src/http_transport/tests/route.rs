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
