//! Turning one run's observability snapshots into the numbers that name a
//! bottleneck.
//!
//! A run produces several snapshots — the round drive, then each background
//! tracking invocation — and each is measured against its own monotonic origin
//! with a wall-clock anchor. Every record here is placed on one absolute
//! timeline by adding its snapshot's `started_at_unix_us` to its
//! `started_after_us`, which is what lets a phase table say when helper
//! delivery ended and confirmation began rather than reporting two unrelated
//! totals.
//!
//! # What is deliberately not done
//!
//! Parent and child stage durations are never summed. A queue wait is already
//! inside the delivery time that contains it, and adding them would inflate
//! every helper phase by the wait that precedes it. Each stage is reported on
//! its own row, and the reader composes them.
//!
//! # When the numbers cannot be trusted
//!
//! A snapshot that dropped records, summary updates, or stage starts describes
//! less than the run did. Concurrency and percentiles derived from it are
//! floors, not measurements, so [`Metrics::complete`] is false and every
//! consumer says so rather than quietly reporting a smaller peak.

mod occupancy;
mod percentiles;
mod table;

pub use occupancy::{sweep, Interval, Occupancy};
pub use percentiles::Percentiles;
pub use table::render;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use zcash_voting::{ObservationOutcome, ObservationRecord};

use crate::CapturedSnapshot;

use crate::events::PhaseEvent;

/// The stage every share's own delivery workflow runs under.
///
/// Used as the ancestry marker for initial delivery: an HTTP attempt below one
/// of these is a first placement, and one below [`RECOVERY_STAGE`] is a repair.
/// Distinguishing them matters because mixing recovery POSTs into the initial
/// window would report a delivery that took as long as the round's slowest
/// retry.
pub const INITIAL_DELIVERY_STAGE: &str = "helper::active_delivery";

/// The stage a re-sent share runs under.
pub const RECOVERY_STAGE: &str = "helper::resubmit_share";

/// One transport attempt carrying a share to a helper.
pub const HELPER_POST_STAGE: &str = "helper.http.post_json";

/// The stages a per-proposal breakdown reports.
///
/// Chosen rather than "every stage that carries a proposal id": a table with a
/// row for each of forty stages is not a bottleneck view. These are the phases
/// a single question's own cost is spent in.
///
/// Note what is **not** here. A combined delegate-and-cast batch is prepared and
/// advanced once per *bundle*, covering every proposal in it, so the SDK
/// attributes `vote::prepare_atomic_vote_batch` and the chain stages to a bundle
/// and no proposal. Listing them here would print a column of zeros and invite
/// the reading that batch preparation is free. They are in [`BUNDLE_STAGES`].
pub const PROPOSAL_STAGES: &[&str] = &[
    "zkp2::build_vote_commitment",
    "zkp2.prove",
    "vote.prepare_share_delivery",
    "helper::delivery_queue_wait",
    "helper::active_delivery",
    "helper::share_lock_wait",
    "helper::post_capacity_wait",
    "helper::post_share",
    "helper.http.post_json",
    "helper::persist_acceptance",
    "helper::share_status",
    "helper::confirmation_quorum",
    "helper::persist_confirmation",
];

/// The stages a per-bundle breakdown reports.
///
/// Delegation, batch preparation, and chain advancement are per-bundle work.
/// Splitting them out is not a formatting choice: a 37-proposal round pays these
/// three times and its proving 111 times, and a table that mixed the two would
/// make the cheaper axis look like the expensive one.
pub const BUNDLE_STAGES: &[&str] = &[
    "delegation::setup",
    "delegation::ensure_proof",
    "zkp1::build_and_prove_delegation",
    "vote::prepare_atomic_vote_batch",
    "vote::persist_prepared_atomic_vote_batch",
    "chain::advance_until_terminal_in_epoch",
    "chain::post_attempt",
    "round::advance_step",
];

