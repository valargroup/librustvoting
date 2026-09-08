//! Host-facing projections of one helper-share tracking run.
//!
//! Both flattened views build a default and then overwrite it, so the variant
//! that keeps the initializer's `kind` — `NothingToTrack` for a quiescence,
//! `PassStarted` for an event — is the one nothing would catch if the default
//! moved. Every variant is pinned here for that reason, along with the payload
//! each carries: a cross-language binding must see what a native caller sees.

use std::time::Duration;

use crate::share_tracking::{ResubmittedShare, ShareKey, ShareTrackingReport};
use crate::share_tracking_drive::{
    ShareTrackingEvent, ShareTrackingQuiescence, ShareTrackingRunReport,
};
use crate::wire::{
    ShareTrackingEventKind, ShareTrackingEventView, ShareTrackingPassReportView,
    ShareTrackingQuiescenceKind, ShareTrackingQuiescenceView, ShareTrackingRunReportView,
};

fn share(share_index: u32) -> ShareKey {
    ShareKey {
        bundle_index: 1,
        proposal_id: 7,
        share_index,
    }
}

fn resubmitted(share_index: u32) -> ResubmittedShare {
    ResubmittedShare {
        share: share(share_index),
        server_url: "https://helper.example".to_string(),
    }
}

fn pass_report() -> ShareTrackingReport {
    ShareTrackingReport {
        confirmed: vec![share(0)],
        resubmitted: vec![resubmitted(1)],
        ambiguous: vec![resubmitted(2)],
        unrecoverable: vec![share(3)],
        cancelled: false,
        next_delay_seconds: Some(90),
    }
}

#[test]
fn every_quiescence_reaches_its_own_kind() {
    // `NothingToTrack` is first on purpose: it is the variant whose arm writes
    // nothing, so it is the one a changed initializer would silently break.
    for (quiescence, kind) in [
        (
            ShareTrackingQuiescence::NothingToTrack,
            ShareTrackingQuiescenceKind::NothingToTrack,
        ),
        (
            ShareTrackingQuiescence::AllConfirmed,
            ShareTrackingQuiescenceKind::AllConfirmed,
        ),
        (
            ShareTrackingQuiescence::VoteEndReached,
            ShareTrackingQuiescenceKind::VoteEndReached,
        ),
        (
            ShareTrackingQuiescence::Cancelled,
            ShareTrackingQuiescenceKind::Cancelled,
        ),
    ] {
        let view = ShareTrackingQuiescenceView::from(quiescence.clone());
        assert_eq!(view.kind, kind, "{quiescence:?}");
        assert!(view.messages.is_empty(), "{quiescence:?}");
        assert!(view.unrecoverable.is_empty(), "{quiescence:?}");
    }
}

#[test]
fn a_failing_run_carries_the_failures_that_ended_it() {
    let view = ShareTrackingQuiescenceView::from(ShareTrackingQuiescence::Failing {
        messages: vec!["helper unreachable".to_string(), "still down".to_string()],
    });

    assert_eq!(view.kind, ShareTrackingQuiescenceKind::Failing);
    assert_eq!(view.messages, vec!["helper unreachable", "still down"]);
    assert!(view.unrecoverable.is_empty());
}

#[test]
fn an_exhausted_budget_names_the_shares_the_last_pass_could_not_repair() {
    let view = ShareTrackingQuiescenceView::from(ShareTrackingQuiescence::PassBudgetExhausted {
        unrecoverable: vec![share(4)],
    });

    assert_eq!(view.kind, ShareTrackingQuiescenceKind::PassBudgetExhausted);
    assert_eq!(view.unrecoverable.len(), 1);
    assert_eq!(view.unrecoverable[0].share_index, 4);
}

#[test]
fn a_started_pass_carries_only_its_number() {
    // The event whose arm writes no `kind`, for the same reason as
    // `NothingToTrack` above.
    let view = ShareTrackingEventView::from(ShareTrackingEvent::PassStarted { pass: 2 });

    assert_eq!(view.kind, ShareTrackingEventKind::PassStarted);
    assert_eq!(view.pass, Some(2));
    assert!(view.report.is_none());
    assert!(view.message.is_none());
    assert!(view.delay_seconds.is_none());
}

