//! The arithmetic every reported number rests on.
//!
//! Built from JSON rather than struct literals, because the SDK's observation
//! types are `#[non_exhaustive]` and cannot be constructed outside their crate.
//! That is not a workaround: a benchmark reads snapshots off disk, so building
//! the fixtures the same way exercises the decode path the real runs use.

use stage_bench::events::PhaseEvent;
use stage_bench::metrics::{sweep, Interval, Metrics, Occupancy, Percentiles};
use stage_bench::CapturedSnapshot;

/// One record, as it appears inside a snapshot.
fn record(
    id: u64,
    parent: Option<u64>,
    stage: &str,
    start: u64,
    elapsed: u64,
    outcome: &str,
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "parent_id": parent,
        "stage": stage,
        "attribution": { "bundle_index": null, "proposal_id": null, "share_index": null },
        "started_after_us": start,
        "elapsed_us": elapsed,
        "outcome": outcome,
        "error_kind": null,
        "http_status": null,
        "endpoint_index": null,
        "attempt": null
    })
}

fn attributed(
    mut value: serde_json::Value,
    proposal_id: u32,
    share_index: u32,
) -> serde_json::Value {
    value["attribution"] = serde_json::json!({
        "bundle_index": 0,
        "proposal_id": proposal_id,
        "share_index": share_index
    });
    value
}

fn with_status(mut value: serde_json::Value, status: u16) -> serde_json::Value {
    value["http_status"] = serde_json::json!(status);
    value
}

fn snapshot(
    operation: &str,
    anchor: u64,
    records: Vec<serde_json::Value>,
    dropped: u64,
) -> CapturedSnapshot {
    let value = serde_json::json!({
        "operation": operation,
        "started_at_unix_us": anchor,
        "round_id": "round",
        "elapsed_us": 1_000_000u64,
        "outcome": "succeeded",
        "records": records,
        "summaries": [],
        "records_dropped": dropped,
        "summary_updates_dropped": 0,
        "active_stages_dropped": 0
    });
    CapturedSnapshot {
        source: format!("{operation}.observability.json"),
        snapshot: serde_json::from_value(value).expect("a decodable snapshot"),
    }
}

#[test]
fn a_sweep_finds_the_peak_and_the_average() {
    // Three intervals over [0, 30): two overlap in [10, 20).
    let intervals = vec![
        Interval {
            start_us: 0,
            elapsed_us: 20,
        },
        Interval {
            start_us: 10,
            elapsed_us: 20,
        },
        Interval {
            start_us: 25,
            elapsed_us: 5,
        },
    ];

    let occupancy = sweep(&intervals);
    assert_eq!(occupancy.samples, 3);
    assert_eq!(occupancy.peak, 2);
    assert_eq!(occupancy.wall_span_us, 30);
    assert_eq!(occupancy.cumulative_us, 45);
    // 45 microseconds of work across a 30 microsecond span, idle gap included.
    assert!((occupancy.average - 1.5).abs() < f64::EPSILON);
}

/// An interval that ends exactly as the next begins is not two in flight.
///
/// The opposite convention inflates every peak by one at a busy boundary, which
/// is precisely where a concurrency claim is made and precisely the error most
/// likely to be believed.
#[test]
fn an_end_and_a_start_at_the_same_instant_do_not_overlap() {
    let intervals = vec![
        Interval {
            start_us: 0,
            elapsed_us: 10,
        },
        Interval {
            start_us: 10,
            elapsed_us: 10,
        },
        Interval {
            start_us: 20,
            elapsed_us: 10,
        },
    ];

    assert_eq!(sweep(&intervals).peak, 1);
}

#[test]
fn an_empty_sweep_reports_nothing_rather_than_dividing_by_zero() {
    assert_eq!(sweep(&[]), Occupancy::default());

    let instant = vec![Interval {
        start_us: 5,
        elapsed_us: 0,
    }];
    let occupancy = sweep(&instant);
    assert_eq!(occupancy.wall_span_us, 0);
    assert!(occupancy.average.is_finite());
}

