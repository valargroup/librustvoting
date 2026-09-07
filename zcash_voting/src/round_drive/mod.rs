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

mod dispatch;
mod policy;
mod progress;
mod quiescence;
pub(crate) mod selection;
pub(crate) mod tally;

#[cfg(test)]
mod tests;

pub use policy::{FailureIsolation, RoundDrivePolicy};
pub use progress::{
    NoopRoundDriveReporter, RoundDriveEvent, RoundDriveReporter, RoundDriveReporterBridge,
};
pub use quiescence::RoundQuiescence;
pub use tally::RoundWorkTally;

use crate::{
    delegate::SignedDelegationBundle,
    round_planning::{ClassifiedPlan, Obligation},
    session::{NextStep, RoundPlan},
    share_tracking::ShareKey,
    ChainSubmissionControl, ChainSubmissionPending, ChainSubmissionResult, ChainTransport,
    DelegationSigner, KeystoneSignatureSource, RoundExecutor, RoundHostContext,
    RoundStepDisposition, RoundStepFailure, RoundStepOutcome, VoteShareDeliveryReport,
};

use tally::BallotBaseline;

/// One failure the run kept, with the bundle it isolated.
///
/// Non-exhaustive: a run reports what it observed, and what there is to
/// observe grows. Hosts read these fields; they never build one.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RoundStepFailureRecord {
    pub step: Option<NextStep>,
    /// The bundle skipped for the rest of the run under
    /// [`FailureIsolation::SkipBundle`]. `None` when the failure was not
    /// attributable to one, such as a plan that could not be read.
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
    pub skipped_bundles: Vec<u32>,
    /// Terminal chain outcomes observed, in order.
    pub chain_outcomes: Vec<(NextStep, ChainSubmissionResult)>,
    pub share_deliveries: Vec<VoteShareDeliveryReport>,
    pub delegations: Vec<SignedDelegationBundle>,
}

