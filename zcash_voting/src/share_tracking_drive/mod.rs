//! Driving one round's helper shares to confirmation over repeated passes.
//!
//! [`share_tracking`](crate::share_tracking) performs **one** pass: it polls
//! every unconfirmed share of a round, seeks the confirmation quorum,
//! resubmits what is overdue, and writes what it learned. This module is the
//! layer above: it repeats that pass on the cadence the pass itself computes,
//! stops with a reason the host can act on, and observes cancellation between
//! and inside passes.
//!
//! It is the share-side counterpart of [`round_drive`](crate::round_drive),
//! and deliberately mirrors its shape — a policy, a host source read once per
//! pass, a synchronous reporter, and one report carrying a quiescence. Three
//! differences are worth stating, because they are structural rather than
//! incidental:
//!
//! - **Passes are strictly sequential.** A round run interleaves independent
//!   bundles; a tracking pass plans from the rows the previous pass wrote, so
//!   overlapping two would have them contend for the same share locks and
//!   re-poll helpers that were just answered.
//! - **The cadence belongs to the pass, not the driver.** The delay comes from
//!   the durable share rows under the timing policy. The driver supplies a
//!   wait of its own only after a failed pass, which computed none, and
//!   shortens either to what is left of the round — the boundary is the one
//!   thing about timing the pass does not know.
//! - **A round admits one run.** Lifecycle events overlap, and two runs over
//!   one round would double its helper traffic while breaking the sequencing
//!   above.

mod admission;
mod policy;
mod progress;
mod quiescence;
mod run_ledger;

#[cfg(test)]
mod tests;

pub use policy::ShareTrackingDrivePolicy;
pub use progress::{
    NoopShareTrackingReporter, ShareTrackingEvent, ShareTrackingReporter,
    ShareTrackingReporterBridge,
};
pub use quiescence::ShareTrackingQuiescence;

use std::time::{Duration, Instant};

use crate::{
    helper::client::HelperClient,
    round::VotingDb,
    round_drive::sleep_until_interrupted,
    share::ShareOperationScope,
    share_tracking::{
        track_pending_shares_recording_partial, ResubmittedShare, ShareKey, ShareTrackingParams,
        ShareTrackingReport,
    },
    ChainSubmissionControl,
};

/// Host inputs that can change between passes.
///
/// Read once per pass, not once per run, for the same reason a round's host
/// context is: a run can span hours, and the configured helper fleet or the
/// round's timing may be refreshed underneath it. A pass that started before
/// such a change still completes against the fleet it was given; the next one
/// sees the new value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareTrackingHostContext {
    /// Complete current helper fleet, already mapped to transport URLs.
    pub configured_helper_urls: Vec<String>,
    pub now_seconds: u64,
    /// Recovery closes here. Absent means the round has no end time, so shares
    /// can be polled but never classified overdue.
    pub vote_end_time_seconds: Option<u64>,
}

/// Supplies the per-pass host inputs.
pub trait ShareTrackingHostSource: Send + Sync {
    fn host_context(&self) -> ShareTrackingHostContext;
}

/// Adapts a closure to [`ShareTrackingHostSource`].
pub struct ShareTrackingHostSourceBridge<F> {
    host: F,
}

impl<F> ShareTrackingHostSourceBridge<F> {
    pub fn new(host: F) -> Self {
        Self { host }
    }
}

impl<F> ShareTrackingHostSource for ShareTrackingHostSourceBridge<F>
where
    F: Fn() -> ShareTrackingHostContext + Send + Sync,
{
    fn host_context(&self) -> ShareTrackingHostContext {
        (self.host)()
    }
}

