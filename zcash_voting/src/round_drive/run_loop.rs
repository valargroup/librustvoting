//! The run loop: plan, admit a wave, dispatch it, fold the results, repeat.
//!
//! One pass of this loop is the driver's whole mechanism. Every decision it
//! makes is delegated: `selection` says what to admit, `signing` says whether
//! the host still owes a signature, `dispatch` runs the wave, `run_ledger`
//! records what happened and whether it ends the run, and `quiescence` says
//! why a plan with nothing dispatchable stops.

use crate::{round_planning::ClassifiedPlan, ChainSubmissionControl, ChainTransport, VotingError};

use super::{
    dispatch,
    policy::FailureIsolation,
    progress::{RoundDriveEvent, RoundDriveReporter},
    quiescence::{quiesce_before_dispatch, RoundQuiescence},
    run_ledger::Run,
    selection, signing,
    tally::BallotBaseline,
    RoundDriver, RoundHostSource, RoundRunReport,
};

impl<T: ChainTransport> RoundDriver<'_, T> {
    /// Plans the round without stalling a worker other steps are using.
    ///
    /// `plan_classified` is synchronous and holds the sidecar's connection
    /// mutex for its whole read transaction, so on a multi-threaded runtime it
    /// runs under `block_in_place`: bundles persisting concurrently must not
    /// queue behind it on the same worker. A current-thread runtime has no
    /// other worker to hand it to, and `block_in_place` panics there, so the
    /// read happens inline.
    fn plan_off_the_worker(&self) -> Result<ClassifiedPlan, VotingError> {
        let multi_threaded = matches!(
            tokio::runtime::Handle::try_current().map(|handle| handle.runtime_flavor()),
            Ok(tokio::runtime::RuntimeFlavor::MultiThread)
        );
        if multi_threaded {
            tokio::task::block_in_place(|| self.executor.plan_classified())
        } else {
            self.executor.plan_classified()
        }
    }

    /// Re-reads the plan and tally so a report describes the round after the
    /// wave's durable effects rather than before them.
    ///
    /// Best effort: a run stops for a reason the wave produced, and a failed
    /// re-read is not that reason. The pre-wave values stand if it fails.
    fn refresh_progress(&self, run: &mut Run) {
        let Ok(classified) = self.plan_off_the_worker() else {
            return;
        };
        if let Some(baseline) = run.baseline.as_ref() {
            run.tally = baseline.tally(&classified.obligations);
        }
        run.plan = Some(classified.plan);
    }

    /// Runs the bound round until it is quiescent. See [`RoundDriver::run`].
    pub(super) async fn drive(
        &self,
        host: &dyn RoundHostSource,
        control: &ChainSubmissionControl,
        events: &dyn RoundDriveReporter,
    ) -> RoundRunReport {
        let mut run = Run::default();
        let entry_epoch = control.operation_epoch();
        let interrupted = || control.is_cancelled() || control.operation_epoch() != entry_epoch;

        loop {
            if interrupted() {
                return run.finish(RoundQuiescence::Cancelled);
            }

            // The one read the driver selects from.
            let classified = match self.plan_off_the_worker() {
                Ok(classified) => classified,
                Err(error) => {
                    run.record_plan_failure(error);
                    return run.finish(RoundQuiescence::Failures);
                }
            };
            let baseline = run
                .baseline
                .get_or_insert_with(|| BallotBaseline::capture(&classified.obligations));
            run.tally = baseline.tally(&classified.obligations);
            run.plan = Some(classified.plan.clone());
            events.report(RoundDriveEvent::PlanRefreshed {
                plan: Box::new(classified.plan.clone()),
                tally: run.tally,
            });

            // The plan read blocks on the database and the callback above runs
            // host code, so either can span a cancellation or an epoch switch.
            // Every early return below reports a state of the round, and a run
            // the host has abandoned must not describe one: it would answer
            // `NoWorkLeft` or `NeedsBallot` for a session already left, and no
            // dispatch follows whose epoch binding could correct it.
            if interrupted() {
                return run.finish(RoundQuiescence::Cancelled);
            }

            if let Some(quiescence) =
                quiesce_before_dispatch(&classified.plan, &classified.obligations.obligations, &run)
            {
                return run.finish(quiescence);
            }
            if run.dispatches >= self.policy.max_dispatches {
                let remaining = classified.plan.next_steps.clone();
                return run.finish(RoundQuiescence::PassBudgetExhausted { remaining });
            }
            run.awaiting_repoll.retain(|step| {
                classified.plan.next_steps.contains(step)
                    && !run.skipped.contains(&selection::bundle_index(step))
            });
            let remaining_budget = self.policy.max_dispatches - run.dispatches;
            let steps = selection::next_dispatches(
                &classified.plan.next_steps,
                &run.skipped,
                &run.awaiting_repoll,
                self.policy.max_bundle_concurrency.get(),
                remaining_budget,
                self.policy.failure_isolation == FailureIsolation::SkipBundle,
            );
            if steps.is_empty() {
                // Every remaining step belongs to a bundle a failure skipped.
                return run.finish(RoundQuiescence::Failures);
            }
            run.awaiting_repoll.retain(|step| !steps.contains(step));
            let dispatches: Vec<_> = steps
                .into_iter()
                .map(|step| (step, host.host_context()))
                .collect();
            let signer_handoff = signing::missing_signer_bundles(
                self.executor,
                &dispatches,
                &classified.plan.round_id,
                &classified.obligations.obligations,
                &run.skipped,
            );
            // Building each host context ran host code and the signature check
            // read the database, so both can span an interruption too. The two
            // returns below are the last that describe the round without a
            // dispatch after them to correct the answer.
            if interrupted() {
                return run.finish(RoundQuiescence::Cancelled);
            }
            match signer_handoff {
                Ok(bundles) if !bundles.is_empty() => {
                    return run.finish(RoundQuiescence::NeedsDelegationSignatures { bundles });
                }
                Ok(_) => {}
                Err(error) => {
                    run.record_plan_failure(error);
                    return run.finish(RoundQuiescence::Failures);
                }
            }

            for (step, _) in &dispatches {
                events.report(RoundDriveEvent::StepSelected { step: step.clone() });
            }
            run.dispatches += dispatches.len();
            let dispatched =
                dispatch::run(self.executor, dispatches, control, entry_epoch, events).await;

            let mut wave_quiescence = None;
            for (step, dispatched) in dispatched {
                match dispatched {
                    Ok(outcome) => {
                        if let Some(quiescence) =
                            run.record_outcome(&step, outcome, self.policy.pending_repoll, events)
                        {
                            wave_quiescence.get_or_insert(quiescence);
                        }
                    }
                    Err(failure) => {
                        events.report(RoundDriveEvent::StepFailed {
                            step: step.clone(),
                            kind: failure.kind,
                            message: failure.message.clone(),
                        });
                        let bundle_index = selection::bundle_index(&step);
                        run.record_failure(Some(step.clone()), Some(bundle_index), failure);
                        match self.policy.failure_isolation {
                            FailureIsolation::StopRound => {
                                wave_quiescence.get_or_insert(RoundQuiescence::Failures);
                            }
                            FailureIsolation::SkipBundle => {
                                run.skipped.push(bundle_index);
                                events.report(RoundDriveEvent::BundleSkipped {
                                    bundle_index,
                                    after: step,
                                });
                            }
                        }
                    }
                }
            }
            if let Some(quiescence) = wave_quiescence {
                // The wave made durable progress before it stopped, so the
                // pre-dispatch plan and tally no longer describe the round: a
                // vote that confirmed and then failed on helper delivery would
                // still be listed as owing reconciliation, and its proposal
                // counted incomplete. Refresh both from durable state before
                // reporting, keeping the pre-wave read if the refresh fails —
                // a failed courtesy read must not replace the reason the run
                // stopped.
                self.refresh_progress(&mut run);
                return run.finish(quiescence);
            }

            if !run.repoll.is_empty() {
                let repolls = std::mem::take(&mut run.repoll);
                let delay = repolls
                    .iter()
                    .map(|(_, delay)| *delay)
                    .max()
                    .unwrap_or_default();
                for (repoll_step, step_delay) in &repolls {
                    events.report(RoundDriveEvent::AwaitingRepoll {
                        step: repoll_step.clone(),
                        delay: *step_delay,
                    });
                }
                if !sleep_until_interrupted(delay, control, entry_epoch).await {
                    self.refresh_progress(&mut run);
                    return run.finish(RoundQuiescence::Cancelled);
                }
                // The next pass re-plans, but it dispatches this step again if
                // the refreshed plan still lists it. Leaving that to plan order
                // would make the event above a promise the driver does not
                // keep, and would let a pending step that is not first be
                // starved by one that is.
                run.awaiting_repoll
                    .extend(repolls.into_iter().map(|(step, _)| step));
            }
        }
    }
}

/// Waits `delay`, returning `false` if the host interrupted meanwhile.
///
/// The wait is polled rather than slept through so a host that closes the
/// session does not pay the rest of it.
pub(crate) async fn sleep_until_interrupted(
    delay: std::time::Duration,
    control: &ChainSubmissionControl,
    entry_epoch: u64,
) -> bool {
    const CHECK: std::time::Duration = std::time::Duration::from_millis(50);
    let deadline = tokio::time::Instant::now() + delay;
    loop {
        if control.is_cancelled() || control.operation_epoch() != entry_epoch {
            return false;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return true;
        }
        tokio::time::sleep(CHECK.min(deadline - now)).await;
    }
}
