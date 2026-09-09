//! When a fresh cast is planned, blocked, or silently held.

use super::fixtures::*;
use crate::phases::{DelegationPhase, VotePhase};
use crate::round_planning::{classify_round, BlockedReason, Obligation};
use crate::session::Decision;

fn blocked(obligations: &[Obligation]) -> Vec<(u32, BlockedReason)> {
    obligations
        .iter()
        .filter_map(|obligation| match obligation {
            Obligation::Blocked {
                bundle_index,
                reason,
            } => Some((*bundle_index, reason.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn an_open_proposal_blocks_the_cast_but_still_plans_the_delegation_prerequisite() {
    let snapshot = snapshot()
        .bundle(0, DelegationPhase::Prepared)
        .intent(1, Decision::Choice(0))
        .build();
    let obligations = classify_round(&snapshot, &[1, 2]).unwrap();
    assert_eq!(
        blocked(&obligations.obligations),
        vec![(0, BlockedReason::OpenBallot(vec![2]))]
    );
    assert!(obligations
        .obligations
        .iter()
        .any(|obligation| matches!(obligation, Obligation::Delegate { bundle_index: 0 })));
    assert!(!obligations
        .obligations
        .iter()
        .any(|obligation| matches!(obligation, Obligation::Cast { .. })));
}

#[test]
fn a_clearable_unrostered_intent_blocks_the_cast_and_a_lifecycle_owned_one_does_not() {
    let clearable = snapshot()
        .bundle(0, DelegationPhase::Confirmed)
        .intent(1, Decision::Choice(0))
        .intent(9, Decision::Choice(0))
        .build();
    let obligations = classify_round(&clearable, &[1]).unwrap();
    assert_eq!(obligations.unrostered_intents, vec![9]);
    assert_eq!(
        blocked(&obligations.obligations),
        vec![(0, BlockedReason::UnrosteredIntents(vec![9]))]
    );

    // The same intent whose vote is on the wire on another bundle is not the
    // host's to clear: it neither blocks the cast on the free bundle nor is
    // reported. Its own bundle stays held by the on-wire vote.
    let owned = snapshot()
        .bundle(0, DelegationPhase::Confirmed)
        .bundle(1, DelegationPhase::Confirmed)
        .vote(1, 9, 0, VotePhase::Submitted)
        .intent(1, Decision::Choice(0))
        .intent(9, Decision::Choice(0))
        .build();
    let obligations = classify_round(&owned, &[1]).unwrap();
    assert!(obligations.unrostered_intents.is_empty());
    assert!(blocked(&obligations.obligations).is_empty());
    let cast_bundles = obligations
        .obligations
        .iter()
        .filter_map(|obligation| match obligation {
            Obligation::Cast { bundle_index, .. } => Some(*bundle_index),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(cast_bundles, vec![0]);
    assert!(obligations.obligations.iter().any(|obligation| matches!(
        obligation,
        Obligation::ReconcileChain {
            bundle_index: 1,
            ..
        }
    )));
}

#[test]
fn a_bundle_held_by_a_managed_or_terminal_delegation_plans_no_cast_at_all() {
    for phase in [
        DelegationPhase::SubmissionManaged,
        DelegationPhase::SubmittedWithoutHash,
        DelegationPhase::SubmissionRejected,
    ] {
        let snapshot = snapshot()
            .bundle(0, phase)
            .intent(1, Decision::Choice(0))
            .build();
        let obligations = classify_round(&snapshot, &[1]).unwrap().obligations;
        assert!(
            !obligations.iter().any(|obligation| matches!(
                obligation,
                Obligation::Cast { .. } | Obligation::Blocked { .. } | Obligation::Delegate { .. }
            )),
            "{phase:?}: {obligations:?}"
        );
    }
}

#[test]
fn only_a_cast_that_signs_its_own_delegation_owes_the_voter_key() {
    // The signing preflight reads this flag instead of re-deriving the answer
    // from the delegation phase and the imported flag, so planning is the one
    // place that decides it. A fresh combined cast signs its delegation; an
    // already-confirmed one has nothing left to sign; and an imported
    // capability is on the chain already while this wallet holds no delegation
    // key to offer, whether or not the plan also carries a step to advance it.
    let snapshot = snapshot()
        .bundle(0, DelegationPhase::Proved)
        .bundle(1, DelegationPhase::Confirmed)
        .imported_bundle(2, DelegationPhase::Submitted)
        .imported_bundle(3, DelegationPhase::Confirmed)
        .intent(1, Decision::Choice(0))
        .build();
    let obligations = classify_round(&snapshot, &[1]).unwrap().obligations;

    let signing = obligations
        .iter()
        .filter_map(|obligation| match obligation {
            Obligation::Cast {
                bundle_index,
                signs_delegation,
                ..
            } => Some((*bundle_index, *signs_delegation)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        signing,
        vec![(0, true), (1, false), (2, false), (3, false)],
        "only the fresh locally prepared delegation is signed by its cast"
    );
}

#[test]
fn every_draft_of_a_bundle_is_one_cast_obligation_with_its_delegation_prerequisite() {
    let snapshot = snapshot()
        .bundle(0, DelegationPhase::Proved)
        .bundle(1, DelegationPhase::Confirmed)
        .intent(1, Decision::Choice(0))
        .intent(2, Decision::Choice(1))
        .intent(3, Decision::Skipped)
        .build();
    let obligations = classify_round(&snapshot, &[1, 2, 3]).unwrap().obligations;
    let casts = obligations
        .iter()
        .filter_map(|obligation| match obligation {
            Obligation::Cast {
                bundle_index,
                drafts,
                prerequisite,
                ..
            } => Some((*bundle_index, drafts.len(), *prerequisite)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(casts, vec![(0, 2, None), (1, 2, None)]);
}

#[test]
fn no_choices_produce_no_cast_and_one_choice_remains_a_singleton() {
    for decision in [Decision::Skipped, Decision::Choice(0)] {
        let snapshot = snapshot()
            .bundle(0, DelegationPhase::Confirmed)
            .intent(1, decision)
            .intent(2, Decision::Skipped)
            .build();
        let obligations = classify_round(&snapshot, &[1, 2]).unwrap();
        let drafts = obligations
            .obligations
            .iter()
            .filter_map(|obligation| match obligation {
                Obligation::Cast { drafts, .. } => Some(drafts),
                _ => None,
            })
            .collect::<Vec<_>>();
        match decision {
            Decision::Skipped => assert!(drafts.is_empty()),
            Decision::Choice(_) => {
                assert_eq!(drafts.len(), 1);
                assert_eq!(drafts[0].len(), 1);
                assert_eq!(drafts[0][0].proposal_id, 1);
            }
        }
    }
}
