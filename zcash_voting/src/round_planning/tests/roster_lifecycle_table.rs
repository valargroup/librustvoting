//! The rule table: lifecycle position x roster relation x ballot relation
//! yields exactly the stated obligation, and nothing acts on part of a batch.

use super::fixtures::*;
use crate::phases::{DelegationPhase, VotePhase};
use crate::round_planning::{classify_round, Obligation, VoteUnitId};
use crate::session::Decision;

fn classified(snapshot: &crate::round_planning::RoundSnapshot, roster: &[u32]) -> Vec<Obligation> {
    classify_round(snapshot, roster).unwrap().obligations
}

fn reconciles(obligations: &[Obligation]) -> Vec<VoteUnitId> {
    obligations
        .iter()
        .filter_map(|obligation| match obligation {
            Obligation::ReconcileChain { unit, .. } => Some(*unit),
            _ => None,
        })
        .collect()
}

fn retires(obligations: &[Obligation]) -> Vec<(VoteUnitId, Vec<u32>)> {
    obligations
        .iter()
        .filter_map(|obligation| match obligation {
            Obligation::Retire { unit, members } => Some((*unit, members.clone())),
            _ => None,
        })
        .collect()
}

fn casts(obligations: &[Obligation]) -> Vec<(u32, Vec<(u32, u32)>)> {
    obligations
        .iter()
        .filter_map(|obligation| match obligation {
            Obligation::Cast {
                bundle_index,
                drafts,
                ..
            } => Some((
                *bundle_index,
                drafts
                    .iter()
                    .map(|draft| (draft.proposal_id, draft.choice))
                    .collect(),
            )),
            _ => None,
        })
        .collect()
}