/// Everything derived from one run's snapshots.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Metrics {
    /// Whether every snapshot retained every measurement it took.
    pub complete: bool,
    /// Why not, when it is not. Empty when `complete`.
    pub incomplete: Vec<String>,
    /// One entry per reported invocation, in capture order.
    pub invocations: Vec<InvocationMetrics>,
    /// Every stage observed, most wall time first.
    pub stages: Vec<StageMetrics>,
    /// Per-proposal cost across [`PROPOSAL_STAGES`].
    pub proposals: Vec<ProposalMetrics>,
    /// Per-bundle cost across [`BUNDLE_STAGES`].
    pub bundles: Vec<BundleMetrics>,
    /// Helper delivery concurrency and outcome counts.
    pub delivery: DeliveryMetrics,
    /// Wall span of the whole run, first record start to last record end.
    pub wall_span_us: u64,
}

/// One reported invocation's own totals.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InvocationMetrics {
    /// The file this invocation's snapshot was read from.
    ///
    /// Reported beside the operation because the SDK names the round driver's
    /// and the share tracker's invocations alike.
    pub source: String,
    pub operation: String,
    pub started_at_unix_us: u64,
    pub elapsed_us: u64,
    pub outcome: String,
    pub records: usize,
    pub records_dropped: u64,
    pub summary_updates_dropped: u64,
    pub active_stages_dropped: u64,
}

/// One SDK stage, aggregated across every snapshot in the run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StageMetrics {
    pub stage: String,
    pub calls: usize,
    /// First start to last end. This, not the cumulative total, is how long the
    /// run actually spent with this stage in progress.
    pub wall_span_us: u64,
    /// Sum of every call's duration. Exceeds the wall span when calls overlap.
    pub cumulative_us: u64,
    pub peak_concurrency: usize,
    pub latency: Percentiles,
    /// Calls by execution outcome, which is not whether the call returned `Ok`.
    pub outcomes: BTreeMap<String, usize>,
}

/// One proposal's cost, stage by stage.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProposalMetrics {
    pub proposal_id: u32,
    /// Stage name to `(calls, cumulative_us, wall_span_us, max_us)`.
    pub stages: BTreeMap<String, ProposalStage>,
    /// Sum of the proposal's `helper::active_delivery` durations.
    pub delivery_cumulative_us: u64,
}

/// One bundle's cost, stage by stage.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BundleMetrics {
    pub bundle_index: u32,
    pub stages: BTreeMap<String, ProposalStage>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ProposalStage {
    pub calls: usize,
    pub cumulative_us: u64,
    pub wall_span_us: u64,
    pub max_us: u64,
}

/// What helper delivery did, and how much of it happened at once.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeliveryMetrics {
    /// Concurrency over per-share delivery workflows. This is the number the
    /// admission policy bounds.
    pub active_shares: Occupancy,
    /// Concurrency over initial-placement HTTP attempts only. Not the same
    /// number: one share fanning out to several helpers opens several requests.
    pub initial_http: Occupancy,
    pub initial_http_latency: Percentiles,
    /// Completed initial attempts divided by the initial window, per second.
    pub initial_http_throughput: f64,
    /// HTTP attempts made while repairing a placement.
    pub recovery_http_attempts: usize,
    /// `helper::post_share` results by outcome. `Pending` is the helper's
    /// `queued`, `Reused` its `duplicate`, `PossiblyDispatched` an ambiguous
    /// answer — each kept separate, because collapsing them into a success rate
    /// is how an ambiguous outcome becomes an accepted one.
    pub post_outcomes: BTreeMap<String, usize>,
    /// Definite acceptance writes by outcome.
    pub acceptance_outcomes: BTreeMap<String, usize>,
    /// Status codes seen on helper POSTs. A 2xx is not by itself acceptance.
    pub http_status: BTreeMap<u16, usize>,
}

/// One record placed on the run's absolute timeline.
struct Placed<'a> {
    record: &'a ObservationRecord,
    start_us: u64,
}

impl Placed<'_> {
    fn end_us(&self) -> u64 {
        self.start_us.saturating_add(self.record.elapsed_us)
    }

    fn interval(&self) -> Interval {
        Interval {
            start_us: self.start_us,
            elapsed_us: self.record.elapsed_us,
        }
    }
}

