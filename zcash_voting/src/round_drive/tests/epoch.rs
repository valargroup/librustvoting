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

/// A host that switches the epoch the first time a dispatch samples it.
struct EpochSwitchingHost {
    inner: StoredSigningHost,
    control: ChainSubmissionControl,
    switched: std::sync::atomic::AtomicBool,
}

impl crate::RoundHostSource for EpochSwitchingHost {
    fn host_context(&self) -> crate::RoundHostContext {
        let context = self.inner.host_context();
        if !self
            .switched
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.control.set_operation_epoch(2);
        }
        context
    }
}

#[tokio::test]
async fn an_epoch_switch_while_gathering_contexts_is_not_a_signature_handoff() {
    // Building each host context runs host code and the signature check reads
    // the database, so both can span a switch. `NeedsDelegationSignatures` is
    // the last pre-dispatch return, with no dispatch after it whose epoch
    // binding could correct the answer, so it must not describe a round the
    // host has abandoned.
    let database = database();
    let executor = executor_over(Arc::clone(&database));
    decide_ballot(&executor);
    let control = ChainSubmissionControl::new(1);

    let report = crate::RoundDriver::new(&executor)
        .run(
            &EpochSwitchingHost {
                inner: StoredSigningHost {
                    database: Arc::clone(&database),
                },
                control: control.clone(),
                switched: std::sync::atomic::AtomicBool::new(false),
            },
            &control,
            &RecordingReporter::default(),
        )
        .await;

    assert!(
        matches!(report.quiescence, crate::RoundQuiescence::Cancelled),
        "the run was abandoned before the handoff: {:?}",
        report.quiescence
    );
}

/// Cancels the run the first time a step reports that it finished.
struct CancellingOnStepFinished {
    control: ChainSubmissionControl,
}

impl crate::RoundDriveReporter for CancellingOnStepFinished {
    fn report(&self, event: crate::RoundDriveEvent) {
        if matches!(event, crate::RoundDriveEvent::StepFinished { .. }) {
            self.control.cancel();
        }
    }
}

#[tokio::test]
async fn a_run_cancelled_after_a_wave_still_reports_what_the_wave_did() {
    // The wave confirmed the delegation on chain, and the cancellation is only
    // observed at the next pass's first check. Returning there without
    // re-planning left the report describing the round as it was before the
    // confirmation: the bundle still non-terminal and its step still listed.
    let database = database_with_imported_delegation();
    let chain = Arc::new(ScriptedChain::default());
    chain.queue_confirmed();
    let executor = executor_over_chain(Arc::clone(&database), chain);
    let control = ChainSubmissionControl::new(1);

    let before = executor.plan().unwrap();
    assert!(
        !before.next_steps.is_empty(),
        "the round owes the imported delegation before the wave"
    );

    let report = crate::RoundDriver::new(&executor)
        .run(
            &SinglePassHost,
            &control,
            &CancellingOnStepFinished {
                control: control.clone(),
            },
        )
        .await;

    assert!(
        matches!(report.quiescence, crate::RoundQuiescence::Cancelled),
        "{:?}",
        report.quiescence
    );
    let after = executor.plan().unwrap();
    assert_ne!(
        before.next_steps, after.next_steps,
        "the wave must change durable state, or this test proves nothing"
    );
    assert_eq!(
        report.plan.as_ref().map(|plan| plan.next_steps.clone()),
        Some(after.next_steps),
        "the report describes the round the wave left"
    );
}
