//! Driving one round to quiescence over a [`RoundExecutor`].
//!
//! [`vote_work`](crate::vote_work) executes **one** obligation per call: it
//! resolves a host-selected step to an obligation under the right lock, runs
//! it, and returns. This module is the layer above: it re-plans from durable
//! state, chooses what to run, paces re-polls, isolates failures, and stops
//! with a reason the host can act on. It owns no classification — every
//! decision about what a step *means* stays in the planner and the executor.
//!
//! Two properties are worth stating outright because they are what a host
//! loop tends to get wrong:
//!
//! - **Selection is always from a plan the driver read itself.** The plan on a
//!   [`RoundStepOutcome`] is a host-facing projection, not a control input, so
//!   the driver never selects from it.
//! - **The host context is read once per dispatch, not once per run.** A round
//!   can take minutes, and a long proof can cross the last-moment or vote-end
//!   boundary, so the step that follows must plan against the clock it
//!   actually runs under. This does not weaken "scope is captured once": each
//!   step still captures one context at entry and reads it for its whole
//!   duration.
//!
//! This file is the facade: the types a host touches, and the entry point.
//! Mechanism lives in children, one per responsibility, in the order one pass
//! uses them: `run_loop` (plan, admit, dispatch, fold, repeat), `selection`
//! (which steps a fresh plan admits, and under which lock), `signing` (whether
//! the host still owes a delegation signature), `dispatch` (running one
//! admitted wave concurrently), `run_ledger` (what the run has accumulated and
//! whether an outcome ends it), `quiescence` (why a plan with nothing
//! dispatchable stops), `tally` (run-relative ballot progress), `policy`
//! (pacing and failure isolation) and `progress` (driver-level events).

mod dispatch;
mod policy;
mod progress;
mod quiescence;
mod run_ledger;
mod run_loop;
pub(crate) mod selection;
mod signing;
pub(crate) mod tally;

#[cfg(test)]
mod tests;

pub use policy::{FailureIsolation, ProgressBaseline, RoundDrivePolicy};
pub use progress::{
    NoopRoundDriveReporter, RoundDriveEvent, RoundDriveReporter, RoundDriveReporterBridge,
};
pub use quiescence::RoundQuiescence;
pub use tally::RoundWorkTally;

use crate::{
    delegate::SignedDelegationBundle,
    session::{NextStep, RoundPlan},
    ChainSubmissionControl, ChainSubmissionResult, ChainTransport, RoundExecutor, RoundHostContext,
    RoundStepFailure, VoteShareDeliveryReport,
};

/// One failure the run kept, with the bundle it isolated.
///
/// Non-exhaustive: a run reports what it observed, and what there is to
/// observe grows. Hosts read these fields; they never build one.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RoundStepFailureRecord {
    pub step: Option<NextStep>,
    /// The bundle this failure is attributed to, whatever the isolation
    /// policy. `None` when it belongs to no bundle, such as a plan that could
    /// not be read.
    ///
    /// Attribution, not isolation: under
    /// [`FailureIsolation::StopRound`] the run ends and nothing is suppressed,
    /// so a host must read [`RoundRunReport::skipped_bundles`] — the
    /// authoritative list — to learn what was actually skipped.
    pub bundle_index: Option<u32>,
    pub failure: RoundStepFailure,
}