impl Metrics {
    /// Derives every metric from a run's snapshots.
    ///
    /// `events` is accepted for symmetry with the run directory and is not read
    /// yet: the phase log names boundaries between invocations, and today every
    /// invocation carries its own wall-clock anchor, which covers the same gap
    /// with a finer clock.
    pub fn derive(captured: &[CapturedSnapshot], events: &[PhaseEvent]) -> Self {
        let _ = events;

        let mut incomplete = Vec::new();
        let mut invocations = Vec::new();
        let mut placed: Vec<Placed<'_>> = Vec::new();
        // Ancestry is per snapshot: record ids are invocation-local, so a
        // parent id from one snapshot names a different record in another.
        let mut initial_http: Vec<Interval> = Vec::new();
        let mut initial_http_samples: Vec<u64> = Vec::new();
        let mut recovery_http_attempts = 0usize;

        for entry in captured {
            let snapshot = &entry.snapshot;
            invocations.push(InvocationMetrics {
                source: entry.source.clone(),
                operation: snapshot.operation.clone(),
                started_at_unix_us: snapshot.started_at_unix_us,
                elapsed_us: snapshot.elapsed_us,
                outcome: outcome_name(snapshot.outcome).to_string(),
                records: snapshot.records.len(),
                records_dropped: snapshot.records_dropped,
                summary_updates_dropped: snapshot.summary_updates_dropped,
                active_stages_dropped: snapshot.active_stages_dropped,
            });
            if snapshot.records_dropped > 0 {
                incomplete.push(format!(
                    "{} dropped {} records",
                    snapshot.operation, snapshot.records_dropped
                ));
            }
            if snapshot.summary_updates_dropped > 0 {
                incomplete.push(format!(
                    "{} dropped {} summary updates",
                    snapshot.operation, snapshot.summary_updates_dropped
                ));
            }
            if snapshot.active_stages_dropped > 0 {
                incomplete.push(format!(
                    "{} dropped {} stage starts",
                    snapshot.operation, snapshot.active_stages_dropped
                ));
            }

            let by_id: BTreeMap<u64, &ObservationRecord> = snapshot
                .records
                .iter()
                .map(|record| (record.id, record))
                .collect();

            for record in &snapshot.records {
                placed.push(Placed {
                    record,
                    start_us: snapshot
                        .started_at_unix_us
                        .saturating_add(record.started_after_us),
                });

                if &*record.stage != HELPER_POST_STAGE {
                    continue;
                }
                match delivery_kind(record, &by_id) {
                    DeliveryKind::Initial => {
                        initial_http.push(Interval {
                            start_us: snapshot
                                .started_at_unix_us
                                .saturating_add(record.started_after_us),
                            elapsed_us: record.elapsed_us,
                        });
                        if is_finished(record.outcome) {
                            initial_http_samples.push(record.elapsed_us);
                        }
                    }
                    DeliveryKind::Recovery => recovery_http_attempts += 1,
                    DeliveryKind::Other => {}
                }
            }
        }

        let stages = stage_metrics(&placed);
        let proposals = proposal_metrics(&placed);
        let bundles = bundle_metrics(&placed);
        let delivery = delivery_metrics(
            &placed,
            initial_http,
            initial_http_samples,
            recovery_http_attempts,
        );
        let wall_span_us = placed
            .iter()
            .map(Placed::end_us)
            .max()
            .unwrap_or_default()
            .saturating_sub(placed.iter().map(|p| p.start_us).min().unwrap_or_default());

        Self {
            complete: incomplete.is_empty(),
            incomplete,
            invocations,
            stages,
            proposals,
            bundles,
            delivery,
            wall_span_us,
        }
    }

    /// The stage row for `stage`, if the run observed it.
    pub fn stage(&self, stage: &str) -> Option<&StageMetrics> {
        self.stages.iter().find(|entry| entry.stage == stage)
    }
}

/// Whether an HTTP attempt was a first placement or a repair.
enum DeliveryKind {
    Initial,
    Recovery,
    Other,
}