#[test]
fn percentiles_are_nearest_rank_over_real_samples() {
    let mut samples: Vec<u64> = (1..=100).collect();
    let percentiles = Percentiles::of(&mut samples);

    assert_eq!(percentiles.p50_us, 50);
    assert_eq!(percentiles.p95_us, 95);
    assert_eq!(percentiles.p99_us, 99);
    assert_eq!(percentiles.max_us, 100);

    // Every reported value is a sample, never an interpolation between two.
    let mut two = vec![10, 90];
    let percentiles = Percentiles::of(&mut two);
    assert_eq!(percentiles.p50_us, 10);
    assert_eq!(percentiles.p95_us, 90);

    assert_eq!(Percentiles::of(&mut []), Percentiles::default());
}

/// A POST below a resubmission is a repair, not a first placement.
///
/// Mixing recovery attempts into the initial window would report a delivery
/// that took as long as the round's slowest retry.
#[test]
fn recovery_attempts_are_excluded_from_the_initial_delivery_window() {
    let snapshot = snapshot(
        "round::run",
        1_000,
        vec![
            record(1, None, "helper::active_delivery", 0, 500, "succeeded"),
            record(2, Some(1), "helper::post_share", 10, 400, "succeeded"),
            record(3, Some(2), "helper.http.post_json", 20, 300, "succeeded"),
            // A repair, nested under the same delivery workflow.
            record(4, Some(1), "helper::resubmit_share", 600, 900, "succeeded"),
            record(5, Some(4), "helper.http.post_json", 610, 800, "succeeded"),
            // A status poll, which is neither.
            record(6, None, "helper::share_status", 700, 50, "pending"),
        ],
        0,
    );

    let metrics = Metrics::derive(&[snapshot], &[]);

    assert_eq!(metrics.delivery.initial_http.samples, 1);
    assert_eq!(metrics.delivery.initial_http.cumulative_us, 300);
    assert_eq!(metrics.delivery.recovery_http_attempts, 1);
}

#[test]
fn delivery_concurrency_is_measured_over_share_workflows() {
    let snapshot = snapshot(
        "round::run",
        0,
        vec![
            record(1, None, "helper::active_delivery", 0, 100, "succeeded"),
            record(2, None, "helper::active_delivery", 50, 100, "succeeded"),
            record(3, None, "helper::active_delivery", 60, 100, "succeeded"),
        ],
        0,
    );

    let metrics = Metrics::derive(&[snapshot], &[]);

    assert_eq!(metrics.delivery.active_shares.peak, 3);
    assert_eq!(metrics.delivery.active_shares.samples, 3);
}

/// Two snapshots land on one timeline through their own wall-clock anchors.
///
/// Without that, a tracking invocation's records would appear to start at the
/// same instant as the round drive's, and every span covering both would be
/// wrong.
#[test]
fn snapshots_are_placed_on_one_absolute_timeline() {
    let round = snapshot(
        "round::run",
        1_000_000,
        vec![record(
            1,
            None,
            "helper::active_delivery",
            0,
            1_000,
            "succeeded",
        )],
        0,
    );
    let tracking = snapshot(
        "helper::tracking_run",
        3_000_000,
        vec![record(
            1,
            None,
            "helper::active_delivery",
            0,
            1_000,
            "succeeded",
        )],
        0,
    );

    let metrics = Metrics::derive(&[round, tracking], &[]);

    // 1_000_000 to 3_001_000: the two are two seconds apart, not simultaneous.
    let stage = metrics
        .stage("helper::active_delivery")
        .expect("the delivery stage");
    assert_eq!(stage.calls, 2);
    assert_eq!(stage.wall_span_us, 2_001_000);
    assert_eq!(stage.peak_concurrency, 1);
    assert_eq!(metrics.wall_span_us, 2_001_000);
}

#[test]
fn unfinished_work_is_counted_but_never_sampled() {
    let snapshot = snapshot(
        "round::run",
        0,
        vec![
            record(1, None, "helper::post_share", 0, 100, "succeeded"),
            record(2, None, "helper::post_share", 0, 5, "unfinished"),
            record(3, None, "helper::post_share", 0, 7, "cancelled"),
        ],
        0,
    );

    let metrics = Metrics::derive(&[snapshot], &[]);

    let stage = metrics.stage("helper::post_share").expect("the post stage");
    assert_eq!(stage.calls, 3);
    // A clipped attempt's duration is a lower bound; sampling it would drag
    // every percentile toward zero.
    assert_eq!(stage.latency.max_us, 100);
    assert_eq!(stage.latency.p50_us, 100);
    assert_eq!(stage.outcomes.get("unfinished"), Some(&1));
    assert_eq!(stage.outcomes.get("cancelled"), Some(&1));
}

