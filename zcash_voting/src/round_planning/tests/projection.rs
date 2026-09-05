//! Steps are a projection of obligations and resolve back to them.

use super::fixtures::*;
use crate::phases::{DelegationPhase, SharePhase, VotePhase};
use crate::round_planning::{classify_round, plan_from_snapshot, resolve_step, Obligation};
use crate::session::{Decision, NextStep};

#[test]
fn every_projected_step_resolves_to_the_obligation_it_came_from() {
    let snapshot = snapshot()
        .bundle(0, DelegationPhase::Proved)
        .bundle(1, DelegationPhase::Submitted)
        .bundle(2, DelegationPhase::Confirmed)
        .batch(2, &[(1, 0), (2, 1)], VotePhase::Submitted)
        .vote(2, 3, 0, VotePhase::Confirmed)
        .share(2, 3, 0, SharePhase::Submitted, false)
        .intent(1, Decision::Choice(0))
        .intent(2, Decision::Choice(1))
        .intent(3, Decision::Choice(0))
        .build();
    let obligations = classify_round(&snapshot, &[1, 2, 3]).unwrap();
    let plan = plan_from_snapshot(&snapshot, &[1, 2, 3]).unwrap();
    assert!(!plan.next_steps.is_empty());
    for step in &plan.next_steps {
        let obligation = resolve_step(&obligations.obligations, step)
            .unwrap_or_else(|| panic!("{step:?} has no obligation"));
        let matches = matches!(
            (step, obligation),
            (NextStep::Delegate { .. }, Obligation::Delegate { .. })
                | (
                    NextStep::AdvanceDelegation { .. },
                    Obligation::AdvanceDelegation { .. }
                )
                | (NextStep::CastVote { .. }, Obligation::Cast { .. })
                | (
                    NextStep::AdvanceVoteBatch { .. },
                    Obligation::ReconcileChain { .. }
                )
                | (NextStep::SubmitShares { .. }, Obligation::Deliver { .. })
                | (NextStep::ConfirmShare { .. }, Obligation::Confirm { .. })
        );
        assert!(matches, "{step:?} resolved to {obligation:?}");
    }
    assert!(
        resolve_step(
            &obligations.obligations,
            &NextStep::CastVote {
                bundle_index: 0,
                proposal_id: 1,
                choice: 2
            }
        )
        .is_none(),
        "a step with another choice is not this plan's work"
    );
}

#[test]
fn a_cast_step_for_one_proposal_resolves_to_the_bundles_whole_draft_set() {
    let snapshot = snapshot()
        .bundle(0, DelegationPhase::Confirmed)
        .intent(1, Decision::Choice(0))
        .intent(2, Decision::Choice(1))
        .build();
    let obligations = classify_round(&snapshot, &[1, 2]).unwrap();
    let step = NextStep::CastVote {
        bundle_index: 0,
        proposal_id: 2,
        choice: 1,
    };
    let Some(Obligation::Cast { drafts, .. }) = resolve_step(&obligations.obligations, &step)
    else {
        panic!("a cast step resolves to its cast");
    };
    assert_eq!(drafts.len(), 2);
}
