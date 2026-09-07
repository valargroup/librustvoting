//! Driver-level observations, on top of per-step [`RoundStepProgress`].

use std::time::Duration;

use super::RoundWorkTally;
use crate::{
    session::{NextStep, RoundPlan},
    RoundStepDisposition, RoundStepFailureKind, RoundStepProgress,
};

/// One observation from a round run.
///
/// Every variant that describes work names the [`NextStep`] it belongs to.
/// The driver interleaves bundles, and a bare [`RoundStepProgress`] is not
/// attributable on its own: `ChainOutcome` and `TreeSynced` carry no subject.
/// A host that read them from one stream while three bundles ran would
/// misattribute them.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum RoundDriveEvent {
    /// The driver re-planned from durable state. This plan, and never the
    /// plan on a step outcome, is what the driver selects from.
    PlanRefreshed {
        plan: Box<RoundPlan>,
        tally: RoundWorkTally,
    },
    /// The driver is about to dispatch this obligation.
    StepSelected { step: NextStep },
    /// Progress from inside a running step.
    StepProgress {
        step: NextStep,
        progress: RoundStepProgress,
    },
    /// The step returned; `disposition` is the executor's own answer.
    StepFinished {
        step: NextStep,
        disposition: RoundStepDisposition,
    },
    /// The step failed. The run continues or stops per the policy's
    /// [`FailureIsolation`](super::FailureIsolation); either way the failure
    /// is also in the report.
    StepFailed {
        step: NextStep,
        kind: RoundStepFailureKind,
        message: String,
    },
    /// The step did not finish; the driver waits `delay` and dispatches it
    /// again.
    ///
    /// Not only chain tracking: a share that became tracking-owned after
    /// selection, and a confirmed vote whose delivery waits on ambiguous
    /// attempts, are paced by the same
    /// [`pending_repoll`](super::RoundDrivePolicy::pending_repoll). Read the
    /// step to tell them apart rather than labelling every one of these as
    /// chain work.
    AwaitingRepoll { step: NextStep, delay: Duration },
    /// Every remaining obligation on this bundle is skipped for this run.
    BundleSkipped { bundle_index: u32, after: NextStep },
}

/// Synchronous observer for [`RoundDriveEvent`].
///
/// Called from several concurrent bundle tasks, so an implementation must be
/// internally synchronised and must not block.
pub trait RoundDriveReporter: Send + Sync {
    fn report(&self, event: RoundDriveEvent);
}

/// Reporter for hosts that need only the run report.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopRoundDriveReporter {}

impl RoundDriveReporter for NoopRoundDriveReporter {
    fn report(&self, _event: RoundDriveEvent) {}
}

/// Adapts a closure to [`RoundDriveReporter`].
pub struct RoundDriveReporterBridge<F> {
    report: F,
}

impl<F> RoundDriveReporterBridge<F> {
    pub fn new(report: F) -> Self {
        Self { report }
    }
}

impl<F> RoundDriveReporter for RoundDriveReporterBridge<F>
where
    F: Fn(RoundDriveEvent) + Send + Sync,
{
    fn report(&self, event: RoundDriveEvent) {
        (self.report)(event);
    }
}

/// Forwards one step's [`RoundStepProgress`] to a [`RoundDriveReporter`],
/// naming the step it came from.
pub(super) struct StepReporter<'a> {
    step: NextStep,
    events: &'a dyn RoundDriveReporter,
}

impl<'a> StepReporter<'a> {
    pub(super) fn new(step: NextStep, events: &'a dyn RoundDriveReporter) -> Self {
        Self { step, events }
    }
}

impl crate::RoundStepProgressReporter for StepReporter<'_> {
    fn report(&self, progress: RoundStepProgress) {
        self.events.report(RoundDriveEvent::StepProgress {
            step: self.step.clone(),
            progress,
        });
    }
}
