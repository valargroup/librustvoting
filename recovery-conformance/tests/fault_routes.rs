//! The two fault wrappers, driven without a network.
//!
//! These are the load-bearing mechanism of both new axes: if `StallingRoute`
//! quietly delegated its armed class, or `HelperFleetRoute` quietly let a
//! synthetic URL reach the network, the live matrices would still run, still
//! report green, and prove nothing. That is exactly the rot this suite exists
//! to prevent, so the wrappers are exercised here rather than trusted.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http::Method;
use zcash_voting::{RouteFuture, RouteHttp, RoutePhase, RouteRequest, RouteResponse};

use recovery_conformance::child::{CrashLog, Observation};
use recovery_conformance::helper_fleet::{
    HelperAvailability, HelperContacts, HelperFleetPlan, HelperFleetRoute, SYNTHETIC_HELPER_URLS,
};
use recovery_conformance::stall::{
    RequestClassifier, StallPlan, StallPoint, StallRecord, StallTarget, StallingRoute,
};

const BACKEND: &str = "https://primary.example";
const SHARES: &str = "/shielded-vote/v1/shares";

/// A route that answers everything, and remembers what it was asked.
///
/// The record is shared rather than owned, because `Arc<T>` is not itself a
/// `RouteHttp` — the wrapper takes the route by value, so a test that wants to
/// read what reached the network has to hold the log separately.
#[derive(Clone, Default)]
struct Recorded(Arc<Mutex<Vec<String>>>);

impl Recorded {
    fn urls(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

struct RecordingRoute {
    urls: Recorded,
}

impl RouteHttp for RecordingRoute {
    fn execute<'a>(
        &'a self,
        request: RouteRequest<'a>,
        on_dispatch: &'a (dyn Fn() + Send + Sync),
    ) -> RouteFuture<'a> {
        self.urls.0.lock().unwrap().push(request.url.to_string());
        on_dispatch();
        Box::pin(async move {
            Ok(RouteResponse {
                status: 200,
                headers: Vec::new(),
                body: b"{\"status\":\"queued\"}".to_vec(),
            })
        })
    }
}

impl RecordingRoute {
    fn sharing(urls: &Recorded) -> Self {
        Self { urls: urls.clone() }
    }
}

fn request<'a>(method: Method, url: &'a str) -> RouteRequest<'a> {
    RouteRequest {
        method,
        url,
        headers: &[],
        body: Vec::new(),
        timeout: Duration::from_secs(30),
        connect_timeout: None,
        max_response_bytes: 1024,
    }
}

/// A crash log in a fresh temporary file, plus its path.
fn log(name: &str) -> (Arc<CrashLog>, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "recovery-conformance-fault-route-{}-{name}.jsonl",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    (Arc::new(CrashLog::create(&path).unwrap()), path)
}