/// Everything one tracking run did.
///
/// A run always produces a report: a failing pass is recorded rather than
/// returned, so a run that made durable progress before failing still reports
/// it.
///
/// Non-exhaustive, like [`RoundRunReport`](crate::round_drive::RoundRunReport):
/// a run reports what it observed, and what there is to observe grows. Hosts
/// read these fields; they never build one.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ShareTrackingRunReport {
    /// Why the run stopped.
    pub quiescence: ShareTrackingQuiescence,
    /// Passes that actually polled, successful or not.
    pub passes: u32,
    /// Shares durably confirmed during this run, in the order observed.
    pub confirmed: Vec<ShareKey>,
    /// Shares that reached a new helper during this run.
    pub resubmitted: Vec<ResubmittedShare>,
    /// Recovery attempts whose helper acceptance outcome remains unknown.
    pub ambiguous: Vec<ResubmittedShare>,
    /// Shares the last pass reported as beyond repair by retrying.
    ///
    /// Taken from the most recent pass rather than accumulated: a share can
    /// stop being unrecoverable when its material is restored, and a host
    /// showing a stale union would keep warning about a share that recovered.
    pub unrecoverable: Vec<ShareKey>,
    /// Messages from failed passes, in order.
    pub failures: Vec<String>,
}

/// Drives one round's unconfirmed helper shares to confirmation.
pub struct ShareTrackingDriver<'a> {
    database: &'a VotingDb,
    client: &'a HelperClient,
    round_id: &'a str,
    policy: ShareTrackingDrivePolicy,
}

impl<'a> ShareTrackingDriver<'a> {
    /// A driver over `round_id`'s shares with the default policy.
    pub fn new(database: &'a VotingDb, client: &'a HelperClient, round_id: &'a str) -> Self {
        Self {
            database,
            client,
            round_id,
            policy: ShareTrackingDrivePolicy::default(),
        }
    }

