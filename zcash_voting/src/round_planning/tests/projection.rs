//! Steps are a projection of obligations and resolve back to them.

use super::fixtures::*;
use crate::phases::{DelegationPhase, SharePhase, VotePhase};
use crate::round_planning::{classify_round, plan_from_snapshot, resolve_step, Obligation};
use crate::session::{CompletedVoteChoice, Decision, NextStep, RoundPlanAction};

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
    assert_eq!(
        drafts
            .iter()
            .map(|draft| draft.proposal_id)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[test]
fn a_blocked_bundle_does_not_hide_a_sibling_bundles_completed_vote() {
    // Bundle 0 has exhausted its recasting and needs a decision; bundle 1's
    // vote is confirmed with its share delivered. Suppressing the completed
    // view round-wide told the voter nothing except that something, somewhere,
    // was unresolved — about a vote that is already on chain.
    let snapshot = snapshot()
        .bundle(0, DelegationPhase::Proved)
        .bundle(1, DelegationPhase::Confirmed)
        .rejection_blocked(0)
        .vote(1, 1, 0, VotePhase::Confirmed)
        .share(1, 1, 0, SharePhase::Confirmed, false)
        .share(1, 1, 1, SharePhase::Confirmed, false)
        .intent(1, Decision::Choice(0))
        .build();

    let plan = plan_from_snapshot(&snapshot, &[1]).unwrap();

    assert!(
        plan.completed_vote_display.is_some(),
        "the confirmed sibling's vote is still shown; steps={:?}",
        plan.next_steps
    );
    // The block is still the round's business: it is not `Done`, and the
    // chain's own words about the refusal survive on the blocked bundle.
    assert!(!plan.completed_for_display);
    // A held bundle emits no steps, so the action a host sees is `Idle`, not a
    // recovery action. `blocking_recovery` and the bundle's own diagnostic are
    // what carry the block; the action alone does not distinguish this round
    // from an untouched one.
    assert_eq!(plan.primary_action, RoundPlanAction::Idle);
    assert!(plan.blocking_recovery);
    assert!(plan
        .delegation_statuses
        .iter()
        .find(|status| status.bundle_index == 0)
        .and_then(|status| status.submission_diagnostic.as_ref())
        .is_some_and(|diagnostic| diagnostic.message().contains("code 7")));
}

#[test]
fn a_blocked_bundles_own_choice_never_reaches_the_completed_view() {
    // Bundle 0 is blocked having stored choice 0; bundle 1 confirmed choice 1
    // for the same proposal. Reading both would either report a vote bundle 0
    // never cast, or disagree and render the proposal undecided inside a
    // display headed "completed".
    let snapshot = snapshot()
        .bundle(0, DelegationPhase::Proved)
        .bundle(1, DelegationPhase::Confirmed)
        .rejection_blocked(0)
        .vote(0, 1, 0, VotePhase::Prepared)
        .vote(1, 1, 1, VotePhase::Confirmed)
        .share(1, 1, 0, SharePhase::Confirmed, false)
        .share(1, 1, 1, SharePhase::Confirmed, false)
        .intent(1, Decision::Choice(1))
        .build();

    let plan = plan_from_snapshot(&snapshot, &[1]).unwrap();

    assert_eq!(
        plan.completed_vote_display
            .as_ref()
            .map(|display| display.choices.as_slice()),
        Some(
            [CompletedVoteChoice {
                proposal_id: 1,
                choice: Some(1),
            }]
            .as_slice()
        ),
        "only the bundle that actually cast is read"
    );
}

#[test]
fn a_proposal_whose_only_cast_is_blocked_withholds_the_whole_view() {
    // Bundle 1 confirmed proposal 1; proposal 2's only cast sits in blocked
    // bundle 0. Rendering proposal 2 as `None` inside a completed view is
    // indistinguishable from the voter having skipped it, so the view waits.
    let snapshot = snapshot()
        .bundle(0, DelegationPhase::Proved)
        .bundle(1, DelegationPhase::Confirmed)
        .rejection_blocked(0)
        .vote(1, 1, 0, VotePhase::Confirmed)
        .share(1, 1, 0, SharePhase::Confirmed, false)
        .share(1, 1, 1, SharePhase::Confirmed, false)
        .vote(0, 2, 1, VotePhase::Prepared)
        .intent(1, Decision::Choice(0))
        .intent(2, Decision::Choice(1))
        .build();

    let plan = plan_from_snapshot(&snapshot, &[1, 2]).unwrap();

    assert_eq!(
        plan.completed_vote_display, None,
        "proposal 2's answer is unknown until the block is resolved"
    );
}

#[test]
fn a_blocked_bundle_does_not_stamp_the_display_with_its_own_timestamp() {
    // The blocked bundle delivered shares during an earlier attempt. "Voted at"
    // must name a moment belonging to the vote being shown, not to one that
    // never landed.
    let snapshot = snapshot()
        .bundle(0, DelegationPhase::Proved)
        .bundle(1, DelegationPhase::Confirmed)
        .rejection_blocked(0)
        .vote(1, 1, 0, VotePhase::Confirmed)
        .share(1, 1, 0, SharePhase::Confirmed, false)
        .share(1, 1, 1, SharePhase::Confirmed, false)
        .share(0, 1, 0, SharePhase::Confirmed, false)
        .intent(1, Decision::Choice(0))
        .build();

    let plan = plan_from_snapshot(&snapshot, &[1]).unwrap();

    let voted_at = plan
        .completed_vote_display
        .as_ref()
        .and_then(|display| display.voted_at);
    let blocked_bundle_share_times: Vec<u64> = snapshot
        .shares
        .iter()
        .filter(|share| share.bundle_index == 0)
        .map(|share| share.created_at)
        .collect();
    assert!(
        voted_at.is_none_or(|at| !blocked_bundle_share_times.contains(&at)),
        "voted_at {voted_at:?} came from the blocked bundle"
    );
}

#[test]
fn a_lone_blocked_bundle_shows_no_completed_vote() {
    // The blocked bundle's own artifact is still withheld: its batch was
    // refused, so there is nothing to tell the voter has landed.
    let snapshot = snapshot()
        .bundle(0, DelegationPhase::Proved)
        .rejection_blocked(0)
        .vote(0, 1, 0, VotePhase::Prepared)
        .intent(1, Decision::Choice(0))
        .build();

    let plan = plan_from_snapshot(&snapshot, &[1]).unwrap();

    assert_eq!(plan.completed_vote_display, None);
    assert!(!plan.completed_for_display);
    assert_ne!(plan.primary_action, RoundPlanAction::Done);
}
