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

/// A reporter that switches the host's epoch when the plan is first reported,
/// standing in for host code that runs on that callback.
struct EpochSwitchingReporter {
    control: ChainSubmissionControl,
    switched: std::sync::atomic::AtomicBool,
}

impl crate::RoundDriveReporter for EpochSwitchingReporter {
    fn report(&self, event: crate::RoundDriveEvent) {
        if matches!(event, crate::RoundDriveEvent::PlanRefreshed { .. })
            && !self
                .switched
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.control.set_operation_epoch(2);
        }
    }
}

#[tokio::test]
async fn an_epoch_switch_during_planning_is_not_reported_as_a_round_state() {
    // The plan read blocks on the database and the plan callback runs host
    // code, so either can span the switch. Every pre-dispatch early return
    // describes a state of the round; an abandoned run must not describe one.
    // This round has an undecided ballot, so without the recheck the run would
    // answer `NeedsBallot` for a session the host had already left.
    let executor = executor();
    let control = ChainSubmissionControl::new(1);
    let events = EpochSwitchingReporter {
        control: control.clone(),
        switched: std::sync::atomic::AtomicBool::new(false),
    };

    let report = crate::RoundDriver::new(&executor)
        .run(&FixedHost, &control, &events)
        .await;

    assert!(
        matches!(report.quiescence, crate::RoundQuiescence::Cancelled),
        "an abandoned run reports only that it was abandoned: {:?}",
        report.quiescence
    );
}