    pub fn with_policy(mut self, policy: ShareTrackingDrivePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Runs passes until the round's shares are quiescent.
    ///
    /// Never returns `Err`: a pass that fails is recorded and retried under
    /// the policy, so a host has one shape to handle. Cancellation or an
    /// operation-epoch change ends the run at the next boundary — between
    /// passes, during the wait, or inside a pass, which observes the same
    /// signal through its cancel callback.
    ///
    /// A round admits one run. Starting a second while the first is *live*
    /// returns [`ShareTrackingQuiescence::AlreadyDriving`] without polling
    /// anything, because the work it was started for is already being done and
    /// two interleaved runs would only double the round's helper traffic.
    ///
    /// A second run started while the first is on its way out takes the round
    /// over instead: admission is released when this future completes or is
    /// dropped, and a caller waits briefly for that before concluding a run is
    /// live. That is what lets a host cancel a run and start its replacement
    /// without the round falling between them — though awaiting the cancelled
    /// run first removes the question entirely.
    pub async fn run(
        &self,
        host: &dyn ShareTrackingHostSource,
        control: &ChainSubmissionControl,
        events: &dyn ShareTrackingReporter,
    ) -> ShareTrackingRunReport {
        let started_at = Instant::now();
        self.run_on_clock(host, control, events, &|| started_at.elapsed().as_secs())
            .await
    }

    /// [`run`](Self::run), reading elapsed wall time through `monotonic_seconds`.
    ///
    /// Seconds since the run began, from a clock only this layer needs: the
    /// vote-end boundary is in host time, and closing on it correctly means
    /// knowing how much of the window a pass just spent. Injected for the same
    /// reason `track_pending_shares_with_elapsed` injects its own — a test
    /// cannot make a pass take an hour.
    async fn run_on_clock(
        &self,
        host: &dyn ShareTrackingHostSource,
        control: &ChainSubmissionControl,
        events: &dyn ShareTrackingReporter,
        monotonic_seconds: &(dyn Fn() -> u64 + Send + Sync),
    ) -> ShareTrackingRunReport {
        let mut run = run_ledger::Run::default();
        // The epoch comes first, and the wallet is read under it. Captured the
        // other way round, a switch landing between the two would leave the
        // run holding the old wallet's admission while accepting the new
        // wallet's epoch as its own — and every pass re-reads the sidecar's
        // current wallet, so it would drive the new wallet's shares beside a
        // correctly admitted run of its own.
        let entry_epoch = control.operation_epoch();
        let interrupted = || control.is_cancelled() || control.operation_epoch() != entry_epoch;
        if interrupted() {
            return run.finish(ShareTrackingQuiescence::Cancelled);
        }
        let scope = ShareOperationScope::capture(self.database);
        let round =
            admission::RoundKey::new(self.database.sidecar_id(), scope.wallet_id(), self.round_id);
        let _admission = match admission::claim_round(&round, &interrupted).await {
            admission::RoundClaim::Admitted(admission) => admission,
            admission::RoundClaim::HeldByALiveRun => {
                return run.finish(ShareTrackingQuiescence::AlreadyDriving)
            }
            admission::RoundClaim::Interrupted => {
                return run.finish(ShareTrackingQuiescence::Cancelled)
            }
        };
        let mut consecutive_failures = 0u32;

        loop {
            if interrupted() {
                return run.finish(ShareTrackingQuiescence::Cancelled);
            }
            // The sidecar's wallet is the subject of every pass, re-read by
            // each one. A host that switched it under this run is driving
            // something else now, and this run's admission does not cover it.
            if self.database.wallet_id() != scope.wallet_id() {
                return run.finish(ShareTrackingQuiescence::Cancelled);
            }
            let host_context = host.host_context();
            // Checked before the pass, not after: recovery is already closed
            // at this point, so a pass here could only re-poll shares it
            // cannot act on.
            if let Some(vote_end) = host_context.vote_end_time_seconds {
                if host_context.now_seconds >= vote_end {
                    return run.finish(ShareTrackingQuiescence::VoteEndReached);
                }
            }
            // Only reachable with a zero budget: once a pass has run, the
            // check below stops the run before it waits for one it cannot
            // dispatch.
            if self.budget_spent(run.passes) {
                return run.finish_exhausted();
            }

            run.passes += 1;
            events.report(ShareTrackingEvent::PassStarted { pass: run.passes });
            let pass_started_at = monotonic_seconds();
            let outcome = self.pass(&host_context, &interrupted).await;
            let pass_seconds = monotonic_seconds().saturating_sub(pass_started_at);

            let delay = match outcome {
                Ok(report) => {
                    consecutive_failures = 0;
                    let cancelled = report.cancelled;
                    let next_delay = report.next_delay_seconds;
                    // What the round owed when this pass began, which only the
                    // pass can say: a pass that confirmed and resubmitted
                    // nothing looks the same whether it had no share to walk
                    // or walked one another task confirmed underneath it.
                    let owed_nothing_at_entry = report.unconfirmed_at_entry == 0;
                    let first_pass = run.passes == 1;
                    // A cancelled pass walked a prefix, so its `unrecoverable`
                    // is a partial snapshot like a failed pass's.
                    if cancelled {
                        run.absorb_partial(&report);
                    } else {
                        run.absorb(&report);
                    }
                    events.report(ShareTrackingEvent::PassFinished {
                        pass: run.passes,
                        report: Box::new(report),
                    });

                    if cancelled || interrupted() {
                        return run.finish(ShareTrackingQuiescence::Cancelled);
                    }
                    let Some(seconds) = next_delay else {
                        // No delay means no unconfirmed share is left to poll.
                        return run.finish(if first_pass && owed_nothing_at_entry {
                            ShareTrackingQuiescence::NothingToTrack
                        } else {
                            ShareTrackingQuiescence::AllConfirmed
                        });
                    };
                    Duration::from_secs(seconds)
                }
                Err(failure) => {
                    consecutive_failures += 1;
                    // Before the run can stop: a pass commits each
                    // confirmation as it reaches it, and the next pass walks
                    // only unconfirmed shares, so an effect dropped here is
                    // one no later pass can rediscover.
                    run.absorb_partial(&failure.partial);
                    run.failures.push(failure.message.clone());
                    let cancelled = failure.partial.cancelled;
                    events.report(ShareTrackingEvent::PassFailed {
                        pass: run.passes,
                        message: failure.message,
                        partial: Box::new(failure.partial),
                    });
                    // Cancellation outranks a failure verdict, as it does on
                    // the successful path. A pass that failed because the host
                    // was draining the run says nothing about the round's
                    // health, and `Failing` would send a host looking for a
                    // fault it caused.
                    if cancelled || interrupted() {
                        return run.finish(ShareTrackingQuiescence::Cancelled);
                    }
                    if consecutive_failures >= self.policy.max_consecutive_failures {
                        let messages = run.recent_failures(consecutive_failures);
                        return run.finish(ShareTrackingQuiescence::Failing { messages });
                    }
                    self.policy.failure_retry
                }
            };
            let delay = wait_within_voting_window(delay, &host_context, pass_seconds);

            // Checked before the wait, not after it: waiting out a delay only
            // to find the budget spent would report exhaustion a full pass
            // interval late, and a host draining the run would block on it.
            if self.budget_spent(run.passes) {
                return run.finish_exhausted();
            }
            events.report(ShareTrackingEvent::AwaitingNextPass { delay });
            if !sleep_until_interrupted(delay, control, entry_epoch).await {
                return run.finish(ShareTrackingQuiescence::Cancelled);
            }
        }
    }

    /// True once a host-set pass budget leaves no pass to dispatch.
    ///
    /// A run without a budget is bounded by vote end, confirmation,
    /// cancellation, and the consecutive-failure limit instead.
    fn budget_spent(&self, passes: u32) -> bool {
        self.policy
            .max_passes
            .is_some_and(|max_passes| passes >= max_passes)
    }

    /// One pass, with its error flattened to a message and its durable effects
    /// kept.
    ///
    /// The error type is not carried onto the report: a failed pass is a
    /// transient condition the driver retries, and every caller that acts on
    /// it acts on the fact of the failure rather than its variant. What the
    /// pass had already committed is carried, because nothing else can
    /// recover it.
    async fn pass(
        &self,
        host_context: &ShareTrackingHostContext,
        interrupted: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<ShareTrackingReport, FailedPass> {
        let params = ShareTrackingParams {
            round_id: self.round_id,
            configured_server_urls: &host_context.configured_helper_urls,
            now_seconds: host_context.now_seconds,
            vote_end_time_seconds: host_context.vote_end_time_seconds,
            policy: self.policy.timing,
            #[cfg(test)]
            random_bytes: &crate::share_tracking::os_random_bytes,
        };
        track_pending_shares_recording_partial(self.database, &params, self.client, interrupted)
            .await
            .map_err(|failure| FailedPass {
                message: failure.error.to_string(),
                partial: failure.partial,
            })
    }
}

/// A failed pass as the driver sees it: why, and what it still did.
struct FailedPass {
    message: String,
    partial: ShareTrackingReport,
}

/// `delay`, shortened so a wait never spans the round's vote end.
///
/// The pass computes its delay from share rows alone, which say when a share
/// is next worth polling and nothing about when the round closes. Sleeping
/// past vote end would leave the run holding a round it can no longer act on
/// until a whole poll interval had elapsed — and after a failed pass, would
/// spend the last usable retry of the window on a wait.
///
/// `pass_seconds` is how much of the window the pass that produced this delay
/// consumed.
///
/// A round whose host reports no vote end has no such boundary, and keeps the
/// delay it was given.
fn wait_within_voting_window(
    delay: Duration,
    host_context: &ShareTrackingHostContext,
    pass_seconds: u64,
) -> Duration {
    let Some(vote_end) = host_context.vote_end_time_seconds else {
        return delay;
    };
    // `now_seconds` was read before the pass, so the pass's own duration has
    // already been spent out of the window. Charging it here is what keeps a
    // slow pass followed by a capped wait from landing past the boundary.
    let spent_at = host_context.now_seconds.saturating_add(pass_seconds);
    delay.min(Duration::from_secs(vote_end.saturating_sub(spent_at)))
}