/// Classifies one HTTP attempt by walking its ancestry.
///
/// Recovery wins over initial when both appear: a resubmission nested inside a
/// still-open delivery workflow is a repair, and counting it as a first
/// placement would let a retry's latency into the initial window.
///
/// The walk is bounded by the record count, so a snapshot with a cyclic parent
/// chain — which the collector does not produce — cannot hang the analysis.
fn delivery_kind(
    record: &ObservationRecord,
    by_id: &BTreeMap<u64, &ObservationRecord>,
) -> DeliveryKind {
    let mut initial = false;
    let mut parent = record.parent_id;
    for _ in 0..by_id.len() {
        let Some(id) = parent else { break };
        let Some(ancestor) = by_id.get(&id) else {
            break;
        };
        match &*ancestor.stage {
            RECOVERY_STAGE => return DeliveryKind::Recovery,
            INITIAL_DELIVERY_STAGE => initial = true,
            _ => {}
        }
        parent = ancestor.parent_id;
    }
    if initial {
        DeliveryKind::Initial
    } else {
        DeliveryKind::Other
    }
}

/// Whether a record describes work that finished inside the invocation.
///
/// An unfinished or cancelled attempt was clipped when the report froze; its
/// duration is a lower bound and would drag every percentile down if treated as
/// an ordinary sample.
fn is_finished(outcome: ObservationOutcome) -> bool {
    !matches!(
        outcome,
        ObservationOutcome::Unfinished | ObservationOutcome::Cancelled
    )
}

