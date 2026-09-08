//! What a run keeps from a pass that did not finish.
//!
//! A pass walks a round's unconfirmed shares and commits each confirmation and
//! retained recovery attempt as it reaches it, so an error means the walk
//! stopped, not that nothing happened. Those effects are also unrepeatable: the
//! next pass walks only unconfirmed shares, so a share this one confirmed is
//! never seen again and would be missing from the run's report altogether.
//!
//! A cancelled pass is partial for the same reason a failed one is, and the
//! run folds it the same way — both stopped somewhere inside the walk.

use crate::share_tracking::{ResubmittedShare, ShareKey, ShareTrackingReport};
use crate::share_tracking_drive::run_ledger::Run;
use crate::share_tracking_drive::ShareTrackingQuiescence;

fn share(share_index: u32) -> ShareKey {
    ShareKey {
        bundle_index: 0,
        proposal_id: 1,
        share_index,
    }
}

fn resubmitted(share_index: u32) -> ResubmittedShare {
    resubmitted_to(share_index, "https://helper.example")
}

fn resubmitted_to(share_index: u32, server_url: &str) -> ResubmittedShare {
    ResubmittedShare {
        share: share(share_index),
        server_url: server_url.to_string(),
    }
}

#[test]
fn a_partial_pass_still_hands_over_what_it_committed() {
    let mut run = Run::default();

    run.absorb_partial(&ShareTrackingReport {
        confirmed: vec![share(0)],
        resubmitted: vec![resubmitted(1)],
        ambiguous: vec![resubmitted(2)],
        ..ShareTrackingReport::default()
    });
    let report = run.finish(ShareTrackingQuiescence::Cancelled);

    assert_eq!(report.confirmed, vec![share(0)]);
    assert_eq!(report.resubmitted, vec![resubmitted(1)]);
    assert_eq!(report.ambiguous, vec![resubmitted(2)]);
}

#[test]
fn a_partial_pass_does_not_narrow_the_unrecoverable_set() {
    // `unrecoverable` is a statement about the whole round, taken from the
    // last pass that saw every share. A partial walk saw a prefix, so letting
    // it replace the set would drop shares whose material is still missing.
    let mut run = Run::default();

    run.absorb(&ShareTrackingReport {
        unrecoverable: vec![share(3), share(4)],
        ..ShareTrackingReport::default()
    });
    run.absorb_partial(&ShareTrackingReport {
        confirmed: vec![share(0)],
        ..ShareTrackingReport::default()
    });
    let report = run.finish(ShareTrackingQuiescence::Cancelled);

    assert_eq!(report.unrecoverable, vec![share(3), share(4)]);
    assert_eq!(report.confirmed, vec![share(0)]);
}

#[test]
fn a_complete_pass_replaces_the_unrecoverable_set() {
    // The other half of the same rule: a share can stop being unrecoverable
    // once its material is restored, and a host shown a stale union would keep
    // warning about a share that recovered.
    let mut run = Run::default();

    run.absorb(&ShareTrackingReport {
        unrecoverable: vec![share(3), share(4)],
        ..ShareTrackingReport::default()
    });
    run.absorb(&ShareTrackingReport {
        unrecoverable: vec![share(4)],
        ..ShareTrackingReport::default()
    });
    let report = run.finish(ShareTrackingQuiescence::Cancelled);

    assert_eq!(report.unrecoverable, vec![share(4)]);
}

#[test]
fn a_partial_pass_drops_the_shares_it_recovered_from_the_snapshot() {
    // Kept is not frozen. A share this pass confirmed or reached a helper with
    // is no longer beyond repair, whatever an earlier pass concluded, and
    // leaving it in would let one report call a share both recovered and
    // unrecoverable — `PassBudgetExhausted` could even name a confirmed share.
    let mut run = Run::default();

    run.absorb(&ShareTrackingReport {
        unrecoverable: vec![share(3), share(4), share(5)],
        ..ShareTrackingReport::default()
    });
    run.absorb_partial(&ShareTrackingReport {
        confirmed: vec![share(3)],
        resubmitted: vec![resubmitted(4)],
        ..ShareTrackingReport::default()
    });
    let report = run.finish(ShareTrackingQuiescence::Cancelled);

    assert_eq!(
        report.unrecoverable,
        vec![share(5)],
        "only the share this pass said nothing about survives",
    );
}

#[test]
fn an_ambiguous_recovery_attempt_also_clears_a_share() {
    // Recovery could only send a share it still had the material for, so an
    // attempt whose acceptance is unknown still disproves "beyond repair".
    let mut run = Run::default();

    run.absorb(&ShareTrackingReport {
        unrecoverable: vec![share(6)],
        ..ShareTrackingReport::default()
    });
    run.absorb_partial(&ShareTrackingReport {
        ambiguous: vec![resubmitted(6)],
        ..ShareTrackingReport::default()
    });

    assert!(run
        .finish(ShareTrackingQuiescence::Cancelled)
        .unrecoverable
        .is_empty());
}