#[test]
fn a_finished_pass_carries_everything_that_pass_did() {
    let view = ShareTrackingEventView::from(ShareTrackingEvent::PassFinished {
        pass: 3,
        report: Box::new(pass_report()),
    });

    assert_eq!(view.kind, ShareTrackingEventKind::PassFinished);
    assert_eq!(view.pass, Some(3));
    let report = view.report.expect("a finished pass carries its report");
    assert_eq!(report.confirmed.len(), 1);
    assert_eq!(report.resubmitted.len(), 1);
    assert_eq!(report.resubmitted[0].server_url, "https://helper.example");
    assert_eq!(report.ambiguous.len(), 1);
    assert_eq!(report.unrecoverable.len(), 1);
    assert!(!report.cancelled);
    assert_eq!(report.next_delay_seconds, Some(90));
}

#[test]
fn a_failed_pass_carries_its_message_and_no_report() {
    let view = ShareTrackingEventView::from(ShareTrackingEvent::PassFailed {
        pass: 1,
        message: "empty helper fleet".to_string(),
    });

    assert_eq!(view.kind, ShareTrackingEventKind::PassFailed);
    assert_eq!(view.pass, Some(1));
    assert_eq!(view.message.as_deref(), Some("empty helper fleet"));
    assert!(view.report.is_none());
}

#[test]
fn a_wait_carries_its_delay_and_belongs_to_no_pass() {
    let view = ShareTrackingEventView::from(ShareTrackingEvent::AwaitingNextPass {
        delay: Duration::from_secs(15),
    });

    assert_eq!(view.kind, ShareTrackingEventKind::AwaitingNextPass);
    assert_eq!(view.delay_seconds, Some(15));
    assert!(
        view.pass.is_none(),
        "a wait sits between two passes rather than inside one"
    );
}

#[test]
fn a_run_report_projects_every_field_and_survives_a_round_trip() {
    let report = ShareTrackingRunReport {
        quiescence: ShareTrackingQuiescence::PassBudgetExhausted {
            unrecoverable: vec![share(3)],
        },
        passes: 4,
        confirmed: vec![share(0)],
        resubmitted: vec![resubmitted(1)],
        ambiguous: vec![resubmitted(2)],
        unrecoverable: vec![share(3)],
        failures: vec!["helper unreachable".to_string()],
    };

    let view = ShareTrackingRunReportView::from(report);
    assert_eq!(
        view.quiescence.kind,
        ShareTrackingQuiescenceKind::PassBudgetExhausted
    );
    assert_eq!(view.passes, 4);
    assert_eq!(view.confirmed.len(), 1);
    assert_eq!(view.resubmitted.len(), 1);
    assert_eq!(view.ambiguous.len(), 1);
    assert_eq!(view.unrecoverable.len(), 1);
    assert_eq!(view.failures, vec!["helper unreachable"]);

    let json = serde_json::to_string(&view).unwrap();
    let decoded: ShareTrackingRunReportView = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, view);
}

#[test]
fn an_omitted_collection_decodes_as_empty_rather_than_failing() {
    // Host bindings written against an earlier shape must keep decoding, which
    // is what the `serde(default)` on every collection is for.
    let pass: ShareTrackingPassReportView = serde_json::from_str(r#"{"cancelled":false}"#).unwrap();
    assert!(pass.confirmed.is_empty());
    assert!(pass.unrecoverable.is_empty());
    assert_eq!(pass.next_delay_seconds, None);

    let quiescence: ShareTrackingQuiescenceView =
        serde_json::from_str(r#"{"kind":"all_confirmed"}"#).unwrap();
    assert_eq!(quiescence.kind, ShareTrackingQuiescenceKind::AllConfirmed);
    assert!(quiescence.messages.is_empty());
}