#[test]
fn a_dropped_record_marks_the_whole_capture_incomplete() {
    let snapshot = snapshot(
        "round::run",
        0,
        vec![record(
            1,
            None,
            "helper::active_delivery",
            0,
            10,
            "succeeded",
        )],
        7,
    );

    let metrics = Metrics::derive(&[snapshot], &[]);

    assert!(!metrics.complete);
    assert_eq!(metrics.incomplete.len(), 1);
    assert!(metrics.incomplete[0].contains("7 records"));
}

#[test]
fn per_proposal_costs_are_grouped_by_attribution() {
    let snapshot = snapshot(
        "round::run",
        0,
        vec![
            attributed(
                record(1, None, "helper::active_delivery", 0, 100, "succeeded"),
                7,
                0,
            ),
            attributed(
                record(2, None, "helper::active_delivery", 100, 300, "succeeded"),
                7,
                1,
            ),
            attributed(
                record(3, None, "zkp2::build_vote_commitment", 0, 900, "succeeded"),
                8,
                0,
            ),
            // No proposal id: infrastructure work that belongs to no question.
            record(4, None, "helper::preflight_fleet", 0, 40, "succeeded"),
        ],
        0,
    );

    let metrics = Metrics::derive(&[snapshot], &[]);

    assert_eq!(metrics.proposals.len(), 2);
    let seven = &metrics.proposals[0];
    assert_eq!(seven.proposal_id, 7);
    assert_eq!(seven.delivery_cumulative_us, 400);
    assert_eq!(
        seven.stages["helper::active_delivery"].calls, 2,
        "both of the proposal's shares are its cost"
    );
    assert_eq!(seven.stages["helper::active_delivery"].max_us, 300);

    let eight = &metrics.proposals[1];
    assert_eq!(eight.proposal_id, 8);
    assert_eq!(
        eight.stages["zkp2::build_vote_commitment"].cumulative_us,
        900
    );
}

#[test]
fn http_statuses_and_post_outcomes_are_kept_apart() {
    let snapshot = snapshot(
        "round::run",
        0,
        vec![
            record(1, None, "helper::post_share", 0, 10, "succeeded"),
            record(2, None, "helper::post_share", 0, 10, "possibly_dispatched"),
            record(3, None, "helper::post_share", 0, 10, "reused"),
            record(4, None, "helper::persist_acceptance", 0, 5, "succeeded"),
            with_status(
                record(5, None, "helper.http.post_json", 0, 8, "succeeded"),
                200,
            ),
            with_status(
                record(6, None, "helper.http.post_json", 0, 8, "failed"),
                503,
            ),
        ],
        0,
    );

    let metrics = Metrics::derive(&[snapshot], &[]);

    let delivery = &metrics.delivery;
    assert_eq!(delivery.post_outcomes.get("succeeded"), Some(&1));
    assert_eq!(delivery.post_outcomes.get("possibly_dispatched"), Some(&1));
    assert_eq!(delivery.post_outcomes.get("reused"), Some(&1));
    assert_eq!(delivery.acceptance_outcomes.get("succeeded"), Some(&1));
    assert_eq!(delivery.http_status.get(&200), Some(&1));
    assert_eq!(delivery.http_status.get(&503), Some(&1));
}

#[test]
fn stages_are_ordered_by_the_wall_time_they_occupied() {
    let snapshot = snapshot(
        "round::run",
        0,
        vec![
            // Large cumulative, small span: eight overlapping calls.
            record(1, None, "helper::active_delivery", 0, 100, "succeeded"),
            record(2, None, "helper::active_delivery", 0, 100, "succeeded"),
            record(3, None, "helper::active_delivery", 0, 100, "succeeded"),
            record(4, None, "helper::active_delivery", 0, 100, "succeeded"),
            // Small cumulative, large span: one long serial call.
            record(5, None, "zkp2::build_vote_commitment", 0, 350, "succeeded"),
        ],
        0,
    );

    let metrics = Metrics::derive(&[snapshot], &[]);

    // The bottleneck is what held the clock, not what accumulated the most
    // time across concurrent workers.
    assert_eq!(metrics.stages[0].stage, "zkp2::build_vote_commitment");
    assert_eq!(metrics.stages[0].wall_span_us, 350);
    assert_eq!(metrics.stages[1].stage, "helper::active_delivery");
    assert_eq!(metrics.stages[1].cumulative_us, 400);
    assert_eq!(metrics.stages[1].peak_concurrency, 4);
}

