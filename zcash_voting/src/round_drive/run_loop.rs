//! Rolling admission, fresh planning, and draining of bundle obligations.

use futures_util::{stream::FuturesUnordered, StreamExt};
use std::collections::BTreeSet;

use crate::{
    round_planning::ClassifiedPlan, session::NextStep, ChainSubmissionControl, ChainTransport,
    VotingError,
};

use super::{
    dispatch,
    policy::{FailureIsolation, ProgressBaseline},
    progress::{RoundDriveEvent, RoundDriveReporter},
    quiescence::{quiesce_before_dispatch, requires_background_tracking, RoundQuiescence},
    run_ledger::Run,
    selection, signing,
    tally::VoteProgressBaseline,
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
    /// completed operations' durable effects rather than before them.
    ///
    /// Best effort: a run stops for a reason an operation produced, and a failed
    /// re-read is not that reason. The last known values stand if it fails.
    fn refresh_progress(&self, run: &mut Run) {
        let Ok(classified) = self.plan_off_the_worker() else {
            return;
        };
        if let Some(baseline) = run.baseline.as_ref() {
            run.tally = baseline.tally(&classified.obligations);
        }
        run.plan = Some(classified.plan);
    }

    /// Refills available bundle slots after each completion from a fresh plan.
    pub(super) async fn drive(
        &self,
        host: &dyn RoundHostSource,
        control: &ChainSubmissionControl,
        events: &dyn RoundDriveReporter,
    ) -> RoundRunReport {
        let mut run = Run::default();
        let entry_epoch = control.operation_epoch();
        let interrupted = || control.is_cancelled() || control.operation_epoch() != entry_epoch;
        let limit = if self.policy.failure_isolation == FailureIsolation::StopRound {
            1
        } else {
            self.policy.max_bundle_concurrency.get()
        };
        let mut flights = FuturesUnordered::new();
        let mut active = BTreeSet::new();
        let mut admitted = BTreeSet::new();
        let mut deadlines: Vec<(NextStep, Option<tokio::time::Instant>)> = Vec::new();
        let mut stopping: Option<(usize, RoundQuiescence)> = None;

        loop {
            if interrupted() {
                stopping = Some((0, RoundQuiescence::Cancelled));
            }
            if stopping.is_none() {
                match self.plan_off_the_worker() {
                    Err(error) => {
                        run.record_plan_failure(error);
                        stopping = Some((run.dispatches, RoundQuiescence::Failures));
                    }
                    Ok(classified) => {
                        let baseline = run.baseline.get_or_insert_with(|| {
                            match self.policy.progress_baseline {
                                ProgressBaseline::Run => {
                                    VoteProgressBaseline::for_run(&classified.obligations)
                                }
                                ProgressBaseline::SelectedChoices => {
                                    VoteProgressBaseline::for_selected_choices(
                                        &classified.obligations,
                                    )
                                }
                            }
                        });
                        run.tally = baseline.tally(&classified.obligations);
                        run.plan = Some(classified.plan.clone());
                        events.report(RoundDriveEvent::PlanRefreshed {
                            plan: Box::new(classified.plan.clone()),
                            tally: run.tally,
                        });
                        if interrupted() {
                            stopping = Some((0, RoundQuiescence::Cancelled));
                        } else if flights.is_empty() {
                            if let Some(reason) = quiesce_before_dispatch(
                                &classified.plan,
                                &classified.obligations.obligations,
                                &run,
                            ) {
                                return run.finish(reason);
                            }
                            if run.dispatches >= self.policy.max_dispatches {
                                return run.finish(RoundQuiescence::PassBudgetExhausted {
                                    remaining: classified.plan.next_steps,
                                });
                            }
                        }
                        let dispatchable: Vec<_> = classified
                            .plan
                            .next_steps
                            .iter()
                            .filter(|step| {
                                !requires_background_tracking(
                                    step,
                                    &classified.obligations.obligations,
                                )
                            })
                            .cloned()
                            .collect();
                        deadlines.retain(|(step, _)| {
                            dispatchable.contains(step)
                                && !run.skipped.contains(&selection::bundle_index(step))
                        });
                        let now = tokio::time::Instant::now();
                        let candidates: Vec<_> = dispatchable
                            .iter()
                            .filter(|step| {
                                !active.contains(&selection::bundle_index(step))
                                    && !deadlines.iter().any(|(waiting, deadline)| {
                                        waiting == *step
                                            && deadline.is_none_or(|deadline| deadline > now)
                                    })
                            })
                            .cloned()
                            .collect();
                        let preferred: Vec<_> = candidates
                            .iter()
                            .filter(|step| deadlines.iter().any(|(waiting, _)| waiting == *step))
                            .chain(
                                candidates.iter().filter(|step| {
                                    admitted.contains(&selection::bundle_index(step))
                                }),
                            )
                            .cloned()
                            .collect();
                        if stopping.is_none()
                            && flights.len() < limit
                            && run.dispatches < self.policy.max_dispatches
                        {
                            let steps = selection::next_dispatches(
                                &candidates,
                                &run.skipped,
                                &preferred,
                                limit - flights.len(),
                                self.policy.max_dispatches - run.dispatches,
                                true,
                            );
                            let dispatches: Vec<_> = steps
                                .into_iter()
                                .map(|step| (step, host.host_context()))
                                .collect();
                            let handoff = signing::missing_signer_bundles(
                                self.executor,
                                &dispatches,
                                &classified.plan.round_id,
                                &classified.obligations.obligations,
                                &run.skipped,
                            );
                            if interrupted() {
                                stopping = Some((0, RoundQuiescence::Cancelled));
                            } else {
                                match handoff {
                                    Ok(bundles) if !bundles.is_empty() => {
                                        stopping = Some((
                                            run.dispatches,
                                            RoundQuiescence::NeedsDelegationSignatures { bundles },
                                        ))
                                    }
                                    Err(error) => {
                                        run.record_plan_failure(error);
                                        stopping =
                                            Some((run.dispatches, RoundQuiescence::Failures));
                                    }
                                    Ok(_) => {
                                        for (step, context) in dispatches {
                                            events.report(RoundDriveEvent::StepSelected {
                                                step: step.clone(),
                                            });
                                            if interrupted() {
                                                stopping = Some((0, RoundQuiescence::Cancelled));
                                                break;
                                            }
                                            let sequence = run.dispatches;
                                            run.dispatches += 1;
                                            active.insert(selection::bundle_index(&step));
                                            admitted.insert(selection::bundle_index(&step));
                                            deadlines.retain(|(waiting, _)| waiting != &step);
                                            flights.push(dispatch::run(
                                                self.executor,
                                                sequence,
                                                step,
                                                context,
                                                control,
                                                entry_epoch,
                                                events,
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if stopping.is_some() && flights.is_empty() {
                if run.dispatches > 0 {
                    self.refresh_progress(&mut run);
                }
                let reason = if interrupted() {
                    RoundQuiescence::Cancelled
                } else {
                    stopping.take().unwrap().1
                };
                return run.finish(reason);
            }
            let completion = if stopping.is_some() {
                flights.next().await
            } else {
                let delay = repoll_delay(
                    &deadlines,
                    &active,
                    flights.len() < limit && run.dispatches < self.policy.max_dispatches,
                );
                tokio::select! {
                    completion = flights.next(), if !flights.is_empty() => completion,
                    _ = sleep_until_interrupted(delay, control, entry_epoch) => continue,
                }
            };
            let Some((sequence, step, dispatched)) = completion else {
                continue;
            };
            active.remove(&selection::bundle_index(&step));
            let before = run.effect_lengths();
            let terminal = match dispatched {
                Ok(outcome) => {
                    run.record_outcome(&step, outcome, self.policy.pending_repoll, events)
                }
                Err(failure) => {
                    let kind = failure.kind;
                    let message = failure.message.clone();
                    let bundle_index = selection::bundle_index(&step);
                    run.record_failure(Some(step.clone()), Some(bundle_index), failure);
                    events.report(RoundDriveEvent::StepFailed {
                        step: step.clone(),
                        kind,
                        message,
                    });
                    match self.policy.failure_isolation {
                        FailureIsolation::StopRound => Some(RoundQuiescence::Failures),
                        FailureIsolation::SkipBundle => {
                            run.skipped.push(bundle_index);
                            events.report(RoundDriveEvent::BundleSkipped {
                                bundle_index,
                                after: step.clone(),
                            });
                            None
                        }
                    }
                }
            };
            run.record_order(sequence, before);
            for (step, delay) in std::mem::take(&mut run.repoll) {
                deadlines.push((step.clone(), tokio::time::Instant::now().checked_add(delay)));
                if run.dispatches < self.policy.max_dispatches {
                    events.report(RoundDriveEvent::AwaitingRepoll { step, delay });
                }
            }
            if let Some(reason) = terminal {
                if stopping
                    .as_ref()
                    .is_none_or(|(earlier, _)| sequence < *earlier)
                {
                    stopping = Some((sequence, reason));
                }
            }
            // Draining still owes an authoritative snapshot after each completed
            // operation, even though no subsequent admission can use it.
            if stopping.is_some() || interrupted() {
                self.refresh_progress(&mut run);
            }
        }
    }
}

/// Wakes for the earliest re-poll that can use an admission slot. When capacity
/// or dispatch budget is exhausted, only completion or interruption can help.
/// Expiry after selection remains a zero delay so overdue work cannot lose its wake-up.
pub(super) fn repoll_delay(
    deadlines: &[(NextStep, Option<tokio::time::Instant>)],
    active: &BTreeSet<u32>,
    admission_available: bool,
) -> std::time::Duration {
    if !admission_available {
        return std::time::Duration::MAX;
    }
    deadlines
        .iter()
        .filter(|(step, _)| !active.contains(&selection::bundle_index(step)))
        .filter_map(|(_, deadline)| *deadline)
        .min()
        .map(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now()))
        .unwrap_or(std::time::Duration::MAX)
}

/// Waits `delay`, returning `false` if the host interrupted meanwhile.
///
/// The wait is woken by the control rather than slept through, so a host that
/// closes the session does not pay the rest of it. A share tracking pass can
/// ask for a delay measured in hours, so the wait races the delay against
/// [`ChainSubmissionControl::interrupted`] instead of re-reading the control on
/// a tick: an uninterrupted wait costs one timer however long it lasts.
///
/// `pending_repoll` is host-configured and unbounded, so an absolute deadline
/// that far out may not be representable. `None` means "wait until
/// interrupted" rather than panicking on the overflow — a policy value must
/// not be able to bring down the host process. This mirrors what the chain
/// client's own re-poll wait does with the same input.
pub(crate) async fn sleep_until_interrupted(
    delay: std::time::Duration,
    control: &ChainSubmissionControl,
    entry_epoch: u64,
) -> bool {
    let deadline = tokio::time::Instant::now().checked_add(delay);
    loop {
        let interrupt = control.interrupted();
        tokio::pin!(interrupt);
        // Registered before the flags are read, because `notify_waiters`
        // stores no permit: a change landing between the read and the
        // registration would wake nothing and leave the wait asleep for the
        // whole delay.
        interrupt.as_mut().enable();
        if control.is_cancelled() || control.operation_epoch() != entry_epoch {
            return false;
        }
        match deadline {
            Some(deadline) => {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => return true,
                    // A wake is not itself the answer: `set_operation_epoch`
                    // notifies whatever it stores, so re-storing the entry
                    // epoch must not read as an interruption. The loop
                    // re-reads the flags instead.
                    _ = &mut interrupt => continue,
                }
            }
            None => {
                interrupt.await;
                continue;
            }
        }
    }
}
