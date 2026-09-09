use super::observations::observe_helper_http;
use crate::{ObservabilityOptions, ObservationAttribution, ObservationOutcome, ObservationScope};
use std::{
    future::{poll_fn, Future},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll, Wake, Waker},
    time::Duration,
};

struct WakeCount(AtomicUsize);
impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn records_wake_delay_and_preserves_result_and_attribution() {
    let owner = ObservationScope::new(Some(ObservabilityOptions::default())).invocation();
    let attribution = ObservationAttribution {
        bundle_index: Some(2),
        proposal_id: Some(1),
        share_index: Some(0),
    };
    let mut polls = 0;
    let future = poll_fn(|context| {
        polls += 1;
        if polls == 1 {
            context.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(42)
        }
    });
    let mut observed = Box::pin(observe_helper_http(
        owner.attributed(attribution.clone()),
        future,
    ));
    let count = Arc::new(WakeCount(AtomicUsize::new(0)));
    let waker = Waker::from(count.clone());
    let mut context = Context::from_waker(&waker);
    assert!(observed.as_mut().poll(&mut context).is_pending());
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
    std::thread::sleep(Duration::from_millis(15));
    assert_eq!(observed.as_mut().poll(&mut context), Poll::Ready(42));
    drop(observed);
    let report = owner
        .finish("test", None, ObservationOutcome::Succeeded)
        .unwrap();
    let delay = report
        .records
        .iter()
        .find(|r| r.stage.as_ref() == "helper.http.wake_to_poll")
        .unwrap();
    assert!(delay.elapsed_us >= 10_000);
    assert_eq!(delay.attribution, attribution);
    assert_eq!(
        report
            .records
            .iter()
            .filter(|r| r.stage.as_ref() == "helper.http.poll")
            .count(),
        2
    );
}

#[test]
fn disabled_observation_preserves_pending_and_cancellation() {
    let owner = ObservationScope::new(None).invocation();
    let mut observed = Box::pin(observe_helper_http(
        owner.scope().clone(),
        std::future::pending::<()>(),
    ));
    let waker = Waker::from(Arc::new(WakeCount(AtomicUsize::new(0))));
    assert!(observed
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    drop(observed);
    assert!(owner
        .finish("test", None, ObservationOutcome::Cancelled)
        .is_none());
}

#[tokio::test]
async fn separates_response_headers_from_delayed_body() {
    use super::{HelperTransport, HyperTransport};
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut request = Vec::new();
        let mut chunk = [0; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0);
            request.extend_from_slice(&chunk[..count]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n")
            .unwrap();
        std::thread::sleep(Duration::from_millis(60));
        stream.write_all(b"{}").unwrap();
    });
    let owner = ObservationScope::new(Some(ObservabilityOptions::default())).invocation();
    let transport = HyperTransport::new();
    let url = format!("http://{address}");
    let response = observe_helper_http(
        owner.scope().clone(),
        transport.get(&url, Duration::from_secs(3)),
    )
    .await;
    server.join().unwrap();
    assert_eq!(response.unwrap().body(), b"{}");
    let report = owner
        .finish("test", None, ObservationOutcome::Succeeded)
        .unwrap();
    let headers = report
        .records
        .iter()
        .find(|r| r.stage.as_ref() == "helper.http.response_headers")
        .unwrap();
    let body = report
        .records
        .iter()
        .find(|r| r.stage.as_ref() == "helper.http.response_body")
        .unwrap();
    assert!(body.started_after_us >= headers.started_after_us + headers.elapsed_us);
    assert!(body.elapsed_us >= 30_000);
}
