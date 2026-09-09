//! Execution of one admitted obligation, independently of sibling completion.

use super::{progress::StepReporter, RoundDriveReporter};
use crate::{
    session::NextStep, ChainSubmissionControl, ChainTransport, RoundExecutor, RoundHostContext,
    RoundStepFailure, RoundStepOutcome,
};

pub(super) type DispatchResult = (usize, NextStep, Result<RoundStepOutcome, RoundStepFailure>);

/// Executes one step with the epoch captured before planning and host callbacks.
pub(super) async fn run<T: ChainTransport>(
    executor: &RoundExecutor<T>,
    sequence: usize,
    step: NextStep,
    host: RoundHostContext,
    control: &ChainSubmissionControl,
    entry_epoch: u64,
    events: &dyn RoundDriveReporter,
) -> DispatchResult {
    let reporter = StepReporter::new(step.clone(), events);
    let outcome = executor
        .advance_step_in_epoch(step.clone(), &host, control, entry_epoch, &reporter)
        .await;
    (sequence, step, outcome)
}
