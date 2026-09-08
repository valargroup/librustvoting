use crate::{
    ObservabilityOptions, ObservationAttribution, ObservationOutcome as Outcome, ObservationScope,
};
use std::sync::Arc;

#[test]
fn disabled_collection_preserves_the_result() {
    let owner = ObservationScope::new(None).invocation();
    owner
        .stage("proof")
        .finish(Outcome::Failed, Some("ProofFailed"));
    let report = owner.complete("vote", Outcome::Failed, Err::<(), _>("original error"));
    let (result, diagnostics) = report.into_parts();
    assert_eq!(result, Err("original error"));
    assert!(diagnostics.is_none());
}

#[test]
fn internal_children_cannot_finalize_their_owner() {
    let owner = ObservationScope::new(Some(ObservabilityOptions::default())).invocation();
    let parent = owner.stage("composition");
    for _ in 0..2 {
        assert!(crate::delegate::observe_spend_auth_signature(&[], 0, parent.scope()).is_err());
    }
    parent.finish(Outcome::Failed, None);
    let report = owner.finish("composition", None, Outcome::Failed).unwrap();
    assert_eq!(report.records.len(), 3);
    assert!(report.records[1..]
        .iter()
        .all(|record| record.parent_id == Some(0)));
}

#[test]
fn summary_only_preserves_every_outcome() {
    let owner = ObservationScope::new(Some(ObservabilityOptions::summaries_only())).invocation();
    for outcome in [
        Outcome::Failed,
        Outcome::Rejected,
        Outcome::PossiblyDispatched,
        Outcome::Cancelled,
        Outcome::Pending,
        Outcome::Succeeded,
        Outcome::NoWork,
        Outcome::Reused,
    ] {
        owner.stage("operation").finish(outcome, None);
    }
    let unfinished = owner.stage("operation");
    let report = owner.finish("run", None, Outcome::Cancelled).unwrap();
    assert!(report.records.is_empty());
    assert_eq!(report.records_dropped, 9);
    assert_eq!(report.summaries.len(), 9);
    assert!(report.summaries.iter().all(|summary| summary.calls == 1));
    assert!(report
        .summaries
        .iter()
        .any(|summary| summary.outcome == Outcome::Unfinished));
    let snapshot = report.clone();
    unfinished.finish(Outcome::Succeeded, None);
    assert_eq!(snapshot, report);
}

#[test]
fn summary_limit_counts_omitted_updates_and_keeps_existing_groups() {
    let owner = ObservationScope::new(Some(ObservabilityOptions {
        max_summary_groups: 1,
        ..Default::default()
    }))
    .invocation();
    owner.stage("attempt").finish(Outcome::Failed, None);
    owner.stage("attempt").finish(Outcome::Rejected, None);
    owner.stage("attempt").finish(Outcome::Rejected, None);
    owner.stage("attempt").finish(Outcome::Failed, None);
    let report = owner.finish("run", None, Outcome::Failed).unwrap();
    assert_eq!(report.summaries.len(), 1);
    assert_eq!(report.summaries[0].calls, 2);
    assert_eq!(report.summary_updates_dropped, 2);
}

#[test]
fn active_limit_retains_the_nearest_admitted_parent() {
    let owner = ObservationScope::new(Some(ObservabilityOptions {
        max_active_stages: 1,
        ..Default::default()
    }))
    .invocation();
    let root = owner.stage("root");
    let omitted = root.scope().stage("omitted");
    root.finish(Outcome::Succeeded, None);
    omitted
        .scope()
        .stage("later-child")
        .finish(Outcome::Succeeded, None);
    let report = owner.finish("run", None, Outcome::Succeeded).unwrap();
    assert_eq!(report.active_stages_dropped, 1);
    assert_eq!(report.records.len(), 2);
    assert_eq!(report.records[1].parent_id, Some(report.records[0].id));
}

#[test]
fn detail_limit_reserves_capacity_for_unfinished_parents() {
    let owner = ObservationScope::new(Some(ObservabilityOptions {
        max_records: 1,
        ..Default::default()
    }))
    .invocation();
    let root = owner.stage("root");
    root.scope().stage("child").finish(Outcome::Succeeded, None);
    let report = owner.finish("run", None, Outcome::Cancelled).unwrap();
    assert_eq!(report.records.len(), 1);
    assert_eq!(report.records[0].stage.as_ref(), "root");
    assert_eq!(report.records[0].outcome, Outcome::Unfinished);
    assert_eq!(report.records_dropped, 1);
}

