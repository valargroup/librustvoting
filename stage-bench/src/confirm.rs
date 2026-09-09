//! Confirming a round's shares concurrently, as an explicit experiment.
//!
//! # Why this is a separate mode
//!
//! The shipped path is `ShareTrackingDriver`, and its pass walks a round's
//! unconfirmed shares **one at a time**
//! (`zcash_voting::share_tracking::walk_pending_shares`). The four-way
//! `SHARE_STATUS_MAX_CONCURRENT_POLLS` inside it parallelises the quorum search
//! *across helpers for one share*, not across shares, so a one-helper fleet
//! polls at a strict concurrency of one — which a 1,776-share round pays for
//! one network round trip at a time.
//!
//! Making that walk concurrent is a change to helper-share behaviour, which
//! `AGENTS.md` gates behind `docs/helper_submission_invariants.md` and its named
//! regression tests. It is not something a benchmark may do on the side, and
//! this module does not do it: the SDK is unmodified.
//!
//! What this does instead is measure the ceiling. `confirm_pending_share` is a
//! public, per-share entry point, and the invariants document is explicit that
//! the per-share operation lock is what keeps two callers off one share. So a
//! host may legitimately drive several focused confirmations at once over
//! *distinct* shares, and the time that takes is the evidence a decision about
//! the walk would need.
//!
//! # Why it replaces the tracking driver rather than joining it
//!
//! "A round admits one run", and interleaved runs re-poll shares the other has
//! just answered — doubling helper traffic for no added progress. This mode
//! therefore runs *instead of* `ShareTrackingDriver`, never beside it.
//!
//! # What its numbers are not
//!
//! Not a measurement of shipped behaviour. Focused confirmation also bypasses
//! the tracker's grace period and its fifteen-second ready-share cadence, so a
//! sweep is faster than the product for two reasons at once. The run manifest
//! records the mode so no reader can mistake one for the other.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use zcash_voting::round::VotingDb;
use zcash_voting::share_tracking::{
    confirm_pending_share_with_report, ShareConfirmationParams, ShareKey,
};
use zcash_voting::{HelperClient, ObservabilityOptions, OperationObservability};

use crate::events::{EventLog, PhaseEvent};
use crate::run_config::TrackingSummary;

/// How long a sweep waits before looking for shares that are still pending.
///
/// A share confirms only once its reveal transaction is on chain, so a sweep
/// that found nothing new gains nothing by re-polling immediately. Shorter than
/// the tracker's fifteen-second ready-share cadence because that cadence is
/// sized for an hour-long voting window and this mode exists to measure a floor.
const SWEEP_INTERVAL: Duration = Duration::from_secs(3);

/// The round a confirmation run acts on, and what it acts through.
///
/// Grouped rather than passed as four positional arguments: a sidecar, a
/// client, a round id, and a fleet transpose silently, and the pair that would
/// transpose here — round id and fleet, both `&str`-shaped — are exactly the
/// two whose mix-up would look like an unresponsive helper.
pub struct ConfirmationTarget<'a> {
    pub database: &'a Arc<VotingDb>,
    pub client: &'a HelperClient,
    pub round_id: &'a str,
    pub helper_urls: &'a [String],
}

/// What one concurrent confirmation run did.
pub struct ConfirmationRun {
    pub summary: TrackingSummary,
    /// One snapshot per focused confirmation call, in completion order.
    pub snapshots: Vec<OperationObservability>,
}

