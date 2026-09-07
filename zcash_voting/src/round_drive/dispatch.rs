//! Concurrent execution of one driver-selected dispatch wave.

use futures_util::future::join_all;

use super::{progress::StepReporter, RoundDriveReporter};
use crate::{
    session::NextStep, ChainSubmissionControl, ChainTransport, RoundExecutor, RoundHostContext,
    RoundStepFailure, RoundStepOutcome,
};

pub(super) type DispatchResult = (NextStep, Result<RoundStepOutcome, RoundStepFailure>);

/// Runs every already-admitted step concurrently and returns results in the
/// same order as `dispatches`.
///
/// Every step inherits the run's `entry_epoch` rather than capturing its own.
/// The driver planned, built each host context and read stored signing
/// material before reaching here, and a host that switched epoch across that
/// gap must interrupt these steps, not be adopted by them.
pub(super) async fn run<T: ChainTransport>(
    executor: &RoundExecutor<T>,
    dispatches: Vec<(NextStep, RoundHostContext)>,
    control: &ChainSubmissionControl,
    entry_epoch: u64,
    events: &dyn RoundDriveReporter,
) -> Vec<DispatchResult> {
    join_all(dispatches.into_iter().map(|(step, host_context)| {
        let dispatched_step = step.clone();
        async move {
            let reporter = StepReporter::new(dispatched_step.clone(), events);
            let outcome = executor
                .advance_step_in_epoch(
                    dispatched_step.clone(),
                    &host_context,
                    control,
                    entry_epoch,
                    &reporter,
                )
                .await;
            (dispatched_step, outcome)
        }
    }))
    .await
}