#[test]
fn attribution_is_independent_across_workers_and_resets_on_identity_changes() {
    let owner = ObservationScope::new(Some(ObservabilityOptions::default())).invocation();
    std::thread::scope(|threads| {
        for bundle_index in 0..4 {
            let scope = owner.attributed(ObservationAttribution {
                bundle_index: Some(bundle_index),
                ..Default::default()
            });
            threads.spawn(move || scope.stage("proof").finish(Outcome::Succeeded, None));
        }
    });
    let share = owner.attributed(ObservationAttribution {
        bundle_index: Some(0),
        proposal_id: Some(2),
        share_index: Some(3),
    });
    share
        .attributed(ObservationAttribution {
            bundle_index: Some(4),
            ..Default::default()
        })
        .stage("new-bundle")
        .finish(Outcome::Succeeded, None);
    let report = owner
        .finish("run", Some("round"), Outcome::Succeeded)
        .unwrap();
    assert_eq!(report.summaries.len(), 5);
    assert_eq!(
        report.records[4].attribution,
        ObservationAttribution {
            bundle_index: Some(4),
            ..Default::default()
        }
    );
}

#[test]
fn timestamps_labels_attempts_and_wire_projection_are_preserved() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros();
    let owner = ObservationScope::new(Some(ObservabilityOptions::default())).invocation();
    for attempt in 1..=2 {
        owner
            .attempt(attempt)
            .stage("transport")
            .finish(Outcome::Failed, Some("Transport"));
    }
    let report = owner.finish("run", None, Outcome::Failed).unwrap();
    let after = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros();
    assert!((before..=after).contains(&(report.started_at_unix_us as u128)));
    assert_eq!(
        report
            .records
            .iter()
            .map(|record| record.attempt)
            .collect::<Vec<_>>(),
        [Some(1), Some(2)]
    );
    assert!(Arc::ptr_eq(
        &report.records[0].stage,
        &report.records[1].stage
    ));
    assert!(Arc::ptr_eq(
        report.records[0].error_kind.as_ref().unwrap(),
        report.records[1].error_kind.as_ref().unwrap()
    ));
    let wire = crate::wire::OperationObservability::from(&report);
    let serialized = serde_json::to_value(&report).unwrap();
    assert_eq!(serialized, serde_json::to_value(wire).unwrap());
    assert_eq!(
        serde_json::from_value::<crate::OperationObservability>(serialized).unwrap(),
        report
    );
    let rendered = report.to_string();
    assert!(rendered.contains("run: failed"));
    assert!(rendered.contains("transport: failed calls=2"));
    assert!(rendered.contains("records=0 summary_updates=0 active_stages=0"));
}

#[test]
fn original_utility_signatures_support_result_combinators() {
    let extract: fn(&[u8], usize) -> Result<[u8; 64], crate::VotingError> =
        crate::delegate::spend_auth_signature;
    let results = [0, 1]
        .into_iter()
        .map(|index| extract(&[], index).map_err(|error| error.kind()))
        .collect::<Result<Vec<_>, _>>();
    assert!(results.is_err());
    fn propagate() -> Result<(), crate::VotingError> {
        crate::delegate::spend_auth_signature(&[], 0)?;
        Ok(())
    }
    assert!(propagate().is_err());
    let _: Option<crate::ObservationRecord> = None;
    let _: Option<crate::ObservationSummary> = None;
}

#[test]
fn recovery_failure_retains_known_round_bundle_and_proposal() {
    let database = crate::round::VotingDb::open_in_memory().unwrap();
    database.set_wallet_id("observed-recovery");
    let owner = ObservationScope::new(Some(ObservabilityOptions::default())).invocation();
    assert!(crate::vote::CommittedVote::observe_recover(
        &database,
        "missing-round",
        2,
        7,
        owner.scope()
    )
    .is_err());
    let report = owner.finish("recovery", None, Outcome::Failed).unwrap();
    assert_eq!(report.round_id.as_deref(), Some("missing-round"));
    assert!(report
        .records
        .iter()
        .all(|record| record.attribution.bundle_index == Some(2)
            && record.attribution.proposal_id == Some(7)));
    assert!(!report.records.is_empty());
}

#[test]
fn zero_limits_preserve_an_enabled_snapshot_and_count_omissions() {
    let owner = ObservationScope::new(Some(ObservabilityOptions {
        max_records: 0,
        max_summary_groups: 0,
        max_active_stages: 0,
    }))
    .invocation();
    owner.stage("omitted").finish(Outcome::Succeeded, None);
    let report = owner.finish("run", None, Outcome::Succeeded).unwrap();
    assert!(report.records.is_empty());
    assert!(report.summaries.is_empty());
    assert_eq!(report.active_stages_dropped, 1);
}
