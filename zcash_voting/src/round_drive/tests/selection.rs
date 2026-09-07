//! Which step the driver takes next.

use crate::round_drive::selection::{bundle_index, lock_scope, next_dispatches, StepLockScope};
use crate::session::NextStep;

fn steps() -> Vec<NextStep> {
    vec![
        NextStep::Delegate { bundle_index: 0 },
        NextStep::Delegate { bundle_index: 1 },
        NextStep::CastVote {
            bundle_index: 1,
            proposal_id: 2,
            choice: 0,
        },
    ]
}

#[test]
fn plan_order_decides_when_nothing_is_isolated() {
    assert_eq!(
        next_dispatches(&steps(), &[], &[], 1, 1, true),
        vec![NextStep::Delegate { bundle_index: 0 }]
    );
}

#[test]
fn an_isolated_bundle_is_passed_over_entirely() {
    // Skipping is keyed on the bundle, so the isolated bundle's later vote
    // work is passed over too. Dispatching it would only earn an
    // `InvalidInput` for the delegation prerequisite it never cleared.
    assert_eq!(
        next_dispatches(&steps(), &[0], &[], 1, 1, true),
        vec![NextStep::Delegate { bundle_index: 1 }]
    );
    assert!(next_dispatches(&steps(), &[0, 1], &[], 3, 3, true).is_empty());
}

#[test]
fn a_repolled_step_is_taken_again_ahead_of_plan_order() {
    let pending = NextStep::CastVote {
        bundle_index: 1,
        proposal_id: 2,
        choice: 0,
    };
    assert_eq!(
        next_dispatches(&steps(), &[], std::slice::from_ref(&pending), 3, 3, true),
        vec![pending],
        "a pending submission is polled again, not starved by an earlier step"
    );
}

#[test]
fn a_repolled_step_the_plan_dropped_falls_back_to_plan_order() {
    let gone = NextStep::AdvanceVote {
        bundle_index: 9,
        proposal_id: 9,
    };
    assert_eq!(
        next_dispatches(&steps(), &[], std::slice::from_ref(&gone), 1, 1, true),
        vec![NextStep::Delegate { bundle_index: 0 }]
    );
}

#[test]
fn a_repolled_step_on_an_isolated_bundle_is_not_taken() {
    let pending = NextStep::Delegate { bundle_index: 1 };
    assert_eq!(
        next_dispatches(&steps(), &[1], std::slice::from_ref(&pending), 1, 1, true),
        vec![NextStep::Delegate { bundle_index: 0 }]
    );
}

#[test]
fn bundle_steps_form_a_bounded_plan_ordered_wave() {
    assert_eq!(
        next_dispatches(&steps(), &[], &[], 2, 2, true),
        vec![
            NextStep::Delegate { bundle_index: 0 },
            NextStep::Delegate { bundle_index: 1 },
        ]
    );
    assert_eq!(
        next_dispatches(&steps(), &[], &[], 3, 1, true),
        vec![NextStep::Delegate { bundle_index: 0 }],
        "the dispatch budget caps the wave"
    );
}

#[test]
fn a_round_step_stops_the_bundle_wave() {
    let steps = vec![
        NextStep::Delegate { bundle_index: 0 },
        NextStep::AdvanceImportedDelegation { bundle_index: 1 },
        NextStep::Delegate { bundle_index: 2 },
    ];
    assert_eq!(
        next_dispatches(&steps, &[], &[], 3, 3, true),
        vec![NextStep::Delegate { bundle_index: 0 }]
    );
}

#[test]
fn stop_round_policy_selects_only_one_bundle_step() {
    assert_eq!(
        next_dispatches(&steps(), &[], &[], 3, 3, false),
        vec![NextStep::Delegate { bundle_index: 0 }]
    );
}

#[test]
fn multiple_repolls_lead_the_next_wave() {
    let preferred = vec![
        NextStep::Delegate { bundle_index: 1 },
        NextStep::Delegate { bundle_index: 0 },
    ];
    assert_eq!(
        next_dispatches(&steps(), &[], &preferred, 2, 2, true),
        preferred
    );
}

/// Every `NextStep` variant, so a new one is a compile error here too.
fn every_step_variant() -> Vec<NextStep> {
    vec![
        NextStep::Delegate { bundle_index: 7 },
        NextStep::AdvanceDelegation { bundle_index: 7 },
        NextStep::AdvanceImportedDelegation { bundle_index: 7 },
        NextStep::CastVote {
            bundle_index: 7,
            proposal_id: 1,
            choice: 0,
        },
        NextStep::AdvanceVote {
            bundle_index: 7,
            proposal_id: 1,
        },
        NextStep::AdvanceVoteBatch {
            bundle_index: 7,
            proposal_id: 1,
        },
        NextStep::SubmitShares {
            bundle_index: 7,
            proposal_id: 1,
            share_index: 0,
        },
        NextStep::ConfirmShare {
            bundle_index: 7,
            proposal_id: 1,
            share_index: 0,
        },
    ]
}

#[test]
fn the_driver_schedules_by_the_executors_own_lock_scope() {
    // Derived from `round_lock::bundle_scope`, not restated: if scheduling and
    // locking were two tables, a drift between them would admit a wave of
    // steps that then serialize on one lock.
    for step in every_step_variant() {
        let executor_scope = crate::vote_work::round_lock::bundle_scope(&step);
        let expected = match executor_scope {
            Some(_) => StepLockScope::Bundle,
            None => StepLockScope::Round,
        };
        assert_eq!(lock_scope(&step), expected, "{step:?}");
        // A wave admits one step per `bundle_index`, so that must be the very
        // bundle the executor locks, or two admitted steps could contend.
        if let Some(locked_bundle) = executor_scope {
            assert_eq!(locked_bundle, bundle_index(&step), "{step:?}");
        }
    }
}

#[test]
fn only_delegation_proving_is_bundle_scoped() {
    // Pins which variants overlap, so widening or narrowing concurrency is a
    // deliberate edit to `round_lock::bundle_scope` and shows up here.
    let bundle_scoped: Vec<_> = every_step_variant()
        .into_iter()
        .filter(|step| lock_scope(step) == StepLockScope::Bundle)
        .collect();
    assert_eq!(
        bundle_scoped,
        vec![
            NextStep::Delegate { bundle_index: 7 },
            NextStep::AdvanceDelegation { bundle_index: 7 },
        ]
    );
}