#[test]
fn an_ambiguous_attempt_settled_by_a_later_pass_leaves_the_run_report() {
    // A pass records an attempt as ambiguous when it could not tell whether
    // the helper took the share. A later pass retrying that same helper can be
    // told plainly. Keeping both would leave the run's `ambiguous` claiming an
    // unknown outcome that is now known.
    let mut run = Run::default();

    run.absorb(&ShareTrackingReport {
        ambiguous: vec![resubmitted(1)],
        ..ShareTrackingReport::default()
    });
    run.absorb(&ShareTrackingReport {
        resubmitted: vec![resubmitted(1)],
        ..ShareTrackingReport::default()
    });
    let report = run.finish(ShareTrackingQuiescence::AllConfirmed);

    assert!(
        report.ambiguous.is_empty(),
        "the attempt's outcome is known now, got {:?}",
        report.ambiguous,
    );
    assert_eq!(report.resubmitted, vec![resubmitted(1)]);
}

#[test]
fn a_settled_attempt_at_another_helper_leaves_the_ambiguity_alone() {
    // Ambiguity is per attempt, not per share: reaching helper B says nothing
    // about whether helper A took the share.
    let mut run = Run::default();

    run.absorb(&ShareTrackingReport {
        ambiguous: vec![resubmitted_to(1, "https://helper-a.example")],
        ..ShareTrackingReport::default()
    });
    run.absorb(&ShareTrackingReport {
        resubmitted: vec![resubmitted_to(1, "https://helper-b.example")],
        ..ShareTrackingReport::default()
    });

    assert_eq!(
        run.finish(ShareTrackingQuiescence::AllConfirmed).ambiguous,
        vec![resubmitted_to(1, "https://helper-a.example")],
    );
}

#[test]
fn a_helper_reached_again_on_a_later_pass_is_recorded_once() {
    // Recovery re-sends an overdue share to helpers that already accepted it —
    // duplicate-safe, and the point of it. A run bounded only by vote end does
    // that for days, so appending each recurrence would grow the report
    // without bound and present every repetition as a helper newly reached.
    let mut run = Run::default();

    for _ in 0..4 {
        run.absorb(&ShareTrackingReport {
            resubmitted: vec![resubmitted(1)],
            ..ShareTrackingReport::default()
        });
    }
    let report = run.finish(ShareTrackingQuiescence::VoteEndReached);

    assert_eq!(report.resubmitted, vec![resubmitted(1)]);
}

#[test]
fn a_share_reaching_two_helpers_keeps_both() {
    // Deduplication is per (share, helper): reaching a second helper is a
    // distinct fact about the share's placement, not a repetition.
    let mut run = Run::default();

    run.absorb(&ShareTrackingReport {
        resubmitted: vec![resubmitted_to(1, "https://helper-a.example")],
        ..ShareTrackingReport::default()
    });
    run.absorb(&ShareTrackingReport {
        resubmitted: vec![resubmitted_to(1, "https://helper-b.example")],
        ..ShareTrackingReport::default()
    });
    let report = run.finish(ShareTrackingQuiescence::VoteEndReached);

    assert_eq!(
        report.resubmitted,
        vec![
            resubmitted_to(1, "https://helper-a.example"),
            resubmitted_to(1, "https://helper-b.example"),
        ],
    );
}

#[test]
fn a_repeated_ambiguous_attempt_is_recorded_once() {
    let mut run = Run::default();

    for _ in 0..3 {
        run.absorb(&ShareTrackingReport {
            ambiguous: vec![resubmitted(2)],
            ..ShareTrackingReport::default()
        });
    }

    assert_eq!(
        run.finish(ShareTrackingQuiescence::VoteEndReached)
            .ambiguous,
        vec![resubmitted(2)],
    );
}

#[test]
fn a_settled_attempt_does_not_become_ambiguous_again() {
    // Once any pass is told plainly that the helper took the share, that is
    // known. A later attempt at the same helper whose outcome is unreadable
    // does not unlearn it.
    let mut run = Run::default();

    run.absorb(&ShareTrackingReport {
        resubmitted: vec![resubmitted(3)],
        ..ShareTrackingReport::default()
    });
    run.absorb(&ShareTrackingReport {
        ambiguous: vec![resubmitted(3)],
        ..ShareTrackingReport::default()
    });
    let report = run.finish(ShareTrackingQuiescence::VoteEndReached);

    assert!(report.ambiguous.is_empty(), "got {:?}", report.ambiguous);
    assert_eq!(report.resubmitted, vec![resubmitted(3)]);
}
