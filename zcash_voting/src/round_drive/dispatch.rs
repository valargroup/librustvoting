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
pub(super) async fn run<T: ChainTransport>(
    executor: &RoundExecutor<T>,
    dispatches: Vec<(NextStep, RoundHostContext)>,
    control: &ChainSubmissionControl,
    events: &dyn RoundDriveReporter,
) -> Vec<DispatchResult> {
    join_all(dispatches.into_iter().map(|(step, host_context)| {
        let dispatched_step = step.clone();
        async move {
            let reporter = StepReporter::new(dispatched_step.clone(), events);
            let outcome = executor
                .advance_step(dispatched_step.clone(), &host_context, control, &reporter)
                .await;
            (dispatched_step, outcome)
        }
    }))
    .await
}
