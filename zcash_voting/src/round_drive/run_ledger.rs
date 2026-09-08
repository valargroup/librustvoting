//! What one run has accumulated, and how each dispatch folds into it.
//!
//! The ledger is the run's only mutable state. It owns nothing about *what* to
//! run: it records what happened, decides whether an outcome ends the run, and
//! turns itself into the report at the end.

use crate::{
    delegate::SignedDelegationBundle,
    session::{NextStep, RoundPlan},
    ChainSubmissionPending, ChainSubmissionResult, RoundStepDisposition, RoundStepFailure,
    RoundStepOutcome, VoteShareDeliveryReport,
};

use super::{
    progress::{RoundDriveEvent, RoundDriveReporter},
    quiescence::RoundQuiescence,
    selection,
    tally::{RoundWorkTally, VoteProgressBaseline},
    RoundRunReport, RoundStepFailureRecord,
};

/// What the run has accumulated so far.
#[derive(Default)]
pub(super) struct Run {
    pub(super) dispatches: usize,
    pub(super) plan: Option<RoundPlan>,
    pub(super) tally: RoundWorkTally,
    pub(super) baseline: Option<VoteProgressBaseline>,
    pub(super) failures: Vec<RoundStepFailureRecord>,
    pub(super) skipped: Vec<u32>,
    pub(super) chain_outcomes: Vec<(NextStep, ChainSubmissionResult)>,
    pub(super) share_deliveries: Vec<VoteShareDeliveryReport>,
    pub(super) delegations: Vec<SignedDelegationBundle>,
    /// Set when the last dispatch asked to be run again after a wait.
    pub(super) repoll: Vec<(NextStep, std::time::Duration)>,
    /// Steps whose completed re-poll wait wants them dispatched again.
    pub(super) awaiting_repoll: Vec<NextStep>,
}

impl Run {
    pub(super) fn finish(self, quiescence: RoundQuiescence) -> RoundRunReport {
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

    pub(super) fn record_failure(
        &mut self,
        step: Option<NextStep>,
        bundle_index: Option<u32>,
        failure: RoundStepFailure,
    ) {
        self.share_deliveries
            .extend(failure.share_deliveries.iter().cloned());
        // A `Delegate` step can prove and sign and then lose the chain. The
        // signed bundle is durable and the run produced it, so it belongs in
        // the aggregate exactly as a successful step's does.
        if let Some(delegation) = failure.delegation.as_ref() {
            self.delegations.push(delegation.clone());
        }
        // A step can confirm on the chain and then fail on the helper work
        // that follows. The confirmation is a durable effect the run observed,
        // so it belongs in the aggregate exactly as a successful step's does;
        // keeping it only inside the failure would leave `chain_outcomes` and
        // its wire projection describing less than the run actually saw.
        if let (Some(step), Some(chain_outcome)) = (step.as_ref(), failure.chain_outcome.as_ref()) {
            self.chain_outcomes
                .push((step.clone(), chain_outcome.clone()));
        }
        self.failures.push(RoundStepFailureRecord {
            step,
            bundle_index,
            failure,
        });
    }

    /// Folds one dispatch's outcome in, returning the reason to stop if it
    /// ends the run.
    pub(super) fn record_outcome(
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
                            delegation: None,
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

    pub(super) fn record_plan_failure(&mut self, error: crate::VotingError) {
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
                delegation: None,
            },
        );
    }
}