/// Confirms every unconfirmed share, `concurrency` at a time, until the round
/// is settled or `budget` expires.
///
/// Each sweep lists the round's unconfirmed shares once and drives a focused
/// confirmation for each, `concurrency` in flight. Shares that stay pending are
/// picked up by the next sweep; a share confirmed by one call is simply absent
/// from the next listing.
pub async fn confirm_concurrently(
    target: &ConfirmationTarget<'_>,
    concurrency: usize,
    budget: Duration,
    events: &EventLog,
) -> Result<ConfirmationRun> {
    let started = Instant::now();
    let mut snapshots = Vec::new();
    let mut sweeps = 0u32;
    let mut confirmed_total = 0usize;

    events.record(PhaseEvent::phase("confirm::started"));

    loop {
        let pending = zcash_voting::share::unconfirmed(target.database, target.round_id)
            .map_err(|error| anyhow::anyhow!("listing unconfirmed shares: {error:?}"))?;
        if pending.is_empty() {
            events.record(PhaseEvent::phase("confirm::settled"));
            break;
        }
        if started.elapsed() >= budget {
            eprintln!(
                "bench: concurrent confirmation hit its {budget:?} budget with {} shares \
                 still unconfirmed",
                pending.len()
            );
            events.record(PhaseEvent::phase("confirm::budget_expired"));
            break;
        }

        sweeps += 1;
        let remaining = budget.saturating_sub(started.elapsed());
        let confirmed = Arc::new(AtomicUsize::new(0));
        let sweep_started = Instant::now();

        let sweep = sweep_shares(
            target,
            pending.iter().map(share_key).collect(),
            concurrency,
            Arc::clone(&confirmed),
            remaining,
        )
        .await;
        snapshots.extend(sweep);

        let gained = confirmed.load(Ordering::Relaxed);
        confirmed_total += gained;
        let mut phase = PhaseEvent::phase("confirm::sweep_finished");
        phase.detail = Some(format!(
            "sweep {sweeps}: {gained} confirmed of {} pending in {:.1}s",
            pending.len(),
            sweep_started.elapsed().as_secs_f64()
        ));
        events.record(phase.clone());
        eprintln!(
            "bench: confirm sweep {sweeps}: {gained} of {} pending confirmed in {:.1}s \
             ({confirmed_total} total)",
            pending.len(),
            sweep_started.elapsed().as_secs_f64()
        );

        // A sweep that confirmed nothing means the chain has not caught up, not
        // that the fleet is unresponsive. Pausing avoids spending the budget on
        // answers that cannot have changed yet.
        if gained == 0 {
            tokio::time::sleep(SWEEP_INTERVAL).await;
        }
    }

    let outstanding = zcash_voting::share::unconfirmed(target.database, target.round_id)
        .map(|pending| pending.len())
        .unwrap_or_default();

    Ok(ConfirmationRun {
        summary: TrackingSummary {
            quiescence: if outstanding == 0 {
                "ConcurrentlySettled".to_string()
            } else {
                format!("ConcurrentBudgetExpired({outstanding} unconfirmed)")
            },
            passes: sweeps,
            confirmed: confirmed_total,
            resubmitted: 0,
            ambiguous: 0,
            unrecoverable: outstanding,
        },
        snapshots,
    })
}

/// Drives one sweep, keeping `concurrency` focused confirmations in flight.
///
/// Refills as each call returns rather than running fixed batches: a batch
/// waits on its slowest member, and a sweep of a thousand shares over a fleet
/// with one slow helper would spend most of its time at that width.
///
/// Each call carries its own diagnostics, so the returned snapshots are the
/// per-share confirmation timings. Their record ids are invocation-local, which
/// is why they stay separate values rather than being merged.
async fn sweep_shares(
    target: &ConfirmationTarget<'_>,
    mut pending: std::collections::VecDeque<ShareKey>,
    concurrency: usize,
    confirmed: Arc<AtomicUsize>,
    budget: Duration,
) -> Vec<OperationObservability> {
    let deadline = Instant::now() + budget;
    let mut in_flight = tokio::task::JoinSet::new();
    let mut snapshots = Vec::new();

    loop {
        while in_flight.len() < concurrency.max(1) {
            if Instant::now() >= deadline {
                break;
            }
            let Some(share) = pending.pop_front() else {
                break;
            };
            let database = Arc::clone(target.database);
            let client = target.client.clone();
            let round_id = target.round_id.to_string();
            let helper_urls = target.helper_urls.to_vec();
            let confirmed = Arc::clone(&confirmed);
            in_flight.spawn(async move {
                let params = ShareConfirmationParams {
                    round_id: &round_id,
                    share,
                    configured_server_urls: &helper_urls,
                    now_seconds: now_seconds(),
                };
                let (result, snapshot) = confirm_pending_share_with_report(
                    &database,
                    &params,
                    &client,
                    &|| false,
                    Some(OPTIONS_PER_SHARE),
                )
                .await
                .into_parts();
                if matches!(result, Ok(report) if report.confirmed) {
                    confirmed.fetch_add(1, Ordering::Relaxed);
                }
                snapshot
            });
        }

        let Some(finished) = in_flight.join_next().await else {
            break;
        };
        // A panicking confirmation is the task's own failure, not the round's;
        // the share stays unconfirmed and the next sweep picks it up.
        if let Ok(Some(snapshot)) = finished {
            snapshots.push(snapshot);
        }
    }

    snapshots
}

/// Records retained per focused confirmation.
///
/// One share's confirmation is a handful of stages, and a sweep produces one
/// report per share: the default cap would be orders of magnitude more than any
/// single call can fill, at a cost paid once per share.
const OPTIONS_PER_SHARE: ObservabilityOptions = ObservabilityOptions {
    max_records: 64,
    max_summary_groups: 64,
    max_active_stages: 64,
};

fn share_key(record: &zcash_voting::ShareDelegationRecord) -> ShareKey {
    ShareKey {
        bundle_index: record.bundle_index,
        proposal_id: record.proposal_id,
        share_index: record.share_index,
    }
}

fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}