/// Everything one run of a round did.
///
/// A run always produces a report: failures are isolated and recorded rather
/// than returned, so a partly failed round still reports the durable effects
/// of every obligation that completed.
///
/// Non-exhaustive for the same reason as [`RoundStepFailureRecord`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RoundRunReport {
    pub quiescence: RoundQuiescence,
    /// The last plan the driver read.
    pub plan: Option<RoundPlan>,
    pub tally: RoundWorkTally,
    /// Every failure, in dispatch order. A non-empty list does not imply
    /// [`RoundQuiescence::Failures`]: a run can isolate one bundle and still
    /// finish the rest with nothing left to do.
    pub failures: Vec<RoundStepFailureRecord>,
    /// Bundles a failure isolated for the rest of the run. Empty under
    /// [`FailureIsolation::StopRound`], which stops the run instead of
    /// suppressing anything.
    pub skipped_bundles: Vec<u32>,
    /// Every chain outcome the run observed, in the order it saw them, each
    /// bound to the step that produced it.
    ///
    /// Not only terminal ones: a submission still tracking is recorded here
    /// too, as is one a failing step saw before it failed. Match on the
    /// [`ChainSubmissionResult`] rather than treating an entry's presence as
    /// an ending.
    pub chain_outcomes: Vec<(NextStep, ChainSubmissionResult)>,
    pub share_deliveries: Vec<VoteShareDeliveryReport>,
    /// Delegation bundles the run signed, in the order it produced them.
    ///
    /// Signed, not necessarily submitted: a step cancelled between signing and
    /// building its chain request returns the bundle it produced, and so does
    /// one that failed at dispatch. `SignedDelegationBundle` carries no
    /// submission state of its own, so do not try to read one off it — every
    /// bundle presents as ready to submit.
    ///
    /// To learn what actually happened to bundle `n`, read `plan`: its
    /// `delegation_statuses` entry for that bundle carries the durable phase,
    /// the transaction hash, and whether the submission is terminal. That plan
    /// is re-read after the wave, so it describes the round this run left.
    /// `chain_outcomes` names what each step saw on the chain, keyed by the
    /// step, which names the bundle.
    pub delegations: Vec<SignedDelegationBundle>,
}

/// Supplies the per-step host inputs.
///
/// Called once per dispatch. See the module documentation for why a run cannot
/// freeze one context for its whole duration.
///
/// # The signer is a property of the round, not of the bundle
///
/// A context names no bundle, so repeated calls cannot tell the driver *which*
/// bundle each answer is for. An implementation must therefore offer the same
/// [`DelegationSigner`] mode for every bundle of a round; what may change
/// between calls is timing, the fleet, and cancellation.
///
/// This matters because the signature handoff is round-wide by design: with a
/// Keystone device the voter signs every bundle before any of them is
/// broadcast, so the driver reports every bundle still owing a stored
/// signature rather than one wave's worth at a time. It can only know the mode
/// of bundles it has admitted, and it takes those as speaking for the round.
/// A source that answered `Keystone(Stored)` for one bundle and a
/// self-signing mode for another it had not yet been asked about would be told
/// to store a signature for a bundle that never needed one, and the run would
/// not progress until it did.
///
/// Within a single wave the driver does not rely on that: a bundle whose own
/// admitted context signs during its step is never reported as owing stored
/// material.
pub trait RoundHostSource: Send + Sync {
    fn host_context(&self) -> RoundHostContext;
}

/// Adapts a closure to [`RoundHostSource`].
pub struct RoundHostSourceBridge<F> {
    host: F,
}

impl<F> RoundHostSourceBridge<F> {
    pub fn new(host: F) -> Self {
        Self { host }
    }
}

impl<F> RoundHostSource for RoundHostSourceBridge<F>
where
    F: Fn() -> RoundHostContext + Send + Sync,
{
    fn host_context(&self) -> RoundHostContext {
        (self.host)()
    }
}

/// Drives one bound round to quiescence over a [`RoundExecutor`].
pub struct RoundDriver<'a, T> {
    executor: &'a RoundExecutor<T>,
    policy: RoundDrivePolicy,
}

impl<'a, T: ChainTransport> RoundDriver<'a, T> {
    /// A driver over `executor` with the default policy.
    pub fn new(executor: &'a RoundExecutor<T>) -> Self {
        Self {
            executor,
            policy: RoundDrivePolicy::default(),
        }
    }

    pub fn with_policy(mut self, policy: RoundDrivePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Runs the bound round until it is quiescent.
    ///
    /// Never returns `Err`: a run that could not plan at all reports
    /// [`RoundQuiescence::Failures`] carrying the planning failure, so a host
    /// has one shape to handle. Cancellation or an operation-epoch change ends
    /// the run at the next boundary as [`RoundQuiescence::Cancelled`] and is
    /// never retried — a detached prover may still hold the bundle lock for
    /// the epoch just left, so an immediate retry would queue behind work it
    /// has already abandoned.
    ///
    /// The loop itself is `run_loop`; every decision inside it belongs to a
    /// child named in the module documentation.
    pub async fn run(
        &self,
        host: &dyn RoundHostSource,
        control: &ChainSubmissionControl,
        events: &dyn RoundDriveReporter,
    ) -> RoundRunReport {
        self.drive(host, control, events).await
    }
}