fn stage_metrics(placed: &[Placed<'_>]) -> Vec<StageMetrics> {
    let mut grouped: BTreeMap<&str, Vec<&Placed<'_>>> = BTreeMap::new();
    for entry in placed {
        grouped.entry(&entry.record.stage).or_default().push(entry);
    }

    let mut stages: Vec<StageMetrics> = grouped
        .into_iter()
        .map(|(stage, entries)| {
            let intervals: Vec<Interval> = entries.iter().map(|entry| entry.interval()).collect();
            let occupancy = sweep(&intervals);
            let mut samples: Vec<u64> = entries
                .iter()
                .filter(|entry| is_finished(entry.record.outcome))
                .map(|entry| entry.record.elapsed_us)
                .collect();
            let mut outcomes: BTreeMap<String, usize> = BTreeMap::new();
            for entry in &entries {
                *outcomes
                    .entry(outcome_name(entry.record.outcome).to_string())
                    .or_default() += 1;
            }
            StageMetrics {
                stage: stage.to_string(),
                calls: entries.len(),
                wall_span_us: occupancy.wall_span_us,
                cumulative_us: occupancy.cumulative_us,
                peak_concurrency: occupancy.peak,
                latency: Percentiles::of(&mut samples),
                outcomes,
            }
        })
        .collect();

    // Most wall time first: the bottleneck view. Ties break by name so two runs
    // of the same shape print their rows in the same order.
    stages.sort_by(|left, right| {
        right
            .wall_span_us
            .cmp(&left.wall_span_us)
            .then_with(|| left.stage.cmp(&right.stage))
    });
    stages
}

fn proposal_metrics(placed: &[Placed<'_>]) -> Vec<ProposalMetrics> {
    let mut grouped: BTreeMap<u32, BTreeMap<&str, Vec<&Placed<'_>>>> = BTreeMap::new();
    for entry in placed {
        let Some(proposal_id) = entry.record.attribution.proposal_id else {
            continue;
        };
        if !PROPOSAL_STAGES.contains(&&*entry.record.stage) {
            continue;
        }
        grouped
            .entry(proposal_id)
            .or_default()
            .entry(&entry.record.stage)
            .or_default()
            .push(entry);
    }

    grouped
        .into_iter()
        .map(|(proposal_id, by_stage)| {
            let mut stages = BTreeMap::new();
            let mut delivery_cumulative_us = 0;
            for (stage, entries) in by_stage {
                let summary = summarize(&entries);
                if stage == INITIAL_DELIVERY_STAGE {
                    delivery_cumulative_us = summary.cumulative_us;
                }
                stages.insert(stage.to_string(), summary);
            }
            ProposalMetrics {
                proposal_id,
                stages,
                delivery_cumulative_us,
            }
        })
        .collect()
}

/// Groups bundle-attributed work, ignoring any record that also names a
/// proposal.
///
/// A helper share carries both a bundle and a proposal, and counting it here
/// would report every bundle as dominated by delivery — which is true of the
/// round and says nothing about the per-bundle work this table exists to show.
fn bundle_metrics(placed: &[Placed<'_>]) -> Vec<BundleMetrics> {
    let mut grouped: BTreeMap<u32, BTreeMap<&str, Vec<&Placed<'_>>>> = BTreeMap::new();
    for entry in placed {
        let Some(bundle_index) = entry.record.attribution.bundle_index else {
            continue;
        };
        if !BUNDLE_STAGES.contains(&&*entry.record.stage) {
            continue;
        }
        grouped
            .entry(bundle_index)
            .or_default()
            .entry(&entry.record.stage)
            .or_default()
            .push(entry);
    }

    grouped
        .into_iter()
        .map(|(bundle_index, by_stage)| BundleMetrics {
            bundle_index,
            stages: by_stage
                .into_iter()
                .map(|(stage, entries)| (stage.to_string(), summarize(&entries)))
                .collect(),
        })
        .collect()
}

/// Collapses one stage's records into a single row.
fn summarize(entries: &[&Placed<'_>]) -> ProposalStage {
    let intervals: Vec<Interval> = entries.iter().map(|entry| entry.interval()).collect();
    let occupancy = sweep(&intervals);
    ProposalStage {
        calls: entries.len(),
        cumulative_us: occupancy.cumulative_us,
        wall_span_us: occupancy.wall_span_us,
        max_us: entries
            .iter()
            .map(|entry| entry.record.elapsed_us)
            .max()
            .unwrap_or_default(),
    }
}

fn delivery_metrics(
    placed: &[Placed<'_>],
    initial_http: Vec<Interval>,
    mut initial_http_samples: Vec<u64>,
    recovery_http_attempts: usize,
) -> DeliveryMetrics {
    let active: Vec<Interval> = placed
        .iter()
        .filter(|entry| &*entry.record.stage == INITIAL_DELIVERY_STAGE)
        .map(Placed::interval)
        .collect();

    let initial = sweep(&initial_http);
    let completed = initial_http_samples.len() as f64;
    let initial_http_throughput = if initial.wall_span_us == 0 {
        0.0
    } else {
        completed / (initial.wall_span_us as f64 / 1_000_000.0)
    };

    DeliveryMetrics {
        active_shares: sweep(&active),
        initial_http: initial,
        initial_http_latency: Percentiles::of(&mut initial_http_samples),
        initial_http_throughput,
        recovery_http_attempts,
        post_outcomes: outcomes_for(placed, "helper::post_share"),
        acceptance_outcomes: outcomes_for(placed, "helper::persist_acceptance"),
        http_status: statuses_for(placed, HELPER_POST_STAGE),
    }
}

fn outcomes_for(placed: &[Placed<'_>], stage: &str) -> BTreeMap<String, usize> {
    let mut outcomes = BTreeMap::new();
    for entry in placed.iter().filter(|entry| &*entry.record.stage == stage) {
        *outcomes
            .entry(outcome_name(entry.record.outcome).to_string())
            .or_default() += 1;
    }
    outcomes
}

fn statuses_for(placed: &[Placed<'_>], stage: &str) -> BTreeMap<u16, usize> {
    let mut statuses = BTreeMap::new();
    for entry in placed.iter().filter(|entry| &*entry.record.stage == stage) {
        if let Some(status) = entry.record.http_status {
            *statuses.entry(status).or_default() += 1;
        }
    }
    statuses
}

/// Names an outcome without depending on its `Debug` shape.
fn outcome_name(outcome: ObservationOutcome) -> &'static str {
    match outcome {
        ObservationOutcome::Succeeded => "succeeded",
        ObservationOutcome::Failed => "failed",
        ObservationOutcome::Pending => "pending",
        ObservationOutcome::Rejected => "rejected",
        ObservationOutcome::Cancelled => "cancelled",
        ObservationOutcome::NoWork => "no_work",
        ObservationOutcome::Reused => "reused",
        ObservationOutcome::Unfinished => "unfinished",
        ObservationOutcome::PossiblyDispatched => "possibly_dispatched",
        _ => "unknown",
    }
}
