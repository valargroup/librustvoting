//! The stall taxonomy, and the request classification it rests on.
//!
//! Hermetic. Nothing here reaches the network; what is under test is whether
//! the suite can still tell one class of request from another, which is the
//! part that rots when an endpoint moves.

use std::time::Duration;

use recovery_conformance::stall::{RequestClassifier, StallPoint, StallTarget};
use recovery_conformance::StallPlan;

const VOTE: &str = "https://vote.example";
const HELPER: &str = "https://helper.example";
const PIR: &str = "https://pir.example";

fn classifier() -> RequestClassifier {
    RequestClassifier::new(vec![PIR.to_string()])
}

#[test]
fn every_target_has_a_distinct_name_that_round_trips() {
    let mut names: Vec<&str> = StallTarget::ALL.iter().map(|target| target.name()).collect();
    let count = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), count, "two stall targets share a name");

    for target in StallTarget::ALL {
        assert_eq!(
            target.name().parse::<StallTarget>().ok(),
            Some(*target),
            "{target} does not survive a round trip through its name"
        );
    }
}

#[test]
fn an_unknown_target_name_is_rejected_rather_than_defaulted() {
    // The same rule the crash stages follow. A typo that selected nothing
    // would report a green run having tested no ground at all, which is the
    // failure mode this whole suite exists to prevent.
    assert!("share-posts".parse::<StallTarget>().is_err());
    assert!("".parse::<StallTarget>().is_err());
}

#[test]
fn only_lightwalletd_is_unreachable_through_the_route() {
    // Everything else is injected below the SDK's deadlines by wrapping the
    // shared route. Lightwalletd dials tonic directly, so it needs a
    // black-hole listener instead, and a plan must not arm a route wrapper
    // that could never fire for it.
    let unrouted: Vec<_> = StallTarget::ALL
        .iter()
        .copied()
        .filter(|target| !target.is_routed())
        .collect();
    assert_eq!(unrouted, vec![StallTarget::Lightwalletd]);

    let plan = StallPlan::hanging(StallTarget::Lightwalletd, StallPoint::BeforeDispatch);
    assert_eq!(plan.armed_target(), None);
    let routed = StallPlan::hanging(StallTarget::SharePost, StallPoint::AfterDispatch);
    assert_eq!(routed.armed_target(), Some(StallTarget::SharePost));
}

#[test]
fn only_the_two_transaction_posts_can_leave_a_submission_in_doubt() {
    let carrying: Vec<_> = StallTarget::ALL
        .iter()
        .copied()
        .filter(|target| target.carries_a_submission())
        .collect();
    assert_eq!(
        carrying,
        vec![StallTarget::DelegationPost, StallTarget::VotePost],
        "only a POST that carries a transaction can leave one possibly delivered"
    );
}

#[test]
fn every_target_declares_a_usable_bound() {
    // A target whose bound is zero or absurd would size a run's budget wrongly
    // in whichever direction hides the finding: too small and every run reports
    // an unbounded request, too large and none ever does.
    for target in StallTarget::ALL {
        let bound = target.declared_bound();
        assert!(bound >= Duration::from_secs(1), "{target} bound is too small");
        assert!(
            bound <= Duration::from_secs(300),
            "{target} bound is longer than any the SDK applies"
        );
    }
}

#[test]
fn a_plan_with_no_target_stalls_nothing() {
    assert_eq!(StallPlan::none().armed_target(), None);
    assert_eq!(StallPlan::none().target, None);
}

#[test]
fn the_default_point_is_the_conservative_one() {
    // A plan built without saying which point it meant must not claim the
    // ambiguous case: `BeforeDispatch` asserts less and is always true of a
    // request that never reached the network.
    assert_eq!(StallPoint::default(), StallPoint::BeforeDispatch);
    assert_eq!(StallPlan::none().point, StallPoint::BeforeDispatch);
}

#[test]
fn each_endpoint_is_classified_as_the_target_that_names_it() {
    let classifier = classifier();
    let cases = [
        ("POST", "/shielded-vote/v1/delegate-vote", StallTarget::DelegationPost),
        ("POST", "/shielded-vote/v1/cast-vote", StallTarget::VotePost),
        ("POST", "/shielded-vote/v1/cast-vote-batch", StallTarget::VotePost),
        ("GET", "/shielded-vote/v1/tx/abcd", StallTarget::TransactionLookup),
        (
            "GET",
            "/shielded-vote/v1/commitment-tree/round-1/latest",
            StallTarget::CommitmentTreeRead,
        ),
        (
            "GET",
            "/shielded-vote/v1/commitment-tree/round-1/leaves?from_height=1&to_height=2",
            StallTarget::CommitmentTreeRead,
        ),
    ];
    for (method, path, expected) in cases {
        assert_eq!(
            classifier.classify(method, &format!("{VOTE}{path}")),
            Some(expected),
            "{method} {path}"
        );
    }

    let helper_cases = [
        ("GET", "/shielded-vote/v1/status", StallTarget::HelperPreflight),
        ("POST", "/shielded-vote/v1/shares", StallTarget::SharePost),
        (
            "GET",
            "/shielded-vote/v1/share-status/ab/cd",
            StallTarget::ShareStatus,
        ),
    ];
    for (method, path, expected) in helper_cases {
        assert_eq!(
            classifier.classify(method, &format!("{HELPER}{path}")),
            Some(expected),
            "{method} {path}"
        );
    }
}

#[test]
fn a_pir_endpoint_is_recognized_by_its_configured_url() {
    // PIR owns its own URL shapes, so it is matched by the endpoint the fleet
    // was configured with rather than by a path this suite would have to keep
    // in step.
    let classifier = classifier();
    assert_eq!(
        classifier.classify("POST", &format!("{PIR}/whatever/the/client/asks")),
        Some(StallTarget::PirQuery)
    );
    assert_eq!(
        classifier.classify("GET", "https://not-pir.example/query"),
        None
    );
}

#[test]
fn a_known_endpoint_reached_by_the_wrong_method_is_not_classified() {
    // A GET to the share endpoint is not the request `share-post` names, and
    // stalling it would report a boundary the run never crossed.
    let classifier = classifier();
    assert_eq!(
        classifier.classify("GET", &format!("{HELPER}/shielded-vote/v1/shares")),
        None
    );
    assert_eq!(
        classifier.classify("POST", &format!("{VOTE}/shielded-vote/v1/tx/abcd")),
        None
    );
}

#[test]
fn traffic_no_target_names_passes_through_unclassified() {
    let classifier = classifier();
    for url in [
        "https://vote.example/health",
        "https://vote.example/shielded-vote/v2/cast-vote",
        "https://lightwalletd.example:443",
    ] {
        assert_eq!(classifier.classify("GET", url), None, "{url}");
        assert_eq!(classifier.classify("POST", url), None, "{url}");
    }
}

#[test]
fn a_mount_path_does_not_hide_the_endpoint() {
    // Helper bases may carry a mount path, and the vote chain's own tests use
    // one. Classification keys on the versioned API prefix rather than on the
    // whole path, so a mounted deployment classifies the same way.
    let classifier = classifier();
    assert_eq!(
        classifier.classify("POST", "https://vote.example/mount/shielded-vote/v1/delegate-vote"),
        Some(StallTarget::DelegationPost)
    );
}