/// Bundle work and proposal work are two different tables.
///
/// A combined delegate-and-cast batch is prepared once per bundle, covering
/// every proposal in it, so the SDK attributes it to a bundle and no proposal.
/// Putting it in the per-proposal table would print a column of zeros; leaving
/// a helper share out of the per-bundle table is what keeps that table about
/// the work paid once per bundle.
#[test]
fn bundle_work_and_proposal_work_are_reported_separately() {
    let mut share = record(3, None, "helper::active_delivery", 0, 50, "succeeded");
    share["attribution"] =
        serde_json::json!({ "bundle_index": 0, "proposal_id": 1, "share_index": 0 });
    let mut batch = record(
        1,
        None,
        "vote::prepare_atomic_vote_batch",
        0,
        700,
        "succeeded",
    );
    batch["attribution"] =
        serde_json::json!({ "bundle_index": 0, "proposal_id": null, "share_index": null });
    let mut delegation = record(
        2,
        None,
        "zkp1::build_and_prove_delegation",
        0,
        300,
        "succeeded",
    );
    delegation["attribution"] =
        serde_json::json!({ "bundle_index": 0, "proposal_id": null, "share_index": null });

    let metrics = Metrics::derive(
        &[snapshot("round", 0, vec![batch, delegation, share], 0)],
        &[],
    );

    assert_eq!(metrics.bundles.len(), 1);
    let bundle = &metrics.bundles[0];
    assert_eq!(bundle.bundle_index, 0);
    assert_eq!(
        bundle.stages["vote::prepare_atomic_vote_batch"].cumulative_us,
        700
    );
    assert_eq!(
        bundle.stages["zkp1::build_and_prove_delegation"].cumulative_us,
        300
    );
    // The share belongs to a proposal and is not per-bundle work, even though
    // it carries a bundle index.
    assert!(!bundle.stages.contains_key("helper::active_delivery"));

    assert_eq!(metrics.proposals.len(), 1);
    assert_eq!(metrics.proposals[0].delivery_cumulative_us, 50);
    assert!(!metrics.proposals[0]
        .stages
        .contains_key("vote::prepare_atomic_vote_batch"));
}

/// Two invocations the SDK names alike stay distinguishable in the report.
#[test]
fn invocations_are_named_by_the_file_they_came_from() {
    let metrics = Metrics::derive(
        &[
            snapshot("round", 0, vec![], 0),
            snapshot("tracking.0", 0, vec![], 0),
        ],
        &[],
    );

    assert_eq!(metrics.invocations.len(), 2);
    assert_eq!(metrics.invocations[0].source, "round.observability.json");
    assert_eq!(
        metrics.invocations[1].source,
        "tracking.0.observability.json"
    );
}

#[test]
fn a_run_with_no_records_derives_empty_metrics() {
    let metrics = Metrics::derive(&[], &[]);

    assert!(metrics.complete);
    assert!(metrics.stages.is_empty());
    assert!(metrics.proposals.is_empty());
    assert_eq!(metrics.wall_span_us, 0);
    assert_eq!(metrics.delivery.active_shares.peak, 0);
}

#[test]
fn phase_events_round_trip_through_their_log() {
    let directory = std::env::temp_dir().join(format!("stage-bench-events-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("a scratch directory");

    let log = stage_bench::events::EventLog::create(&directory).expect("a log");
    log.record(PhaseEvent::phase("round::drive_started"));
    let mut detailed = PhaseEvent::phase("round::step_selected");
    detailed.proposal_id = Some(12);
    detailed.bundle_index = Some(1);
    log.record(detailed);

    let read = stage_bench::events::EventLog::read(&directory).expect("the log back");
    assert_eq!(read.len(), 2);
    assert_eq!(read[0].phase, "round::drive_started");
    assert_eq!(read[1].proposal_id, Some(12));
    assert_eq!(read[1].bundle_index, Some(1));
    assert!(read[0].at_unix_us > 0);

    let _ = std::fs::remove_dir_all(&directory);
}
