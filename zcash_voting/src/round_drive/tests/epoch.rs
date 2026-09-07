//! A dispatch belongs to the run that decided on it, not to the epoch that
//! happens to be current when the step finally begins.

use std::sync::Arc;

use super::fixtures::*;
use crate::{
    session::NextStep, ChainSubmissionControl, NoopRoundStepProgressReporter, RoundStepDisposition,
};

fn imported_delegation_step() -> NextStep {
    NextStep::AdvanceImportedDelegation { bundle_index: 0 }
}

#[tokio::test]
async fn a_dispatch_decided_in_an_earlier_epoch_is_cancelled_not_adopted() {
    // The regression: the driver checks for an interruption, then plans, then
    // builds a host context and reads stored signing material before the step
    // begins. A host that switched epoch across that gap invalidated the run,
    // and a step that captured its own epoch on entry would adopt the new one
    // and prove, persist or broadcast for a session the host had left.
    let database = database_with_imported_delegation();
    let chain = Arc::new(ScriptedChain::default());
    chain.queue_confirmed();
    let executor = executor_over_chain(database, chain);

    let control = ChainSubmissionControl::new(1);
    let run_epoch = control.operation_epoch();
    // The host switches account or session while the driver is still deciding.
    control.set_operation_epoch(2);

    let outcome = executor
        .advance_step_in_epoch(
            imported_delegation_step(),
            &host(),
            &control,
            run_epoch,
            &NoopRoundStepProgressReporter {},
        )
        .await
        .expect("an interrupted step is not a failure");

    assert_eq!(
        outcome.disposition,
        RoundStepDisposition::Cancelled,
        "the step belongs to the epoch the run captured"
    );
    assert!(
        outcome.chain_outcome.is_none(),
        "nothing was dispatched to the chain"
    );
}

#[tokio::test]
async fn a_dispatch_in_the_run_s_own_epoch_still_runs() {
    let database = database_with_imported_delegation();
    let chain = Arc::new(ScriptedChain::default());
    chain.queue_confirmed();
    let executor = executor_over_chain(database, chain);

    let control = ChainSubmissionControl::new(1);
    let outcome = executor
        .advance_step_in_epoch(
            imported_delegation_step(),
            &host(),
            &control,
            control.operation_epoch(),
            &NoopRoundStepProgressReporter {},
        )
        .await
        .expect("the epoch matches, so the step runs");

    assert_ne!(outcome.disposition, RoundStepDisposition::Cancelled);
}
