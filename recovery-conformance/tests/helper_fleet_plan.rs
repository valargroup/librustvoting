//! The synthetic helper fleet, and the placement arithmetic its scenarios rest on.
//!
//! Hermetic. The arithmetic tests are not testing the SDK — they are pinning
//! the premise every fleet scenario is written against. A change to the target
//! count or the per-helper quota would leave the scenarios still passing while
//! quietly testing something else, which is exactly how provisioning already
//! treats the wallet's eleven-note, three-bundle layout.

use zcash_voting::helper::url::canonicalize_helper_base_url;
use zcash_voting::share_policy::{
    share_server_selection_policy, share_submission_target_count,
    SHARE_HELPER_MAX_INITIAL_SHARES_PER_SERVER, SHARE_HELPER_TARGET_COUNT_CAP,
    VOTE_COMMITMENT_SHARE_COUNT,
};

use recovery_conformance::helper_fleet::{HelperAvailability, HelperFleetPlan, SYNTHETIC_HELPER_URLS};

/// The fleet size every scenario is written for.
const FLEET: usize = 10;

const BACKEND: &str = "https://primary.example";

#[test]
fn the_fleet_is_the_size_the_scenarios_assume() {
    assert_eq!(SYNTHETIC_HELPER_URLS.len(), FLEET);
}

#[test]
fn ten_helpers_put_each_share_on_five_of_them() {
    // Half the fleet rounded up, capped at ten. This is what makes partial
    // delivery and deficit repair real: with one helper the target is one and
    // the whole placement layer is a degenerate case.
    assert_eq!(share_submission_target_count(FLEET), 5);
    assert_eq!(share_submission_target_count(1), 1);
}

#[test]
fn ten_helpers_sit_exactly_on_the_protocol_cap() {
    // The reason the fleet is ten rather than some other even number: the cap
    // stops being a value only a unit test has ever reached.
    assert_eq!(SHARE_HELPER_TARGET_COUNT_CAP, FLEET);
    assert_eq!(
        share_submission_target_count(FLEET * 2),
        SHARE_HELPER_TARGET_COUNT_CAP,
        "a larger fleet must still be capped"
    );
}

#[test]
fn half_the_fleet_is_below_the_pool_a_complete_batch_needs() {
    // The rule with no assertion anywhere today: readiness may enlarge the
    // planning pool but never shrink it below this minimum, pulling in
    // non-ready helpers as fallback. A scenario with five helpers up sits
    // under it, which is what makes that rule observable.
    let policy = share_server_selection_policy(FLEET);
    assert_eq!(policy.target_count, 5);
    assert_eq!(
        policy.max_shares_per_server as usize,
        SHARE_HELPER_MAX_INITIAL_SHARES_PER_SERVER
    );
    assert_eq!(policy.max_shares_per_server, 12);
    assert_eq!(
        policy.min_server_count, 7,
        "sixteen shares on five helpers each, twelve to a helper, needs seven"
    );
    assert!(
        (FLEET / 2) < policy.min_server_count as usize,
        "half the fleet must sit below the planning pool or the scenario proves nothing"
    );
}

#[test]
fn one_helper_hides_every_rule_the_fleet_exists_to_test() {
    // Stated as a test rather than as a comment, because it is the whole
    // justification for the fleet: driven against a single URL the SDK's
    // placement layer has nothing to decide.
    let single = share_server_selection_policy(1);
    assert_eq!(single.target_count, 1);
    assert_eq!(
        single.max_shares_per_server as usize, VOTE_COMMITMENT_SHARE_COUNT,
        "one helper receives the entire commitment"
    );
    assert_eq!(single.min_server_count, 1);
}

#[test]
fn every_synthetic_url_is_already_canonical_and_distinct() {
    // The journal records the canonical form. If canonicalization changed one
    // of these, an assertion comparing a planned URL to a journaled one would
    // silently compare two different strings.
    let mut canonical: Vec<String> = Vec::new();
    for url in SYNTHETIC_HELPER_URLS {
        let form = canonicalize_helper_base_url(url)
            .unwrap_or_else(|error| panic!("{url} is not a usable helper base: {error:?}"));
        assert_eq!(&form, url, "{url} is not already in canonical form");
        canonical.push(form);
    }
    let count = canonical.len();
    canonical.sort();
    canonical.dedup();
    assert_eq!(count, canonical.len(), "two synthetic helpers share an identity");
}

