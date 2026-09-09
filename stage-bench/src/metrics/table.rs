//! The run summary a reader actually looks at.
//!
//! Rendered rather than logged: the point of the table is that a bottleneck is
//! visible at a glance, which means fixed columns, one unit, and the phases
//! ordered by the wall time they occupied rather than by when they ran.
//!
//! Every duration is printed in seconds. The underlying records are
//! microseconds and `metrics.json` keeps them that way; the table is for
//! reading.

use std::fmt::Write;

use crate::manifest::Manifest;

use super::Metrics;

/// Renders one run's summary.
pub fn render(manifest: &Manifest, metrics: &Metrics) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "\n== stage-bench: {} ==", manifest.round_id);
    let _ = writeln!(
        out,
        "{} proposals x {} bundles, {} configured helpers, {} bundles and {} proofs wide",
        manifest.proposals,
        manifest.bundles,
        manifest.configured_helpers,
        manifest.bundle_concurrency,
        manifest.proof_concurrency
    );
    if manifest.confirm_concurrency > 1 {
        let _ = writeln!(
            out,
            "confirmation: EXPERIMENT — {} focused confirmations at a time, replacing the \
             shipped tracker. Not a measurement of shipped behaviour.",
            manifest.confirm_concurrency
        );
    }
    let _ = writeln!(
        out,
        "quiescence {} ({} of {} proposals complete), drive {:.1}s, tracking {:.1}s",
        manifest.quiescence_kind,
        manifest.completed_proposals,
        manifest.proposals,
        manifest.round_drive_seconds,
        manifest.tracking_seconds
    );

    if !metrics.complete {
        let _ = writeln!(
            out,
            "\n!! INCOMPLETE CAPTURE — concurrency and percentiles below are floors, not \
             measurements. Re-run with a larger --max-records."
        );
        for reason in &metrics.incomplete {
            let _ = writeln!(out, "   {reason}");
        }
    }

    let delivery = &metrics.delivery;
    let _ = writeln!(out, "\n-- helper delivery --");
    let _ = writeln!(
        out,
        "  active shares    peak {:>4}  avg {:>6.1}  over {:>7.1}s  ({} workflows)",
        delivery.active_shares.peak,
        delivery.active_shares.average,
        seconds(delivery.active_shares.wall_span_us),
        delivery.active_shares.samples,
    );
    let _ = writeln!(
        out,
        "  initial POSTs    peak {:>4}  avg {:>6.1}  over {:>7.1}s  ({} attempts, {:.1}/s)",
        delivery.initial_http.peak,
        delivery.initial_http.average,
        seconds(delivery.initial_http.wall_span_us),
        delivery.initial_http.samples,
        delivery.initial_http_throughput,
    );
    let _ = writeln!(
        out,
        "  POST latency     p50 {:>7.3}s  p95 {:>7.3}s  p99 {:>7.3}s  max {:>7.3}s",
        seconds(delivery.initial_http_latency.p50_us),
        seconds(delivery.initial_http_latency.p95_us),
        seconds(delivery.initial_http_latency.p99_us),
        seconds(delivery.initial_http_latency.max_us),
    );
    let _ = writeln!(
        out,
        "  recovery POSTs   {}   post outcomes {}   acceptances {}",
        delivery.recovery_http_attempts,
        render_counts(&delivery.post_outcomes),
        render_counts(&delivery.acceptance_outcomes),
    );
    if !delivery.http_status.is_empty() {
        let statuses: Vec<String> = delivery
            .http_status
            .iter()
            .map(|(status, count)| format!("{status}={count}"))
            .collect();
        let _ = writeln!(out, "  POST statuses    {}", statuses.join(" "));
    }

    let _ = writeln!(out, "\n-- phases, by wall time --");
    let _ = writeln!(
        out,
        "  {:<44} {:>6} {:>9} {:>9} {:>5} {:>8} {:>8}",
        "stage", "calls", "wall(s)", "sum(s)", "peak", "p50(s)", "p95(s)"
    );
    for stage in metrics.stages.iter().take(STAGE_ROWS) {
        let _ = writeln!(
            out,
            "  {:<44} {:>6} {:>9.2} {:>9.2} {:>5} {:>8.3} {:>8.3}",
            truncate(&stage.stage, 44),
            stage.calls,
            seconds(stage.wall_span_us),
            seconds(stage.cumulative_us),
            stage.peak_concurrency,
            seconds(stage.latency.p50_us),
            seconds(stage.latency.p95_us),
        );
    }

    if !metrics.proposals.is_empty() {
        let _ = writeln!(out, "\n-- per proposal, slowest delivery first --");
        let _ = writeln!(
            out,
            "  {:>8} {:>10} {:>10} {:>10} {:>10} {:>10}",
            "proposal", "zkp2(s)", "queue(s)", "deliver(s)", "post(s)", "confirm(s)"
        );
        let mut proposals: Vec<&super::ProposalMetrics> = metrics.proposals.iter().collect();
        proposals.sort_by(|left, right| {
            right
                .delivery_cumulative_us
                .cmp(&left.delivery_cumulative_us)
                .then_with(|| left.proposal_id.cmp(&right.proposal_id))
        });
        for proposal in proposals.iter().take(PROPOSAL_ROWS) {
            let _ = writeln!(
                out,
                "  {:>8} {:>10.2} {:>10.2} {:>10.2} {:>10.2} {:>10.2}",
                proposal.proposal_id,
                stage_seconds(&proposal.stages, "zkp2::build_vote_commitment"),
                stage_seconds(&proposal.stages, "helper::delivery_queue_wait"),
                stage_seconds(&proposal.stages, "helper::active_delivery"),
                stage_seconds(&proposal.stages, "helper::post_share"),
                stage_seconds(&proposal.stages, "helper::confirmation_quorum"),
            );
        }
        if proposals.len() > PROPOSAL_ROWS {
            let _ = writeln!(
                out,
                "  ... {} more in metrics.json",
                proposals.len() - PROPOSAL_ROWS
            );
        }
    }

    if !metrics.bundles.is_empty() {
        // Separate from the proposal table because these are paid once per
        // bundle, not once per question: a wide ballot pays delegation three
        // times and proving a hundred, and one table would hide that.
        let _ = writeln!(out, "\n-- per bundle --");
        let _ = writeln!(
            out,
            "  {:>8} {:>12} {:>10} {:>10} {:>10} {:>10}",
            "bundle", "delegate(s)", "zkp1(s)", "batch(s)", "chain(s)", "steps(s)"
        );
        for bundle in &metrics.bundles {
            let _ = writeln!(
                out,
                "  {:>8} {:>12.2} {:>10.2} {:>10.2} {:>10.2} {:>10.2}",
                bundle.bundle_index,
                stage_seconds(&bundle.stages, "delegation::setup"),
                stage_seconds(&bundle.stages, "zkp1::build_and_prove_delegation"),
                stage_seconds(&bundle.stages, "vote::prepare_atomic_vote_batch"),
                stage_seconds(&bundle.stages, "chain::advance_until_terminal_in_epoch"),
                stage_seconds(&bundle.stages, "round::advance_step"),
            );
        }
    }

    let _ = writeln!(out, "\n-- invocations --");
    for invocation in &metrics.invocations {
        let _ = writeln!(
            out,
            "  {:<34} {:<10} {:>8.2}s  {:<10} {} records",
            truncate(&invocation.source, 34),
            truncate(&invocation.operation, 10),
            seconds(invocation.elapsed_us),
            invocation.outcome,
            invocation.records,
        );
    }
    let _ = writeln!(out, "\nrun directory: {}", manifest.run_dir.display());
    out
}

/// Phase rows printed before the reader stops reading.
///
/// The rest are in `metrics.json`. A table long enough to scroll is a table
/// nobody reads the top of, and the top is where the bottleneck is.
const STAGE_ROWS: usize = 18;

/// Proposal rows printed. A 37-proposal run would otherwise bury everything
/// above it.
const PROPOSAL_ROWS: usize = 12;

fn stage_seconds(
    stages: &std::collections::BTreeMap<String, super::ProposalStage>,
    stage: &str,
) -> f64 {
    stages
        .get(stage)
        .map(|entry| seconds(entry.cumulative_us))
        .unwrap_or_default()
}

fn seconds(microseconds: u64) -> f64 {
    microseconds as f64 / 1_000_000.0
}

fn render_counts(counts: &std::collections::BTreeMap<String, usize>) -> String {
    if counts.is_empty() {
        return "none".to_string();
    }
    counts
        .iter()
        .map(|(name, count)| format!("{name}={count}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn truncate(text: &str, width: usize) -> String {
    if text.len() <= width {
        return text.to_string();
    }
    format!("{}...", &text[..width.saturating_sub(3)])
}
