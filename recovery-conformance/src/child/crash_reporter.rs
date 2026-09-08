//! The driver observer that dies at a named point in a round.
//!
//! Every crash point that is not the broadcast boundary is already announced
//! by the driver, and each announcement names the step it belongs to. That
//! matters here more than it does for a host UI: the driver interleaves
//! bundles, so a reporter that matched on a bare `ChainOutcome` or `TreeSynced`
//! would kill the process on whichever bundle happened to reach it first, and
//! the stage would land on a different bundle every run.
//!
//! The broadcast and share-POST stages are not here. Each sits inside one
//! transport call, which no event separates; they belong to
//! [`CrashTransport`](super::crash_transport::CrashTransport) and
//! [`CrashHelperTransport`](super::crash_helper_transport::CrashHelperTransport).

use std::sync::Arc;

use zcash_voting::{
    delegate::DelegationProgress,
    round_drive::{RoundDriveEvent, RoundDriveReporter},
    session::NextStep,
    vote::VoteCommitStage,
    RoundStepProgress,
};

use super::crash::{crash_now, CrashLog, Observation};
use crate::stages::{CrashStage, CrashTrigger};

/// Which bundle and proposal a crash stage is scoped to.
///
/// A multi-bundle round reaches every delegation stage once per bundle. Without
/// a scope the first bundle to arrive would always win, so `E1` — crash one
/// bundle, assert the others are untouched — could not be written at all.
#[derive(Clone, Copy, Debug)]
pub struct CrashTarget {
    pub bundle_index: u32,
    pub proposal_id: u32,
}

/// Kills the process when the driver reports the armed stage.
///
/// Also records the plan the driver last read, so the parent can compare what
/// the child believed it owed against what the reopened sidecar says.
pub struct CrashReporter {
    armed: Option<CrashStage>,
    target: CrashTarget,
    log: Arc<CrashLog>,
}

impl CrashReporter {
    /// Arms `stage` if it is event-triggered; broadcast stages belong to
    /// [`CrashTransport`](super::crash_transport::CrashTransport).
    pub fn new(stage: Option<CrashStage>, target: CrashTarget, log: Arc<CrashLog>) -> Self {
        let armed = stage.filter(|stage| stage.trigger() == CrashTrigger::Event);
        Self { armed, target, log }
    }

    fn crash(&self, stage: CrashStage) -> ! {
        crash_now(&self.log, stage)
    }

    /// Whether `step` belongs to the bundle (and proposal) this run targets.
    fn on_target(&self, step: &NextStep) -> bool {
        let bundle = match step {
            NextStep::Delegate { bundle_index }
            | NextStep::AdvanceDelegation { bundle_index }
            | NextStep::AdvanceImportedDelegation { bundle_index } => *bundle_index,
            NextStep::CastVote {
                bundle_index,
                proposal_id,
                ..
            }
            | NextStep::AdvanceVote {
                bundle_index,
                proposal_id,
            }
            | NextStep::AdvanceVoteBatch {
                bundle_index,
                proposal_id,
            }
            | NextStep::SubmitShares {
                bundle_index,
                proposal_id,
                ..
            }
            | NextStep::ConfirmShare {
                bundle_index,
                proposal_id,
                ..
            } => {
                if *proposal_id != self.target.proposal_id {
                    return false;
                }
                *bundle_index
            }
            _ => return false,
        };
        bundle == self.target.bundle_index
    }
}

impl RoundDriveReporter for CrashReporter {
    fn report(&self, event: RoundDriveEvent) {
        let Some(armed) = self.armed else {
            return;
        };

        match &event {
            RoundDriveEvent::PlanRefreshed { plan, .. } => {
                self.log.record(&Observation::PlanBeforeCrash {
                    next_steps: plan
                        .next_steps
                        .iter()
                        .map(|step| format!("{step:?}"))
                        .collect(),
                });
                // A committed vote that has reserved no POST is only visible
                // as a plan shape: the driver never announces "the vote is
                // written". `AdvanceVote` for the target proposal says exactly
                // that, and says it from the one plan the driver itself read.
                if armed == CrashStage::AfterVoteCommit
                    && plan.next_steps.iter().any(|step| {
                        matches!(
                            step,
                            NextStep::AdvanceVote { .. } | NextStep::AdvanceVoteBatch { .. }
                        ) && self.on_target(step)
                    })
                {
                    self.crash(armed);
                }
            }

            RoundDriveEvent::StepSelected { step } if self.on_target(step) => match (armed, step) {
                (CrashStage::BeforeDelegation, NextStep::Delegate { .. })
                | (CrashStage::BeforeCast, NextStep::CastVote { .. }) => self.crash(armed),
                _ => {}
            },

            RoundDriveEvent::StepProgress { step, progress } if self.on_target(step) => {
                self.crash_on_progress(armed, step, progress);
            }

            _ => {}
        }
    }
}

impl CrashReporter {
    /// The stages that map one-to-one onto a step's own progress events.
    fn crash_on_progress(&self, armed: CrashStage, step: &NextStep, progress: &RoundStepProgress) {
        let matched = match (armed, progress) {
            // `PcztBuilding`, not `SelectingNotes`. The latter is announced
            // *before* `prepare` runs, so crashing on it lands before any note
            // has been chosen -- the same durable state `before-delegation`
            // already covers, under a name claiming otherwise. `PcztBuilding`
            // is the first event after selection returns.
            (
                CrashStage::AfterNoteSelection,
                RoundStepProgress::Delegation {
                    progress: DelegationProgress::PcztBuilding,
                    ..
                },
            )
            | (
                CrashStage::AfterPczt,
                RoundStepProgress::Delegation {
                    progress: DelegationProgress::PcztBuilt,
                    ..
                },
            )
            | (
                CrashStage::AfterProof,
                RoundStepProgress::Delegation {
                    progress: DelegationProgress::ProofComplete,
                    ..
                },
            )
            | (
                CrashStage::AfterSigning,
                RoundStepProgress::Delegation {
                    progress: DelegationProgress::PayloadReady,
                    ..
                },
            ) => true,

            (
                CrashStage::AfterVoteCommit,
                RoundStepProgress::DelegateAndVoteBatchPersisted { .. },
            ) => true,
            (CrashStage::AfterTreeSync, RoundStepProgress::TreeSynced { .. }) => true,

            // A delegation and a vote both report a bare `ChainOutcome`, and
            // both can be on the target bundle, so matching the outcome alone
            // would let a vote's confirmation fire the delegation stage.
            (CrashStage::AfterTracking, RoundStepProgress::ChainOutcome(_)) => true,
            (CrashStage::AfterVoteConfirmed, RoundStepProgress::ChainOutcome(_)) => {
                !is_delegation(step)
            }

            // `Signing` is the last stage before anything about the vote is
            // written, so it is the point where the proof is finished and
            // still entirely in memory — the crash that costs minutes and must
            // cost nothing else.
            (
                CrashStage::AfterVoteProof,
                RoundStepProgress::VoteCommit(VoteCommitStage::Signing { .. }),
            ) => true,

            (CrashStage::AfterHelperPlans, RoundStepProgress::HelperPlansPrepared(_)) => true,
            (CrashStage::AfterShareAccepted, RoundStepProgress::ShareOutcome(_)) => true,
            _ => false,
        };

        if matched {
            self.crash(armed);
        }
    }
}

/// Whether `step` advances a delegation rather than a vote or a share.
fn is_delegation(step: &NextStep) -> bool {
    matches!(
        step,
        NextStep::Delegate { .. }
            | NextStep::AdvanceDelegation { .. }
            | NextStep::AdvanceImportedDelegation { .. }
    )
}