fn delivers(obligations: &[Obligation]) -> Vec<(u32, u32, Vec<u32>)> {
    obligations
        .iter()
        .filter_map(|obligation| match obligation {
            Obligation::Deliver {
                bundle_index,
                proposal_id,
                share_indexes,
                ..
            } => Some((*bundle_index, *proposal_id, share_indexes.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn an_undispatched_rostered_vote_the_ballot_agrees_with_is_reconciled() {
    let snapshot = snapshot()
        .bundle(0, DelegationPhase::Confirmed)
        .vote(0, 1, 2, VotePhase::Committed)
        .intent(1, Decision::Choice(2))
        .build();
    let obligations = classified(&snapshot, &[1]);
    assert_eq!(
        reconciles(&obligations),
        vec![VoteUnitId::Singleton {
            bundle_index: 0,
            proposal_id: 1
        }]
    );
    assert!(casts(&obligations).is_empty());
    assert!(retires(&obligations).is_empty());
}

#[test]
fn an_undispatched_rostered_vote_without_an_intent_holds_its_bundle_and_plans_nothing() {
    let snapshot = snapshot()
        .bundle(0, DelegationPhase::Confirmed)
        .vote(0, 1, 2, VotePhase::Committed)
        .intent(2, Decision::Choice(0))
        .build();
    let obligations = classified(&snapshot, &[1, 2]);
    assert!(reconciles(&obligations).is_empty());
    assert!(
        casts(&obligations).is_empty(),
        "proposal 2 is due but the bundle is held: {obligations:?}"
    );
    assert!(!obligations
        .iter()
        .any(|obligation| matches!(obligation, Obligation::Blocked { .. })));
}

#[test]
fn an_undispatched_singleton_the_ballot_conflicts_with_is_superseded_by_a_cast() {
    let snapshot = snapshot()
        .bundle(0, DelegationPhase::Confirmed)
        .vote(0, 1, 2, VotePhase::Committed)
        .intent(1, Decision::Choice(0))
        .build();
    let obligations = classified(&snapshot, &[1]);
    assert_eq!(casts(&obligations), vec![(0, vec![(1, 0)])]);
    assert!(reconciles(&obligations).is_empty());
}

#[test]
fn an_undispatched_unit_with_a_departed_member_is_retired_whole_and_the_rest_recast() {
    let departed = snapshot()
        .bundle(0, DelegationPhase::Confirmed)
        .batch(0, &[(1, 0), (2, 1)], VotePhase::Committed)
        .intent(1, Decision::Choice(0))
        .build();
    let obligations = classified(&departed, &[1]);
    let retired = retires(&obligations);
    assert_eq!(retired.len(), 1);
    assert!(matches!(
        retired[0].0,
        VoteUnitId::Batch {
            bundle_index: 0,
            ..
        }
    ));
    assert_eq!(
        retired[0].1,
        vec![1, 2],
        "every member of the batch is retired"
    );
    assert_eq!(casts(&obligations), vec![(0, vec![(1, 0)])]);
    assert!(reconciles(&obligations).is_empty());
    // While the departed member's intent is still recorded it is the host's
    // to clear, so the recast waits; the retirement does not.
    let with_intent = snapshot()
        .bundle(0, DelegationPhase::Confirmed)
        .batch(0, &[(1, 0), (2, 1)], VotePhase::Committed)
        .intent(1, Decision::Choice(0))
        .intent(2, Decision::Choice(1))
        .build();
    let obligations = classified(&with_intent, &[1]);
    assert_eq!(retires(&obligations).len(), 1);
    assert!(casts(&obligations).is_empty());
    assert!(obligations.iter().any(|obligation| matches!(
        obligation,
        Obligation::Blocked {
            bundle_index: 0,
            ..
        }
    )));
}

#[test]
fn an_on_wire_vote_survives_leaving_the_roster_and_losing_its_intent() {
    for phase in [VotePhase::Submitted, VotePhase::SubmissionManaged] {
        let unrostered = snapshot()
            .bundle(0, DelegationPhase::Confirmed)
            .vote(0, 1, 2, phase)
            .intent(1, Decision::Choice(2))
            .build();
        let obligations = classified(&unrostered, &[2]);
        assert_eq!(
            reconciles(&obligations).len(),
            1,
            "{phase:?}: {obligations:?}"
        );

        let intentless = snapshot()
            .bundle(0, DelegationPhase::Confirmed)
            .vote(0, 1, 2, phase)
            .build();
        let obligations = classified(&intentless, &[1]);
        assert_eq!(
            reconciles(&obligations).len(),
            1,
            "{phase:?}: {obligations:?}"
        );
    }
}

#[test]
fn a_batch_on_the_wire_is_one_reconcile_obligation_naming_every_member() {
    let snapshot = snapshot()
        .bundle(0, DelegationPhase::Confirmed)
        .batch(0, &[(1, 0), (2, 1)], VotePhase::Submitted)
        .intent(1, Decision::Choice(0))
        .build();
    let obligations = classified(&snapshot, &[1, 2]);
    let members = obligations
        .iter()
        .filter_map(|obligation| match obligation {
            Obligation::ReconcileChain {
                ordered_proposal_ids,
                ..
            } => Some(ordered_proposal_ids.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(members, vec![vec![1, 2]]);
}

#[test]
fn a_conflicting_intent_on_anything_past_committed_is_an_invariant_violation() {
    for phase in [
        VotePhase::Submitted,
        VotePhase::SubmissionManaged,
        VotePhase::SubmittedWithoutHash,
        VotePhase::SubmissionRejected,
        VotePhase::Confirmed,
    ] {
        let snapshot = snapshot()
            .bundle(0, DelegationPhase::Confirmed)
            .vote(0, 1, 2, phase)
            .intent(1, Decision::Skipped)
            .build();
        let error = classify_round(&snapshot, &[1]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("has a submitted vote that conflicts with ballot intent"),
            "{phase:?}: {error}"
        );
    }
}

#[test]
fn a_terminal_vote_plans_nothing_and_only_a_hashless_one_holds_the_bundle() {
    let hashless = snapshot()
        .bundle(0, DelegationPhase::Confirmed)
        .vote(0, 1, 2, VotePhase::SubmittedWithoutHash)
        .intent(1, Decision::Choice(2))
        .intent(2, Decision::Choice(0))
        .build();
    let obligations = classified(&hashless, &[1, 2]);
    assert!(reconciles(&obligations).is_empty());
    assert!(casts(&obligations).is_empty(), "{obligations:?}");

    let rejected = snapshot()
        .bundle(0, DelegationPhase::Confirmed)
        .vote(0, 1, 2, VotePhase::SubmissionRejected)
        .intent(1, Decision::Choice(2))
        .intent(2, Decision::Choice(0))
        .build();
    let obligations = classified(&rejected, &[1, 2]);
    assert!(reconciles(&obligations).is_empty());
    assert_eq!(
        casts(&obligations),
        vec![(0, vec![(2, 0)])],
        "a rejected vote spent nothing and reserves nothing"
    );
}

#[test]
fn a_confirmed_vote_owes_its_missing_shares_whatever_the_roster_says() {
    let unrostered = snapshot()
        .bundle(0, DelegationPhase::Confirmed)
        .vote(0, 1, 2, VotePhase::Confirmed)
        .intent(1, Decision::Choice(2))
        .build();
    assert_eq!(
        delivers(&classified(&unrostered, &[2])),
        vec![(0, 1, vec![0, 1])]
    );

    let partly_recorded = snapshot()
        .bundle(0, DelegationPhase::Confirmed)
        .vote(0, 1, 2, VotePhase::Confirmed)
        .share(0, 1, 0, crate::phases::SharePhase::Confirmed, true)
        .build();
    assert_eq!(
        delivers(&classified(&partly_recorded, &[1])),
        vec![(0, 1, vec![1])]
    );
}

#[test]
fn every_vote_phase_has_one_lifecycle_position() {
    use crate::round_planning::LifecyclePosition;
    let positions = [
        (None, LifecyclePosition::Uncast),
        (Some(VotePhase::Prepared), LifecyclePosition::Uncast),
        (Some(VotePhase::Committed), LifecyclePosition::Undispatched),
        (Some(VotePhase::Submitted), LifecyclePosition::OnWire),
        (
            Some(VotePhase::SubmissionManaged),
            LifecyclePosition::OnWire,
        ),
        (
            Some(VotePhase::SubmittedWithoutHash),
            LifecyclePosition::Terminal,
        ),
        (
            Some(VotePhase::SubmissionRejected),
            LifecyclePosition::Terminal,
        ),
        (Some(VotePhase::Confirmed), LifecyclePosition::Confirmed),
    ];
    for (phase, expected) in positions {
        assert_eq!(LifecyclePosition::of(phase), expected, "{phase:?}");
        assert_eq!(
            expected.is_lifecycle_owned(),
            matches!(
                expected,
                LifecyclePosition::OnWire
                    | LifecyclePosition::Terminal
                    | LifecyclePosition::Confirmed
            )
        );
    }
}