/// Supplies the per-step host inputs.
///
/// Called once per dispatch. See the module documentation for why a run cannot
/// freeze one context for its whole duration.
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

    /// Plans the round without stalling a worker other steps are using.
    ///
    /// `plan_classified` is synchronous and holds the sidecar's connection
    /// mutex for its whole read transaction, so on a multi-threaded runtime it
    /// runs under `block_in_place`: bundles persisting concurrently must not
    /// queue behind it on the same worker. A current-thread runtime has no
    /// other worker to hand it to, and `block_in_place` panics there, so the
    /// read happens inline.
    fn plan_off_the_worker(&self) -> Result<ClassifiedPlan, crate::VotingError> {
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

    /// Runs the bound round until it is quiescent.
    ///
    /// Never returns `Err`: a run that could not plan at all reports
    /// [`RoundQuiescence::Failures`] carrying the planning failure, so a host
    /// has one shape to handle. Cancellation or an operation-epoch change ends
    /// the run at the next boundary as [`RoundQuiescence::Cancelled`] and is
    /// never retried — a detached prover may still hold the bundle lock for
    /// the epoch just left, so an immediate retry would queue behind work it
    /// has already abandoned.
    pub async fn run(
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

            if let Some(quiescence) = quiesce_before_dispatch(&classified.plan, &run) {
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
            match self.missing_signer_bundles(
                &dispatches,
                &classified.plan.round_id,
                &classified.obligations.obligations,
                &run.skipped,
            ) {
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

    /// Delegation bundles the host must supply a signature for before this
    /// run can dispatch anything.
    ///
    /// The answer covers **every** bundle the round still owes a delegation
    /// for, not just the ones this wave would run. A wave is bounded by the
    /// concurrency limit, so checking only its members would prove and
    /// broadcast the signed bundles first and report the unsigned ones one
    /// wave later — the host would collect signatures in several rounds, and
    /// work would already have happened before the first of them.
    fn missing_signer_bundles(
        &self,
        dispatches: &[(NextStep, RoundHostContext)],
        round_id: &str,
        obligations: &[Obligation],
        skipped: &[u32],
    ) -> Result<Vec<u32>, crate::VotingError> {
        let required = signer_bundles(obligations, skipped);
        if required.is_empty() {
            return Ok(Vec::new());
        }
        // A wave with no delegation work cannot be blocked by a signature:
        // plan order puts a bundle's delegation ahead of everything that
        // depends on it, so its vote and share work is not selected yet.
        let Some(context) = dispatches
            .iter()
            .find(|(step, _)| selection::needs_delegation_signer(step))
            .map(|(_, context)| context)
        else {
            return Ok(Vec::new());
        };
        let Some(inputs) = context.delegation.as_ref() else {
            return Ok(required);
        };
        if !matches!(
            inputs.signer,
            DelegationSigner::Keystone(KeystoneSignatureSource::Stored)
        ) {
            // Every other signer produces its signature during the step.
            return Ok(Vec::new());
        }

        let stored = self.executor.database().get_keystone_signatures(round_id)?;
        Ok(required
            .into_iter()
            .filter(|bundle_index| {
                !stored
                    .iter()
                    .any(|record| record.bundle_index == *bundle_index)
            })
            .collect())
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

/// The reason to stop before dispatching anything from this plan, if any.
///
/// Ordered so that anything the host must act on outranks a handoff that asks
/// nothing of it: a recorded failure, then a persisted submission, then the
/// two setup blockers, then a ballot the voter has not finished, and only then
/// the shares a timer will finish on its own.
fn quiesce_before_dispatch(plan: &RoundPlan, run: &Run) -> Option<RoundQuiescence> {
    // Foreground work remains: drive it. Anything below describes a plan this
    // run cannot advance itself.
    let Some(background_shares) = background_share_handoff(plan, &run.skipped) else {
        return None;
    };

    if !run.failures.is_empty() {
        // Something failed and nothing dispatchable is left. Reporting one of
        // the healthy handoffs below would read as "the round is fine" and
        // hide the failure.
        return Some(RoundQuiescence::Failures);
    }
    // Nothing dispatchable is left, so the only thing that can still be
    // holding the foreground open is durable submission state: a terminal
    // submission plans no retry, and a managed one that projects no step
    // cannot be advanced from here either. Both are the host's to handle.
    if plan.blocking_recovery {
        return Some(RoundQuiescence::PersistedChainTerminal);
    }
    if plan.needs_bundle_setup {
        return Some(RoundQuiescence::NeedsBundleSetup);
    }
    // A withheld cast plans nothing at all, not even its delegation
    // prerequisite, so an open ballot is the host's to resolve rather than a
    // finished round. It outranks the share handoff below because it is the
    // one of the two the voter can still act on; the report's plan carries
    // `has_unconfirmed_shares` for a host that shows both.
    if !plan.open_proposals.is_empty() || !plan.unrostered_intents.is_empty() {
        return Some(RoundQuiescence::NeedsBallot {
            open_proposals: plan.open_proposals.clone(),
            unrostered_intents: plan.unrostered_intents.clone(),
        });
    }
    if !background_shares.is_empty() {
        return Some(RoundQuiescence::BackgroundShareWorkOnly {
            shares: background_shares,
        });
    }
    Some(RoundQuiescence::NoWorkLeft)
}

/// The shares this plan leaves to background tracking, or `None` when it still
/// lists a step this run would dispatch.
///
/// A `ConfirmShare` for a share some helper already accepted is finished by
/// polling, which the host's background tracking timer owns; a foreground run
/// that polled it would hold the vote flow open for work that does not block
/// it. An empty plan yields an empty handoff, since it too has nothing the
/// foreground can dispatch.
///
/// Steps on a bundle a failure isolated are excluded, because selection will
/// never admit them: counting them as foreground work would keep the run
/// dispatching the healthy bundles' background shares instead of reporting the
/// failure.
///
/// `RoundPlan::blocking_recovery` is deliberately *not* the question asked
/// here. It is a property of the whole round, so it stays true for a persisted
/// terminal submission that plans no step at all, and for a step on a skipped
/// bundle — and in both cases a round whose only remaining steps are shares a
/// helper already accepted would read as ordinary work and be polled for the
/// entire dispatch budget.
fn background_share_handoff(plan: &RoundPlan, skipped: &[u32]) -> Option<Vec<ShareKey>> {
    // A share row no helper has reached is delivered, not polled, and the
    // planner reports exactly that class as blocking. It is round-wide, so a
    // skipped bundle's undelivered share still counts: the run cannot hand
    // the round to background tracking while one exists.
    if plan.blocking_share_work {
        return None;
    }
    plan.next_steps
        .iter()
        .filter(|step| !skipped.contains(&selection::bundle_index(step)))
        .map(|step| match step {
            NextStep::ConfirmShare {
                bundle_index,
                proposal_id,
                share_index,
            } => Some(ShareKey {
                bundle_index: *bundle_index,
                proposal_id: *proposal_id,
                share_index: *share_index,
            }),
            _ => None,
        })
        .collect()
}

/// The bundles whose delegation obligations still need signing material.
fn signer_bundles(obligations: &[Obligation], skipped: &[u32]) -> Vec<u32> {
    let mut bundles: Vec<u32> = obligations
        .iter()
        .filter_map(|obligation| match obligation {
            Obligation::Delegate { bundle_index } => Some(*bundle_index),
            Obligation::AdvanceDelegation {
                bundle_index,
                imported: false,
                ..
            } => Some(*bundle_index),
            _ => None,
        })
        .filter(|bundle_index| !skipped.contains(bundle_index))
        .collect();
    bundles.sort_unstable();
    bundles.dedup();
    bundles
}

/// What the run has accumulated so far.
#[derive(Default)]
struct Run {
    dispatches: usize,
    plan: Option<RoundPlan>,
    tally: RoundWorkTally,
    baseline: Option<BallotBaseline>,
    failures: Vec<RoundStepFailureRecord>,
    skipped: Vec<u32>,
    chain_outcomes: Vec<(NextStep, ChainSubmissionResult)>,
    share_deliveries: Vec<VoteShareDeliveryReport>,
    delegations: Vec<SignedDelegationBundle>,
    /// Set when the last dispatch asked to be run again after a wait.
    repoll: Vec<(NextStep, std::time::Duration)>,
    /// Steps whose completed re-poll wait wants them dispatched again.
    awaiting_repoll: Vec<NextStep>,
}

impl Run {
    fn finish(self, quiescence: RoundQuiescence) -> RoundRunReport {
        RoundRunReport {
            quiescence,
            plan: self.plan,
            tally: self.tally,
            failures: self.failures,
            skipped_bundles: self.skipped,
            chain_outcomes: self.chain_outcomes,
            share_deliveries: self.share_deliveries,
            delegations: self.delegations,
        }
    }

    fn record_failure(
        &mut self,
        step: Option<NextStep>,
        bundle_index: Option<u32>,
        failure: RoundStepFailure,
    ) {
        self.share_deliveries
            .extend(failure.share_deliveries.iter().cloned());
        self.failures.push(RoundStepFailureRecord {
            step,
            bundle_index,
            failure,
        });
    }

    /// Folds one dispatch's outcome in, returning the reason to stop if it
    /// ends the run.
    fn record_outcome(
        &mut self,
        step: &NextStep,
        outcome: RoundStepOutcome,
        pending_repoll: std::time::Duration,
        events: &dyn RoundDriveReporter,
    ) -> Option<RoundQuiescence> {
        events.report(RoundDriveEvent::StepFinished {
            step: step.clone(),
            disposition: outcome.disposition,
        });
        self.share_deliveries.extend(outcome.share_deliveries);
        if let Some(delegation) = outcome.delegation {
            self.delegations.push(delegation);
        }
        if let Some(chain_outcome) = outcome.chain_outcome.clone() {
            self.chain_outcomes.push((step.clone(), chain_outcome));
        }
        match outcome.disposition {
            // More independent work may remain; the next plan says what.
            RoundStepDisposition::Advanced | RoundStepDisposition::NoWork => None,
            RoundStepDisposition::Cancelled => Some(RoundQuiescence::Cancelled),
            RoundStepDisposition::ChainTerminal => match outcome.chain_outcome {
                Some(chain_outcome) => Some(RoundQuiescence::ChainTerminal {
                    step: step.clone(),
                    outcome: chain_outcome,
                }),
                // The disposition says a submission ended without a
                // confirmation, so the outcome carrying its diagnostic is the
                // one thing the host needs. Reporting the round finished
                // instead would lose a rejection entirely.
                None => {
                    self.record_failure(
                        Some(step.clone()),
                        Some(selection::bundle_index(step)),
                        RoundStepFailure {
                            kind: crate::RoundStepFailureKind::InvariantViolation,
                            step: Some(step.clone()),
                            strongest_chain_state: None,
                            chain_outcome: None,
                            message: format!(
                                "{step:?} ended as a terminal chain result with no chain outcome"
                            ),
                            plan: None,
                            share_deliveries: Vec::new(),
                        },
                    );
                    Some(RoundQuiescence::Failures)
                }
            },
            RoundStepDisposition::Pending => match outcome.chain_outcome {
                // Still tracking, a share confirmation with no chain outcome,
                // or a confirmed vote whose helper delivery is waiting on
                // ambiguous attempts: waiting and a fresh plan are what make
                // progress.
                None
                | Some(ChainSubmissionResult::Pending(ChainSubmissionPending::Tracking {
                    ..
                }))
                | Some(ChainSubmissionResult::Confirmed(_)) => {
                    self.repoll.push((step.clone(), pending_repoll));
                    None
                }
                // The episode already escalated to exact-tree recovery and
                // still could not resolve. Re-polling it for the rest of the
                // round would hide a stuck submission the host can retry
                // later.
                Some(chain_outcome) => Some(RoundQuiescence::ChainRecoveryStalled {
                    step: step.clone(),
                    outcome: chain_outcome,
                }),
            },
        }
    }

    fn record_plan_failure(&mut self, error: crate::VotingError) {
        self.record_failure(
            None,
            None,
            RoundStepFailure {
                kind: crate::vote_work::step_outcomes::failure_kind_for(&error),
                step: None,
                strongest_chain_state: None,
                chain_outcome: None,
                message: error.to_string(),
                plan: None,
                share_deliveries: Vec::new(),
            },
        );
    }
}