#[test]
fn synthetic_helpers_cannot_accidentally_resolve() {
    // `.invalid` is reserved and never resolves, so a bug in the route wrapper
    // fails as a DNS error rather than by quietly reaching some real host.
    for url in SYNTHETIC_HELPER_URLS {
        assert!(url.ends_with(".invalid"), "{url} is not in a reserved domain");
    }
}

#[test]
fn an_empty_plan_claims_no_traffic() {
    // What keeps every existing crash exercise unchanged: with no synthetic
    // helper named, the route wrapper delegates everything.
    let plan = HelperFleetPlan::none();
    assert!(plan.configured_urls().is_empty());
    assert_eq!(plan.resolve(SYNTHETIC_HELPER_URLS[0]), None);
    assert_eq!(plan.route_to_backend(SYNTHETIC_HELPER_URLS[0]), None);
}

#[test]
fn a_plan_with_no_backend_routes_nothing() {
    // A fleet whose answering helpers have nowhere to go must be treated as
    // unclaimed rather than have the synthetic URL passed to the network,
    // where it would fail as a DNS error attributed to the SDK.
    let plan = HelperFleetPlan {
        backend: String::new(),
        availability: [(SYNTHETIC_HELPER_URLS[0].to_string(), HelperAvailability::Answers)]
            .into_iter()
            .collect(),
    };
    assert_eq!(plan.resolve(SYNTHETIC_HELPER_URLS[0]), None);
}

#[test]
fn a_configured_fleet_keeps_unreachable_helpers_in_it() {
    // The distinction the SDK draws and the scenarios rely on: a helper that
    // refuses a connection is still configured. Reporting only the reachable
    // ones would turn every outage scenario into a fleet contraction, which is
    // a different rule with a different expected outcome.
    let plan = HelperFleetPlan::all_answering(BACKEND, FLEET)
        .with(&SYNTHETIC_HELPER_URLS[5..], HelperAvailability::Refuses);
    assert_eq!(plan.configured_urls().len(), FLEET);
    assert_eq!(plan.configured_urls(), SYNTHETIC_HELPER_URLS.to_vec());
}

#[test]
fn configured_order_follows_the_fleet_rather_than_the_map() {
    // Helper order is behaviour: the retry walk and the shuffle both start
    // from the configured list. A plan stored in a map must still report the
    // fleet's own order.
    let plan = HelperFleetPlan::all_answering(BACKEND, 3);
    assert_eq!(
        plan.configured_urls(),
        vec![
            SYNTHETIC_HELPER_URLS[0].to_string(),
            SYNTHETIC_HELPER_URLS[1].to_string(),
            SYNTHETIC_HELPER_URLS[2].to_string(),
        ]
    );
}

#[test]
fn availability_is_named_by_url_rather_than_by_index() {
    // So flipping a fleet reads as a statement about which helpers are up, and
    // cannot silently shift by one.
    let plan = HelperFleetPlan::all_answering(BACKEND, FLEET)
        .with(&SYNTHETIC_HELPER_URLS[..5], HelperAvailability::NeverAnswers);
    for url in &SYNTHETIC_HELPER_URLS[..5] {
        assert_eq!(plan.resolve(url).map(|(_, a)| a), Some(HelperAvailability::NeverAnswers));
    }
    for url in &SYNTHETIC_HELPER_URLS[5..] {
        assert_eq!(plan.resolve(url).map(|(_, a)| a), Some(HelperAvailability::Answers));
    }
}

#[test]
fn an_answering_helper_keeps_its_path_when_it_is_routed() {
    let plan = HelperFleetPlan::all_answering(BACKEND, FLEET);
    let requested = format!("{}/shielded-vote/v1/shares", SYNTHETIC_HELPER_URLS[3]);
    assert_eq!(
        plan.route_to_backend(&requested),
        Some(format!("{BACKEND}/shielded-vote/v1/shares"))
    );
    let status = format!("{}/shielded-vote/v1/share-status/ab/cd", SYNTHETIC_HELPER_URLS[9]);
    assert_eq!(
        plan.route_to_backend(&status),
        Some(format!("{BACKEND}/shielded-vote/v1/share-status/ab/cd"))
    );
}