fn classifier() -> RequestClassifier {
    RequestClassifier::new(vec!["https://pir.example".to_string()])
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

/// Whether a future is still unresolved after a short wait.
///
/// A stalled request is supposed to never resolve, and "never" is not something
/// a test can wait for. Fifty milliseconds against a future that would
/// otherwise complete immediately is the distinction that matters.
fn still_hanging(future: impl std::future::Future) -> bool {
    runtime().block_on(async {
        tokio::time::timeout(Duration::from_millis(50), future)
            .await
            .is_err()
    })
}

// --- StallingRoute ---------------------------------------------------------

#[test]
fn an_unarmed_stall_route_delegates_everything() {
    // What keeps a control run and a stalled run on one code path.
    let (log, _path) = log("unarmed");
    let route = StallingRoute::new(
        RecordingRoute::sharing(&Recorded::default()),
        StallPlan::none(),
        classifier(),
        log,
    );
    let url = format!("https://helper.example{SHARES}");
    let response = runtime().block_on(route.execute(request(Method::POST, &url), &|| {}));
    assert_eq!(response.unwrap().status, 200);
}

#[test]
fn an_armed_route_hangs_only_the_class_it_names() {
    let (log, _path) = log("armed-class");
    let route = StallingRoute::new(
        RecordingRoute::sharing(&Recorded::default()),
        StallPlan::hanging(StallTarget::SharePost, StallPoint::AfterDispatch),
        classifier(),
        log,
    );

    let target = format!("https://helper.example{SHARES}");
    assert!(
        still_hanging(route.execute(request(Method::POST, &target), &|| {})),
        "the armed class must never answer"
    );

    // Every other request must pass through untouched, or the round dies of a
    // fault the exercise did not arm and the durable state means nothing.
    let other = "https://vote.example/shielded-vote/v1/delegate-vote";
    let response = runtime().block_on(route.execute(request(Method::POST, other), &|| {}));
    assert_eq!(response.unwrap().status, 200);
}

#[test]
fn the_stall_is_recorded_before_the_hang_begins() {
    // The record has to be on disk *before* the future stops answering: a
    // stalled run may be ended by its budget rather than by returning, and a
    // record written afterwards would never exist.
    let (log, path) = log("recorded");
    let route = StallingRoute::new(
        RecordingRoute::sharing(&Recorded::default()),
        StallPlan::hanging(StallTarget::SharePost, StallPoint::AfterDispatch),
        classifier(),
        log,
    );
    let url = format!("https://helper.example{SHARES}");
    assert!(still_hanging(
        route.execute(request(Method::POST, &url), &|| {})
    ));

    let records = StallRecord::from_observations(&CrashLog::read(&path).unwrap());
    assert_eq!(records.len(), 1);
    assert!(records[0].is(StallTarget::SharePost));
    assert!(records[0].after_dispatch);
    assert_eq!(records[0].url, url);
    // The deadline the SDK put on this very request, so the matrix can size a
    // budget from the SDK's own claim rather than from a hardcoded guess.
    assert_eq!(records[0].timeout, Duration::from_secs(30));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_before_dispatch_stall_leaves_the_hook_unfired() {
    // The whole basis of the SDK's definitely-unsent classification. A stall
    // that fired the hook anyway would make every pre-dispatch exercise assert
    // the ambiguous case under the safe case's name.
    let (log, _path) = log("before-dispatch");
    let fired = Arc::new(AtomicUsize::new(0));
    let route = StallingRoute::new(
        RecordingRoute::sharing(&Recorded::default()),
        StallPlan::hanging(StallTarget::SharePost, StallPoint::BeforeDispatch),
        classifier(),
        log,
    );
    let url = format!("https://helper.example{SHARES}");
    let counter = Arc::clone(&fired);
    let hook = move || {
        counter.fetch_add(1, Ordering::Relaxed);
    };
    assert!(still_hanging(
        route.execute(request(Method::POST, &url), &hook)
    ));
    assert_eq!(fired.load(Ordering::Relaxed), 0);
}

#[test]
fn an_after_dispatch_stall_fires_the_hook_first() {
    let (log, _path) = log("after-dispatch");
    let fired = Arc::new(AtomicUsize::new(0));
    let route = StallingRoute::new(
        RecordingRoute::sharing(&Recorded::default()),
        StallPlan::hanging(StallTarget::SharePost, StallPoint::AfterDispatch),
        classifier(),
        log,
    );
    let url = format!("https://helper.example{SHARES}");
    let counter = Arc::clone(&fired);
    let hook = move || {
        counter.fetch_add(1, Ordering::Relaxed);
    };
    assert!(still_hanging(
        route.execute(request(Method::POST, &url), &hook)
    ));
    assert_eq!(
        fired.load(Ordering::Relaxed),
        1,
        "a helper that accepted the connection and went quiet may well have \
         processed the request; the wallet must not be told otherwise"
    );
}

#[test]
fn a_lightwalletd_plan_arms_no_route_wrapper() {
    // It is not reached through the route, so arming for it would produce a
    // wrapper that can never fire — indistinguishable, from the matrix, from a
    // stall that failed to happen.
    let (log, _path) = log("lwd");
    let route = StallingRoute::new(
        RecordingRoute::sharing(&Recorded::default()),
        StallPlan::hanging(StallTarget::Lightwalletd, StallPoint::BeforeDispatch),
        classifier(),
        log,
    );
    let url = format!("https://helper.example{SHARES}");
    let response = runtime().block_on(route.execute(request(Method::POST, &url), &|| {}));
    assert_eq!(response.unwrap().status, 200);
}

// --- HelperFleetRoute ------------------------------------------------------

#[test]
fn an_empty_fleet_plan_touches_nothing() {
    let (log, _path) = log("empty-fleet");
    let inner = Recorded::default();
    let route = HelperFleetRoute::new(
        RecordingRoute::sharing(&inner),
        HelperFleetPlan::none(),
        log,
    );
    let url = format!("{}{SHARES}", SYNTHETIC_HELPER_URLS[0]);
    let _ = runtime().block_on(route.execute(request(Method::POST, &url), &|| {}));
    assert_eq!(inner.urls(), vec![url]);
}

#[test]
fn an_answering_helper_reaches_the_backend_under_its_own_path() {
    let (log, _path) = log("answering");
    let inner = Recorded::default();
    let route = HelperFleetRoute::new(
        RecordingRoute::sharing(&inner),
        HelperFleetPlan::all_answering(BACKEND, 10),
        log,
    );
    let url = format!("{}{SHARES}", SYNTHETIC_HELPER_URLS[7]);
    let response = runtime().block_on(route.execute(request(Method::POST, &url), &|| {}));
    assert_eq!(response.unwrap().status, 200);
    // The synthetic name never reaches the network; the path does.
    assert_eq!(inner.urls(), vec![format!("{BACKEND}{SHARES}")]);
}

#[test]
fn a_refusing_helper_fails_before_dispatch_and_never_reaches_the_network() {
    // Pre-dispatch is the truthful phase — nothing was built, let alone sent —
    // and it is what entitles the SDK to try another helper with no ambiguity
    // to carry.
    let (log, path) = log("refusing");
    let inner = Recorded::default();
    let plan = HelperFleetPlan::all_answering(BACKEND, 10)
        .with(&SYNTHETIC_HELPER_URLS[..1], HelperAvailability::Refuses);
    let route = HelperFleetRoute::new(RecordingRoute::sharing(&inner), plan, log);
    let url = format!("{}{SHARES}", SYNTHETIC_HELPER_URLS[0]);
    let error = runtime()
        .block_on(route.execute(request(Method::POST, &url), &|| {}))
        .expect_err("a refusing helper must not answer");
    assert_eq!(error.phase, RoutePhase::BeforeDispatch);
    assert!(
        inner.urls().is_empty(),
        "a refused request must not be sent"
    );

    let contacts = HelperContacts::from_observations(&CrashLog::read(&path).unwrap());
    assert_eq!(
        contacts.refused,
        [SYNTHETIC_HELPER_URLS[0].to_string()].into_iter().collect()
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_silent_helper_never_answers_and_says_so_in_the_log() {
    let (log, path) = log("silent");
    let inner = Recorded::default();
    let plan = HelperFleetPlan::all_answering(BACKEND, 10).with(
        &SYNTHETIC_HELPER_URLS[..1],
        HelperAvailability::NeverAnswers,
    );
    let route = HelperFleetRoute::new(RecordingRoute::sharing(&inner), plan, log);
    let url = format!("{}{SHARES}", SYNTHETIC_HELPER_URLS[0]);
    assert!(still_hanging(
        route.execute(request(Method::POST, &url), &|| {})
    ));

    let contacts = HelperContacts::from_observations(&CrashLog::read(&path).unwrap());
    assert_eq!(
        contacts.unanswered,
        [SYNTHETIC_HELPER_URLS[0].to_string()].into_iter().collect()
    );
    // A silence is not a refusal. Recording it as one would licence the wallet
    // to move on as though the helper definitely never took the share.
    assert!(contacts.refused.is_empty());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn only_share_posts_are_recorded() {
    // Readiness probes and status polls reach every helper on every pass and
    // say nothing about placement. Recording them would bury the handful of
    // records an assertion reads under thousands of fsynced lines.
    let (log, path) = log("status-only");
    let inner = Recorded::default();
    let plan = HelperFleetPlan::all_answering(BACKEND, 10);
    let route = HelperFleetRoute::new(RecordingRoute::sharing(&inner), plan, log);
    let status = format!("{}/shielded-vote/v1/status", SYNTHETIC_HELPER_URLS[0]);
    let _ = runtime().block_on(route.execute(request(Method::GET, &status), &|| {}));

    let observations = CrashLog::read(&path).unwrap();
    assert!(
        observations.is_empty(),
        "a readiness probe left {observations:?}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_share_post_that_is_answered_records_the_synthetic_url() {
    // Not the backend that served it. An assertion about re-sending to a helper
    // that already accepted is an assertion about the identity the wallet
    // holds, and every synthetic helper shares one backend.
    let (log, path) = log("records-synthetic");
    let inner = Recorded::default();
    let route = HelperFleetRoute::new(
        RecordingRoute::sharing(&inner),
        HelperFleetPlan::all_answering(BACKEND, 10),
        log,
    );
    let url = format!("{}{SHARES}", SYNTHETIC_HELPER_URLS[4]);
    let _ = runtime().block_on(route.execute(request(Method::POST, &url), &|| {}));

    let observations = CrashLog::read(&path).unwrap();
    assert!(matches!(
        observations.as_slice(),
        [Observation::HelperPost { url: recorded, status: 200 }]
            if recorded == SYNTHETIC_HELPER_URLS[4]
    ));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_two_wrappers_compose_the_way_a_run_stacks_them() {
    // The stall wrapper sits outside the fleet wrapper, so an armed class hangs
    // whichever helper it was headed for. Getting this backwards would let a
    // refusing helper answer a stall's target.
    let (log, _path) = log("composed");
    let fleet = HelperFleetRoute::new(
        RecordingRoute::sharing(&Recorded::default()),
        HelperFleetPlan::all_answering(BACKEND, 10),
        Arc::clone(&log),
    );
    let route = StallingRoute::new(
        fleet,
        StallPlan::hanging(StallTarget::SharePost, StallPoint::AfterDispatch),
        classifier(),
        log,
    );
    let url = format!("{}{SHARES}", SYNTHETIC_HELPER_URLS[2]);
    assert!(still_hanging(
        route.execute(request(Method::POST, &url), &|| {})
    ));
}
