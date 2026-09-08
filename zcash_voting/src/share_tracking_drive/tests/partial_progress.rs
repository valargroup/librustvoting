//! What a run keeps from a pass that did not finish.
//!
//! A pass walks a round's unconfirmed shares and commits each confirmation and
//! retained recovery attempt as it reaches it, so an error means the walk
//! stopped, not that nothing happened. Those effects are also unrepeatable: the
//! next pass walks only unconfirmed shares, so a share this one confirmed is
//! never seen again and would be missing from the run's report altogether.

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
    ResubmittedShare {
        share: share(share_index),
        server_url: "https://helper.example".to_string(),
    }
}

#[test]
fn a_failed_pass_still_hands_over_what_it_committed() {
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
fn a_failed_pass_does_not_narrow_the_unrecoverable_set() {
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
