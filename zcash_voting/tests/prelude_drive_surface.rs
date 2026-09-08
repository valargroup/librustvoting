//! The documented import path can name a driver and everything it needs.
//!
//! Integrations are told to import `zcash_voting::prelude::*`. A driver
//! reachable only from the crate root makes that path insufficient: a host
//! following the documentation cannot name the type, its policy, its host
//! source, its reporter, or its report without an import the documentation
//! never mentions. This is a compile-time test — it fails to build rather than
//! to assert.

use std::time::Duration;

use zcash_voting::prelude::*;

#[test]
fn a_share_tracking_run_can_be_set_up_entirely_from_the_prelude() {
    let policy = ShareTrackingDrivePolicy {
        failure_retry: Duration::from_secs(30),
        ..ShareTrackingDrivePolicy::default()
    };

    let host = ShareTrackingHostSourceBridge::new(|| ShareTrackingHostContext {
        configured_helper_urls: vec!["https://helper.example".to_string()],
        now_seconds: 1_000,
        vote_end_time_seconds: Some(2_000),
    });
    let events = ShareTrackingReporterBridge::new(|event: ShareTrackingEvent| {
        let _ = event;
    });

    // Naming the traits is the point: a host implements or holds these.
    let host: &dyn ShareTrackingHostSource = &host;
    let events: &dyn ShareTrackingReporter = &events;
    let _ = (host, events, NoopShareTrackingReporter::default());

    // The driver, its report and its quiescence come from the same path.
    fn quiescence_of(report: ShareTrackingRunReport) -> ShareTrackingQuiescence {
        report.quiescence
    }
    fn paced<'a>(driver: ShareTrackingDriver<'a>) -> ShareTrackingDriver<'a> {
        driver.with_policy(ShareTrackingDrivePolicy::default())
    }
    let _ = (quiescence_of, paced);

    assert_eq!(policy.failure_retry, Duration::from_secs(30));
}