#[test]
fn a_trailing_slash_on_the_backend_does_not_double_up() {
    let plan = HelperFleetPlan::all_answering(format!("{BACKEND}/"), 1);
    assert_eq!(
        plan.route_to_backend(&format!("{}/shielded-vote/v1/shares", SYNTHETIC_HELPER_URLS[0])),
        Some(format!("{BACKEND}/shielded-vote/v1/shares"))
    );
}

#[test]
fn traffic_outside_the_fleet_is_left_alone() {
    let plan = HelperFleetPlan::all_answering(BACKEND, FLEET);
    for url in [
        "https://vote.example/shielded-vote/v1/delegate-vote",
        "https://pir.example/query",
        BACKEND,
    ] {
        assert_eq!(plan.resolve(url), None, "{url}");
    }
}

#[test]
fn only_an_answering_helper_can_accept_and_only_silence_is_unknown() {
    assert!(HelperAvailability::Answers.can_accept());
    assert!(!HelperAvailability::Refuses.can_accept());
    assert!(!HelperAvailability::NeverAnswers.can_accept());

    // A refusal is a definite answer, even though it is not a helpful one.
    assert!(!HelperAvailability::Refuses.leaves_outcome_unknown());
    assert!(HelperAvailability::NeverAnswers.leaves_outcome_unknown());
    assert!(!HelperAvailability::Answers.leaves_outcome_unknown());
}

// --- the scenario taxonomy -------------------------------------------------

use recovery_conformance::helper_fleet::FleetScenario;
use recovery_conformance::CrashStage;

#[test]
fn every_scenario_has_a_distinct_name_that_round_trips() {
    let mut names: Vec<&str> = FleetScenario::ALL
        .iter()
        .map(|scenario| scenario.name())
        .collect();
    let count = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), count, "two scenarios share a name");

    for scenario in FleetScenario::ALL {
        assert_eq!(scenario.name().parse::<FleetScenario>().ok(), Some(*scenario));
    }
}

#[test]
fn an_unknown_scenario_name_is_rejected_rather_than_defaulted() {
    assert!("half-then-the-other-half".parse::<FleetScenario>().is_err());
    assert!("".parse::<FleetScenario>().is_err());
}

#[test]
fn every_scenario_leaves_the_resumed_run_something_to_do() {
    // The property that makes this matrix believable. A first run that finished
    // the round would make every later assertion hold trivially, and the live
    // suite has already been fooled that way once: `half-then-other-half`
    // passed having confirmed all 144 shares on the first half.
    //
    // Only `whole-fleet-down` can guarantee outstanding work without being cut
    // short — nothing can be delivered at all there. Every other scenario needs
    // a crash, because a share confirms at whatever placement it reaches, so
    // under-delivery alone leaves nothing owed.
    for scenario in FleetScenario::ALL {
        assert!(scenario.must_leave_work_outstanding(), "{scenario}");
        if scenario.crash_stage().is_none() {
            assert_eq!(
                *scenario,
                FleetScenario::WholeFleetDown,
                "{scenario} neither crashes nor blocks delivery, so its first run would \
                 finish the round and its resume would have nothing to do"
            );
        }
    }
}

#[test]
fn a_flip_scenario_is_cut_short_after_an_acceptance_not_before_one() {
    // The crash has to leave the round unfinished *and* leave acceptances
    // behind. `AfterSharePost` fires before anything is accepted, which was
    // observed live: 730 placements on the second half, zero on the first,
    // making "those acceptances survive the flip" a claim about an empty set.
    for scenario in [FleetScenario::HalfThenOtherHalf, FleetScenario::SilentHelpers] {
        assert_eq!(
            scenario.crash_stage(),
            Some(CrashStage::AfterShareAccepted),
            "{scenario} must be cut short after a helper has taken a share"
        );
        assert!(scenario.must_leave_acceptances_behind(), "{scenario}");
        assert!(scenario.must_leave_work_outstanding(), "{scenario}");
    }
}

