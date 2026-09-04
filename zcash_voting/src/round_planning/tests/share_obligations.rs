//! Deliver versus confirm: a share no helper accepted is delivered from the
//! durable plan, an accepted one is polled.

use super::fixtures::*;
use crate::phases::{DelegationPhase, SharePhase, VotePhase};
use crate::round_planning::{classify_round, Obligation};
use crate::session::Decision;

#[test]
fn an_unaccepted_submitted_share_is_a_blocking_confirm_and_an_accepted_one_is_not() {
    let snapshot = snapshot()
        .bundle(0, DelegationPhase::Confirmed)
        .vote(0, 1, 2, VotePhase::Confirmed)
        .share(0, 1, 0, SharePhase::Submitted, false)
        .share(0, 1, 1, SharePhase::Submitted, true)
        .intent(1, Decision::Choice(2))
        .build();
    let obligations = classify_round(&snapshot, &[1]).unwrap().obligations;
    let confirms = obligations
        .iter()
        .filter_map(|obligation| match obligation {
            Obligation::Confirm {
                share_index,
                accepted,
                ..
            } => Some((*share_index, *accepted)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(confirms, vec![(0, false), (1, true)]);
    assert!(
        !obligations
            .iter()
            .any(|obligation| matches!(obligation, Obligation::Deliver { .. })),
        "every share has a row, so nothing is owed as delivery"
    );
}

#[test]
fn a_delivery_carries_the_confirmed_tree_position_the_executor_needs() {
    let snapshot = snapshot()
        .bundle(0, DelegationPhase::Confirmed)
        .vote(0, 1, 2, VotePhase::Confirmed)
        .build();
    let obligations = classify_round(&snapshot, &[1]).unwrap().obligations;
    let positions = obligations
        .iter()
        .filter_map(|obligation| match obligation {
            Obligation::Deliver {
                vc_tree_position, ..
            } => Some(*vc_tree_position),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(positions, vec![7]);
}

#[test]
fn shares_of_a_stale_vote_are_not_confirmed() {
    let snapshot = snapshot()
        .bundle(0, DelegationPhase::Confirmed)
        .vote(0, 1, 2, VotePhase::Prepared)
        .share(0, 1, 0, SharePhase::Submitted, true)
        .intent(1, Decision::Choice(0))
        .build();
    let obligations = classify_round(&snapshot, &[1]).unwrap().obligations;
    assert!(!obligations
        .iter()
        .any(|obligation| matches!(obligation, Obligation::Confirm { .. })));
}
