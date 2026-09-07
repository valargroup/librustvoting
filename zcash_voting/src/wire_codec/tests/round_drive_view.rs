//! Host-facing projections of one round run.
//!
//! The projection must be total: a cross-language binding sees what a native
//! caller sees, so every field of `RoundRunReport` reaches its view.

use crate::round_drive::{RoundDriveEvent, RoundQuiescence, RoundRunReport, RoundWorkTally};
use crate::session::NextStep;
use crate::wire::{
    RoundDriveEventKind, RoundDriveEventView, RoundQuiescenceKind, RoundQuiescenceView,
    RoundRunReportView,
};

fn step() -> NextStep {
    NextStep::AdvanceVoteBatch {
        bundle_index: 1,
        proposal_id: 7,
    }
}

#[test]
fn a_ballot_quiescence_carries_what_the_host_must_resolve() {
    let view = RoundQuiescenceView::try_from(RoundQuiescence::NeedsBallot {
        open_proposals: vec![2, 3],
        unrostered_intents: vec![9],
    })
    .unwrap();
    assert_eq!(view.kind, RoundQuiescenceKind::NeedsBallot);
    assert_eq!(view.open_proposals, vec![2, 3]);
    assert_eq!(view.unrostered_intents, vec![9]);
    assert!(view.bundles.is_empty());
    assert!(view.step.is_none());
}

#[test]
fn a_signature_handoff_names_every_bundle() {
    let view = RoundQuiescenceView::try_from(RoundQuiescence::NeedsDelegationSignatures {
        bundles: vec![0, 3],
    })
    .unwrap();
    assert_eq!(view.kind, RoundQuiescenceKind::NeedsDelegationSignatures);
    assert_eq!(view.bundles, vec![0, 3]);
}

#[test]
fn an_exhausted_budget_carries_the_work_it_left() {
    let view = RoundQuiescenceView::try_from(RoundQuiescence::PassBudgetExhausted {
        remaining: vec![step()],
    })
    .unwrap();
    assert_eq!(view.kind, RoundQuiescenceKind::PassBudgetExhausted);
    assert_eq!(view.remaining.len(), 1);
    assert_eq!(view.remaining[0].bundle_index, 1);
    assert_eq!(view.remaining[0].proposal_id, 7);
}

#[test]
fn a_stopped_run_reports_its_quiescence_and_tally() {
    let report = RoundRunReport {
        quiescence: RoundQuiescence::NoWorkLeft,
        plan: None,
        tally: RoundWorkTally {
            completed_proposals: 2,
            total_proposals: 3,
            remaining_obligations: 1,
        },
        failures: Vec::new(),
        skipped_bundles: vec![4],
        chain_outcomes: Vec::new(),
        share_deliveries: Vec::new(),
        delegations: Vec::new(),
    };
    let view = RoundRunReportView::try_from(report).unwrap();
    assert_eq!(view.quiescence.kind, RoundQuiescenceKind::NoWorkLeft);
    assert_eq!(view.tally.completed_proposals, 2);
    assert_eq!(view.tally.total_proposals, 3);
    assert_eq!(view.skipped_bundles, vec![4]);
    assert!(view.plan.is_none());
}

#[test]
fn every_work_event_names_its_step() {
    // The subjectless progress records are exactly why the view keeps
    // a step: a host reading one stream while bundles overlap has
    // nothing else to attribute them by.
    for event in [
        RoundDriveEvent::StepSelected { step: step() },
        RoundDriveEvent::StepFinished {
            step: step(),
            disposition: crate::RoundStepDisposition::Advanced,
        },
        RoundDriveEvent::AwaitingRepoll {
            step: step(),
            delay: std::time::Duration::from_secs(2),
        },
        RoundDriveEvent::BundleSkipped {
            bundle_index: 1,
            after: step(),
        },
    ] {
        let view = RoundDriveEventView::try_from(event).unwrap();
        let named = view.step.expect("every work event names its step");
        assert_eq!(named.bundle_index, 1);
    }
}

#[test]
fn a_repoll_event_carries_its_delay() {
    let view = RoundDriveEventView::try_from(RoundDriveEvent::AwaitingRepoll {
        step: step(),
        delay: std::time::Duration::from_millis(1500),
    })
    .unwrap();
    assert_eq!(view.kind, RoundDriveEventKind::AwaitingRepoll);
    assert_eq!(view.delay_seconds, Some(1.5));
}

#[test]
fn a_quiescence_view_round_trips_through_json() {
    // These views are a stable cross-language schema, so the field
    // names travel with them.
    let view = RoundQuiescenceView::try_from(RoundQuiescence::NeedsBallot {
        open_proposals: vec![1],
        unrostered_intents: Vec::new(),
    })
    .unwrap();
    let encoded = serde_json::to_string(&view).unwrap();
    assert!(encoded.contains("\"needs_ballot\""), "{encoded}");
    let decoded: RoundQuiescenceView = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, view);
}