#[test]
fn only_the_unreachable_fleet_leaves_no_acceptances_behind() {
    let empty: Vec<_> = FleetScenario::ALL
        .iter()
        .copied()
        .filter(|scenario| !scenario.must_leave_acceptances_behind())
        .collect();
    assert_eq!(empty, vec![FleetScenario::WholeFleetDown]);
}

#[test]
fn only_the_unreachable_fleet_needs_no_crash() {
    // The rest are about an unreachable fleet, and adding a crash to them would
    // confuse two faults whose durable evidence is different.
    // `whole-fleet-down` is the one scenario that needs no crash: with nothing
    // reachable, the outstanding work is guaranteed by the fleet itself.
    let uncrashed: Vec<_> = FleetScenario::ALL
        .iter()
        .copied()
        .filter(|scenario| scenario.crash_stage().is_none())
        .collect();
    assert_eq!(uncrashed, vec![FleetScenario::WholeFleetDown]);
    assert_eq!(
        FleetScenario::FullFleetThenCrash.crash_stage(),
        Some(CrashStage::AfterShareAccepted),
        "the crash must land where a helper has definitely accepted, or there is no \
         placement for the resume to avoid repeating"
    );
}

#[test]
fn the_first_half_is_strictly_below_the_target_it_must_fall_short_of() {
    // The scenario's whole premise. Were the first fleet able to meet every
    // share's target on its own, the flip would have no deficit to repair and
    // the exercise would pass having tested nothing.
    let first = FleetScenario::HalfThenOtherHalf.first_fleet(BACKEND);
    let answering = first
        .configured_urls()
        .iter()
        .filter(|url| {
            first
                .resolve(url)
                .is_some_and(|(_, availability)| availability.can_accept())
        })
        .count();
    assert!(
        answering < share_submission_target_count(FLEET),
        "{answering} answering helper(s) is not below the target of {}",
        share_submission_target_count(FLEET)
    );
}

#[test]
fn the_two_halves_of_a_flip_share_no_reachable_helper() {
    // Otherwise the second run could fill its deficit on a helper the first run
    // had already used, and the claim that every remaining acceptance came from
    // a helper never tried would be untrue.
    let first = FleetScenario::HalfThenOtherHalf.first_fleet(BACKEND);
    let second = FleetScenario::HalfThenOtherHalf.second_fleet(BACKEND);
    let answering = |plan: &HelperFleetPlan| -> Vec<String> {
        plan.configured_urls()
            .into_iter()
            .filter(|url| {
                plan.resolve(url)
                    .is_some_and(|(_, availability)| availability.can_accept())
            })
            .collect()
    };
    let before = answering(&first);
    let after = answering(&second);
    assert!(
        before.iter().all(|url| !after.contains(url)),
        "the halves overlap: {before:?} and {after:?}"
    );
    assert!(!before.is_empty() && !after.is_empty());
}

#[test]
fn a_contracted_fleet_is_smaller_rather_than_unreachable() {
    // Contraction and outage are different rules with different expected
    // outcomes: an unreachable helper is still configured, a contracted one is
    // not. Mixing them up would assert the wrong thing about the target.
    let contracted = FleetScenario::FleetContractsThenGrows.second_fleet(BACKEND);
    assert!(contracted.configured_urls().len() < FLEET);
    assert!(
        contracted
            .configured_urls()
            .iter()
            .all(|url| contracted
                .resolve(url)
                .is_some_and(|(_, a)| a.can_accept())),
        "a contracted fleet's remaining helpers are all reachable"
    );
    assert!(
        !FleetScenario::FleetContractsThenGrows.meets_the_full_target(),
        "a contracted fleet clamps the target, so the original one is not owed"
    );
}

#[test]
fn only_the_silent_scenario_leaves_an_unknown_outcome() {
    let silent: Vec<_> = FleetScenario::ALL
        .iter()
        .copied()
        .filter(|scenario| scenario.leaves_an_unknown_outcome())
        .collect();
    assert_eq!(silent, vec![FleetScenario::SilentHelpers]);

    // A refusal is definite, so a scenario built only from refusals leaves
    // nothing ambiguous behind.
    let plan = FleetScenario::WholeFleetDown.first_fleet(BACKEND);
    assert!(plan
        .configured_urls()
        .iter()
        .all(|url| !plan
            .resolve(url)
            .is_some_and(|(_, a)| a.leaves_outcome_unknown())));
}
