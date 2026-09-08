//! From obligations to the host-facing [`RoundPlan`]: the ordered step list,
//! the grouped recovery work, and every derived flag. Nothing here decides
//! what work exists; it only presents what the classifier found.

use std::collections::{BTreeMap, BTreeSet};

use crate::phases::{DelegationPhase, VotePhase};
use crate::session::{
    CompletedVoteChoice, CompletedVoteDisplay, Decision, DelegationRecoveryWork,
    DelegationRecoveryWorkKind, DelegationStatus, NextStep, RoundPlan, RoundPlanAction,
    VoteRecoveryWork, VoteRecoveryWorkKind,
};
use crate::share_policy::round_immediate_share_key;
use crate::types::VotingError;

use super::classify::{Obligation, RoundObligations};
use super::lifecycle::is_terminal_delegation_phase;
use super::snapshot::RoundSnapshot;
use super::vote_units::VoteUnitId;

/// Projects `obligations` over `snapshot` into the plan a host consumes.
pub(crate) fn project(
    snapshot: &RoundSnapshot,
    proposal_ids: &[u32],
    obligations: &RoundObligations,
) -> Result<RoundPlan, VotingError> {
    let round_id = snapshot.round_id.as_str();
    let intents = &snapshot.intents;
    let delegation: BTreeMap<u32, DelegationPhase> = snapshot
        .delegations
        .iter()
        .map(|status| (status.bundle_index, status.phase))
        .collect();
    let bundles: Vec<u32> = delegation.keys().copied().collect();
    let votes: BTreeMap<(u32, u32), VotePhase> = snapshot
        .votes
        .iter()
        .map(|(&key, vote)| (key, vote.phase))
        .collect();
    let vote_choices: BTreeMap<(u32, u32), u32> = snapshot
        .votes
        .iter()
        .map(|(&key, vote)| (key, vote.choice))
        .collect();
    let stale_vote_keys = &obligations.stale_vote_keys;

    let mut steps = steps_from_obligations(&obligations.obligations);
    steps.sort_by_key(step_rank);

    let blocking_confirm_share_keys: BTreeSet<(u32, u32, u32)> = obligations
        .obligations
        .iter()
        .filter_map(|obligation| match obligation {
            Obligation::Confirm {
                bundle_index,
                proposal_id,
                share_index,
                accepted: false,
                outcome_unknown: false,
                ..
            } => Some((*bundle_index, *proposal_id, *share_index)),
            _ => None,
        })
        .collect();
    let blocking_share_work = !blocking_confirm_share_keys.is_empty();
    let submission_managed = delegation
        .values()
        .any(|phase| *phase == DelegationPhase::SubmissionManaged)
        || votes
            .values()
            .any(|phase| *phase == VotePhase::SubmissionManaged);
    // Terminal hashless dispatch keeps the foreground closed but schedules
    // no recovery step, so it contributes to `blocking_recovery` only.
    let submitted_without_hash = delegation
        .values()
        .any(|phase| *phase == DelegationPhase::SubmittedWithoutHash)
        || votes
            .values()
            .any(|phase| *phase == VotePhase::SubmittedWithoutHash);
    let submission_rejected = delegation
        .values()
        .any(|phase| *phase == DelegationPhase::SubmissionRejected)
        || votes
            .values()
            .any(|phase| *phase == VotePhase::SubmissionRejected);
    let blocking_recovery = submission_managed
        || submitted_without_hash
        || submission_rejected
        || steps.iter().any(|step| match step {
            NextStep::ConfirmShare {
                bundle_index,
                proposal_id,
                share_index,
            } => blocking_confirm_share_keys.contains(&(*bundle_index, *proposal_id, *share_index)),
            _ => true,
        });

    let delegation_statuses = snapshot
        .delegations
        .iter()
        .map(|status| DelegationStatus {
            bundle_index: status.bundle_index,
            phase: status.phase,
            tx_hash: snapshot.delegation_tx_hash(status.bundle_index),
            submission_diagnostic: status.diagnostic.clone(),
            terminal: is_terminal_delegation_phase(status.phase),
        })
        .collect();
    let hotkey_bound = delegation
        .values()
        .any(|phase| *phase != DelegationPhase::Prepared)
        || !votes.is_empty()
        || !snapshot.share_phases.is_empty();
    let all_decided = proposal_ids.iter().all(|&pid| match intents.get(&pid) {
        Some(Decision::Skipped) => true,
        Some(Decision::Choice(choice)) => {
            !bundles.is_empty()
                && bundles.iter().all(|&b| {
                    let vote_key = (b, pid);
                    vote_choices.get(&vote_key) == Some(choice)
                        && matches!(votes.get(&vote_key), Some(VotePhase::Confirmed))
                })
        }
        None => false,
    });
    let completed_vote_artifact =
        vote_choices
            .iter()
            .any(|(&(bundle_index, proposal_id), &stored_choice)| {
                !stale_vote_keys.contains(&(bundle_index, proposal_id))
                    && matches!(
                        intents.get(&proposal_id),
                        Some(Decision::Choice(intent_choice)) if *intent_choice == stored_choice
                    )
            })
            || snapshot
                .share_phases
                .iter()
                .any(|(bundle_index, proposal_id, _, _)| {
                    !stale_vote_keys.contains(&(*bundle_index, *proposal_id))
                        && matches!(intents.get(proposal_id), Some(Decision::Choice(_)))
                });
    let completed_for_display = completed_vote_artifact && !blocking_recovery;
    let voted_at = snapshot
        .shares
        .iter()
        .map(|share| share.created_at)
        .filter(|created_at| *created_at > 0)
        .max();
    let completed_vote_display = completed_for_display.then(|| {
        completed_vote_display(
            proposal_ids,
            intents,
            &vote_choices,
            stale_vote_keys,
            voted_at,
        )
    });
    // `SubmittedWithoutHash` and `SubmissionRejected` are terminal: they block
    // the foreground above but are not pending recovery work.
    let pending_recovery = submission_managed || !steps.is_empty();
    let needs_draft_setup =
        !blocking_recovery && !all_decided && !obligations.open_proposals.is_empty();
    // Bundle setup is the only thing that can unblock the round, and it is
    // delegation preparation, so point the host at the delegation area
    // rather than reporting Idle for a round that plainly owes work.
    let primary_action = if obligations.needs_bundle_setup {
        RoundPlanAction::Delegate
    } else {
        select_primary_action(
            &steps,
            blocking_recovery,
            blocking_share_work,
            completed_for_display,
        )
    };
    let recovered_delegation_work = recovered_delegation_work_from_steps(
        snapshot,
        &delegation,
        &obligations.obligations,
        &steps,
    )?;
    let recovered_vote_work = recovered_vote_work_from_steps(
        snapshot,
        &blocking_confirm_share_keys,
        &obligations.obligations,
        &steps,
    )?;

    let mut work_summary = summarize_plan_work(&steps, blocking_share_work);
    for step in &steps {
        if let NextStep::CastVote { bundle_index, .. } = step {
            // A fresh cast on an unconfirmed delegation signs the delegation
            // as part of its combined transaction. An imported delegation is
            // already on the chain and its holder has no delegation key.
            let imported = snapshot
                .bundles
                .get(bundle_index)
                .is_some_and(|bundle| bundle.capability_imported);
            if !imported
                && delegation
                    .get(bundle_index)
                    .is_some_and(|phase| *phase != DelegationPhase::Confirmed)
            {
                work_summary
                    .delegation_bundles_needing_signing
                    .push(*bundle_index);
                work_summary
                    .delegation_bundles_needing_work
                    .push(*bundle_index);
            }
        }
    }
    work_summary
        .delegation_bundles_needing_signing
        .sort_unstable();
    work_summary.delegation_bundles_needing_signing.dedup();
    work_summary.delegation_bundles_needing_work.sort_unstable();
    work_summary.delegation_bundles_needing_work.dedup();
    work_summary.needs_delegation_signing =
        !work_summary.delegation_bundles_needing_signing.is_empty();

    let has_unconfirmed_shares = snapshot.shares.iter().any(|share| !share.confirmed);

    // Once a persisted plan carries the designation it is what will be
    // executed, whatever the current roster derives; report that marker, and
    // derive from the current choices only while no plan exists.
    let immediate_share_key = snapshot.persisted_immediate_share.or_else(|| {
        round_immediate_share_key(bundles.iter().copied().max(), &obligations.choice_proposals)
    });
    let immediate_share_confirmed = immediate_share_key.as_ref().is_some_and(|key| {
        snapshot.shares.iter().any(|share| {
            share.bundle_index == key.bundle_index
                && share.proposal_id == key.proposal_id
                && share.share_index == key.share_index
                && share.confirmed
        })
    });

    Ok(RoundPlan {
        round_id: round_id.to_string(),
        pending_recovery,
        next_steps: steps,
        open_proposals: obligations.open_proposals.clone(),
        unrostered_intents: obligations.unrostered_intents.clone(),
        immediate_share_key,
        immediate_share_confirmed,
        all_decided,
        delegation_statuses,
        blocking_recovery,
        blocking_share_work,
        has_unconfirmed_shares,
        hotkey_bound,
        completed_vote_artifact,
        completed_for_display,
        completed_vote_display,
        needs_draft_setup,
        needs_bundle_setup: obligations.needs_bundle_setup,
        primary_action,
        needs_delegation_signing: work_summary.needs_delegation_signing,
        has_in_flight_delegation: work_summary.has_in_flight_delegation,
        delegation_bundles_needing_work: work_summary.delegation_bundles_needing_work,
        delegation_bundles_needing_signing: work_summary.delegation_bundles_needing_signing,
        needs_vote_polling: work_summary.needs_vote_polling,
        has_remaining_vote_or_share_work: work_summary.has_remaining_vote_or_share_work,
        has_recoverable_vote_or_share_work: work_summary.has_recoverable_vote_or_share_work,
        recovered_delegation_work,
        recovered_vote_work,
    })
}

/// One `NextStep` per executable obligation. Retired and blocked work is
/// reported through the plan's flags, not as steps.
fn steps_from_obligations(obligations: &[Obligation]) -> Vec<NextStep> {
    let mut steps = Vec::new();
    for obligation in obligations {
        match obligation {
            Obligation::Delegate { bundle_index } => steps.push(NextStep::Delegate {
                bundle_index: *bundle_index,
            }),
            Obligation::AdvanceDelegation {
                bundle_index,
                imported: true,
                ..
            } => steps.push(NextStep::AdvanceImportedDelegation {
                bundle_index: *bundle_index,
            }),
            Obligation::AdvanceDelegation {
                bundle_index,
                imported: false,
                ..
            } => steps.push(NextStep::AdvanceDelegation {
                bundle_index: *bundle_index,
            }),
            Obligation::Cast {
                bundle_index,
                drafts,
                ..
            } => steps.extend(drafts.iter().map(|draft| NextStep::CastVote {
                bundle_index: *bundle_index,
                proposal_id: draft.proposal_id,
                choice: draft.choice,
            })),
            Obligation::ReconcileChain {
                unit,
                bundle_index,
                ordered_proposal_ids,
                ..
            } => steps.push(match unit {
                VoteUnitId::Singleton { proposal_id, .. } => NextStep::AdvanceVote {
                    bundle_index: *bundle_index,
                    proposal_id: *proposal_id,
                },
                VoteUnitId::Batch { .. } => NextStep::AdvanceVoteBatch {
                    bundle_index: *bundle_index,
                    proposal_id: ordered_proposal_ids[0],
                },
            }),
            Obligation::Deliver {
                bundle_index,
                proposal_id,
                share_indexes,
                ..
            } => steps.extend(
                share_indexes
                    .iter()
                    .map(|share_index| NextStep::SubmitShares {
                        bundle_index: *bundle_index,
                        proposal_id: *proposal_id,
                        share_index: *share_index,
                    }),
            ),
            Obligation::Confirm {
                bundle_index,
                proposal_id,
                share_index,
                ..
            } => steps.push(NextStep::ConfirmShare {
                bundle_index: *bundle_index,
                proposal_id: *proposal_id,
                share_index: *share_index,
            }),
            Obligation::Retire { .. } | Obligation::Blocked { .. } => {}
        }
    }
    steps
}

/// The obligation a host-selected `step` executes, in the plan `obligations`
/// were classified for; `None` when the plan no longer lists that work.
pub(crate) fn resolve_step<'a>(
    obligations: &'a [Obligation],
    step: &NextStep,
) -> Option<&'a Obligation> {
    obligations
        .iter()
        .find(|obligation| match (obligation, step) {
            (Obligation::Delegate { bundle_index }, NextStep::Delegate { bundle_index: b }) => {
                bundle_index == b
            }
            (
                Obligation::AdvanceDelegation {
                    bundle_index,
                    imported: false,
                    ..
                },
                NextStep::AdvanceDelegation { bundle_index: b },
            )
            | (
                Obligation::AdvanceDelegation {
                    bundle_index,
                    imported: true,
                    ..
                },
                NextStep::AdvanceImportedDelegation { bundle_index: b },
            ) => bundle_index == b,
            (
                Obligation::Cast {
                    bundle_index,
                    drafts,
                    ..
                },
                NextStep::CastVote {
                    bundle_index: b,
                    proposal_id,
                    choice,
                },
            ) => {
                bundle_index == b
                    && drafts
                        .iter()
                        .any(|draft| draft.proposal_id == *proposal_id && draft.choice == *choice)
            }
            (
                Obligation::ReconcileChain {
                    unit: VoteUnitId::Singleton { .. },
                    bundle_index,
                    ordered_proposal_ids,
                    ..
                },
                NextStep::AdvanceVote {
                    bundle_index: b,
                    proposal_id,
                },
            )
            | (
                Obligation::ReconcileChain {
                    unit: VoteUnitId::Batch { .. },
                    bundle_index,
                    ordered_proposal_ids,
                    ..
                },
                NextStep::AdvanceVoteBatch {
                    bundle_index: b,
                    proposal_id,
                },
            ) => bundle_index == b && ordered_proposal_ids[0] == *proposal_id,
            (
                Obligation::Deliver {
                    bundle_index,
                    proposal_id,
                    share_indexes,
                    ..
                },
                NextStep::SubmitShares {
                    bundle_index: b,
                    proposal_id: p,
                    share_index,
                },
            ) => bundle_index == b && proposal_id == p && share_indexes.contains(share_index),
            (
                Obligation::Confirm {
                    bundle_index,
                    proposal_id,
                    share_index,
                    ..
                },
                NextStep::ConfirmShare {
                    bundle_index: b,
                    proposal_id: p,
                    share_index: s,
                },
            ) => bundle_index == b && proposal_id == p && share_index == s,
            _ => false,
        })
}

pub(crate) fn step_rank(step: &NextStep) -> (u32, u32, u32, u32) {
    // Delegation is a prerequisite for fresh vote work, so keep it before
    // vote/share recovery. Vote work is proposal-primary so an interrupted
    // question finishes across all bundles before later questions resume.
    match step {
        NextStep::Delegate { bundle_index } => (0, 0, *bundle_index, 0),
        NextStep::AdvanceDelegation { bundle_index }
        | NextStep::AdvanceImportedDelegation { bundle_index } => (0, 0, *bundle_index, 0),
        NextStep::CastVote {
            bundle_index,
            proposal_id,
            choice: _,
        } => (1, *proposal_id, *bundle_index, 0),
        NextStep::AdvanceVote {
            bundle_index,
            proposal_id,
        }
        | NextStep::AdvanceVoteBatch {
            bundle_index,
            proposal_id,
        } => (1, *proposal_id, *bundle_index, 0),
        NextStep::SubmitShares {
            bundle_index,
            proposal_id,
            share_index,
        } => (1, *proposal_id, *bundle_index, *share_index),
        NextStep::ConfirmShare {
            bundle_index,
            proposal_id,
            share_index,
        } => (2, *proposal_id, *bundle_index, *share_index),
    }
}

/// The earlier step in `steps` that must clear before `step` can run.
///
/// Rank ordering is proposal-primary and says nothing about per-bundle
/// dependencies, so this is the check a host or executor needs before
/// running a step out of order: a bundle's outstanding delegation step
/// blocks that bundle's vote and share steps. Delegation steps have no
/// prerequisite, and steps on other bundles are independent.
pub(crate) fn blocking_prerequisite<'a>(
    steps: &'a [NextStep],
    step: &NextStep,
) -> Option<&'a NextStep> {
    let dependent_bundle = match step {
        NextStep::Delegate { .. }
        | NextStep::AdvanceDelegation { .. }
        | NextStep::AdvanceImportedDelegation { .. } => return None,
        NextStep::CastVote { bundle_index, .. }
        | NextStep::AdvanceVote { bundle_index, .. }
        | NextStep::AdvanceVoteBatch { bundle_index, .. }
        | NextStep::SubmitShares { bundle_index, .. }
        | NextStep::ConfirmShare { bundle_index, .. } => *bundle_index,
    };
    steps.iter().find(|candidate| {
        matches!(
            candidate,
            NextStep::Delegate { bundle_index }
                | NextStep::AdvanceDelegation { bundle_index }
                | NextStep::AdvanceImportedDelegation { bundle_index }
                if *bundle_index == dependent_bundle
        )
    })
}

fn select_primary_action(
    steps: &[NextStep],
    blocking_recovery: bool,
    blocking_share_work: bool,
    completed_for_display: bool,
) -> RoundPlanAction {
    if completed_for_display {
        return RoundPlanAction::Done;
    }
    if !blocking_recovery {
        return RoundPlanAction::Idle;
    }
    if steps.iter().any(|step| {
        matches!(
            step,
            NextStep::Delegate { .. }
                | NextStep::AdvanceDelegation { .. }
                | NextStep::AdvanceImportedDelegation { .. }
        )
    }) {
        return RoundPlanAction::Delegate;
    }
    if steps.iter().any(|step| {
        matches!(
            step,
            NextStep::CastVote { .. }
                | NextStep::AdvanceVote { .. }
                | NextStep::AdvanceVoteBatch { .. }
                | NextStep::SubmitShares { .. }
        )
    }) {
        return RoundPlanAction::Vote;
    }
    if blocking_share_work {
        RoundPlanAction::SubmitShares
    } else {
        RoundPlanAction::Idle
    }
}

fn completed_vote_display(
    proposal_ids: &[u32],
    intents: &BTreeMap<u32, Decision>,
    vote_choices: &BTreeMap<(u32, u32), u32>,
    stale_vote_keys: &BTreeSet<(u32, u32)>,
    voted_at: Option<u64>,
) -> CompletedVoteDisplay {
    let choices = proposal_ids
        .iter()
        .map(|&proposal_id| {
            let proposal_choices = vote_choices
                .iter()
                .filter_map(|(&(bundle_index, vote_proposal_id), &choice)| {
                    (vote_proposal_id == proposal_id
                        && !stale_vote_keys.contains(&(bundle_index, vote_proposal_id)))
                    .then_some(choice)
                })
                .collect::<BTreeSet<_>>();
            let choice = match intents.get(&proposal_id) {
                Some(Decision::Skipped) => None,
                Some(Decision::Choice(_)) if proposal_choices.len() == 1 => {
                    proposal_choices.first().copied()
                }
                _ => None,
            };
            CompletedVoteChoice {
                proposal_id,
                choice,
            }
        })
        .collect();

    CompletedVoteDisplay { choices, voted_at }
}

fn missing_recovery_field(message: String) -> VotingError {
    VotingError::Internal { message }
}

fn recovered_delegation_work_from_steps(
    snapshot: &RoundSnapshot,
    delegation: &BTreeMap<u32, DelegationPhase>,
    obligations: &[Obligation],
    steps: &[NextStep],
) -> Result<Vec<DelegationRecoveryWork>, VotingError> {
    let round_id = snapshot.round_id.as_str();
    // The phase and hash an advance obligation carries; a step the classifier
    // did not emit has none.
    let advance = |step: &NextStep| -> Option<(DelegationPhase, Option<String>)> {
        match resolve_step(obligations, step)? {
            Obligation::AdvanceDelegation { phase, tx_hash, .. } => Some((*phase, tx_hash.clone())),
            _ => None,
        }
    };
    let mut work = Vec::<DelegationRecoveryWork>::new();
    for step in steps {
        match *step {
            NextStep::Delegate { bundle_index } => {
                let phase = delegation.get(&bundle_index).copied().ok_or_else(|| {
                    missing_recovery_field(format!(
                        "delegate step missing phase for round={round_id}, bundle={bundle_index}"
                    ))
                })?;
                work.push(DelegationRecoveryWork {
                    kind: DelegationRecoveryWorkKind::Delegate,
                    bundle_index,
                    phase,
                    tx_hash: None,
                });
            }
            NextStep::AdvanceDelegation { bundle_index } => {
                let (phase, tx_hash) = advance(step).ok_or_else(|| {
                    missing_recovery_field(format!(
                        "poll delegation step missing phase for round={round_id}, bundle={bundle_index}"
                    ))
                })?;
                // A reserved-but-undispatched generation has no hash yet, so
                // the hash is reported when known rather than required.
                work.push(DelegationRecoveryWork {
                    kind: DelegationRecoveryWorkKind::AdvanceDelegation,
                    bundle_index,
                    phase,
                    tx_hash,
                });
            }
            NextStep::AdvanceImportedDelegation { bundle_index } => {
                let (phase, tx_hash) = advance(step).ok_or_else(|| {
                    missing_recovery_field(format!(
                        "imported delegation step missing phase for round={round_id}, bundle={bundle_index}"
                    ))
                })?;
                work.push(DelegationRecoveryWork {
                    kind: DelegationRecoveryWorkKind::AdvanceImportedDelegation,
                    bundle_index,
                    phase,
                    tx_hash,
                });
            }
            // Listed exhaustively on purpose: a new step must be classified
            // here rather than silently dropped.
            NextStep::CastVote { .. }
            | NextStep::AdvanceVote { .. }
            | NextStep::AdvanceVoteBatch { .. }
            | NextStep::SubmitShares { .. }
            | NextStep::ConfirmShare { .. } => {}
        }
    }
    Ok(work)
}

fn recovered_vote_work_from_steps(
    snapshot: &RoundSnapshot,
    blocking_confirm_share_keys: &BTreeSet<(u32, u32, u32)>,
    obligations: &[Obligation],
    steps: &[NextStep],
) -> Result<Vec<VoteRecoveryWork>, VotingError> {
    let round_id = snapshot.round_id.as_str();
    let mut work = Vec::<VoteRecoveryWork>::new();
    // Every member of a unit whose chain work is still pending, so a share
    // retry on such a vote waits for confirmation instead of being grouped
    // as delivery work.
    let pending_vote_confirmation_keys: BTreeSet<(u32, u32)> = obligations
        .iter()
        .filter_map(|obligation| match obligation {
            Obligation::ReconcileChain {
                bundle_index,
                ordered_proposal_ids,
                ..
            } => Some(
                ordered_proposal_ids
                    .iter()
                    .map(move |proposal_id| (*bundle_index, *proposal_id)),
            ),
            _ => None,
        })
        .flatten()
        .collect();
    for step in steps {
        match *step {
            // A reserved-but-undispatched generation has no hash yet, so the
            // hash is reported when known rather than required.
            NextStep::AdvanceVote {
                bundle_index,
                proposal_id,
            }
            | NextStep::AdvanceVoteBatch {
                bundle_index,
                proposal_id,
            } => {
                let reconcile = resolve_step(obligations, step).ok_or_else(|| {
                    missing_recovery_field(format!(
                        "advance vote batch step missing active batch for round={round_id}, bundle={bundle_index}, proposal={proposal_id}"
                    ))
                })?;
                let Obligation::ReconcileChain { unit, tx_hash, .. } = reconcile else {
                    unreachable!(
                        "resolve_step pairs an advance step with its reconcile obligation"
                    );
                };
                work.push(VoteRecoveryWork {
                    kind: match unit {
                        VoteUnitId::Singleton { .. } => VoteRecoveryWorkKind::AdvanceVote,
                        VoteUnitId::Batch { .. } => VoteRecoveryWorkKind::AdvanceVoteBatch,
                    },
                    bundle_index,
                    proposal_id,
                    tx_hash: tx_hash.clone(),
                    vc_tree_position: None,
                    share_indexes: Vec::new(),
                });
            }
            NextStep::SubmitShares {
                bundle_index,
                proposal_id,
                share_index,
            } => {
                let Some(Obligation::Deliver {
                    vc_tree_position, ..
                }) = resolve_step(obligations, step)
                else {
                    unreachable!("a SubmitShares step is projected from a Deliver obligation");
                };
                push_submit_share_work(
                    &mut work,
                    bundle_index,
                    proposal_id,
                    share_index,
                    *vc_tree_position,
                );
            }
            NextStep::ConfirmShare {
                bundle_index,
                proposal_id,
                share_index,
            } if blocking_confirm_share_keys.contains(&(
                bundle_index,
                proposal_id,
                share_index,
            )) && !pending_vote_confirmation_keys.contains(&(bundle_index, proposal_id)) =>
            {
                let vc_tree_position = snapshot
                    .confirmed_tree_position(bundle_index, proposal_id)?
                    .ok_or_else(|| {
                        missing_recovery_field(format!(
                            "submit shares step missing vc_tree_position for round={round_id}, bundle={bundle_index}, proposal={proposal_id}"
                        ))
                    })?;
                push_submit_share_work(
                    &mut work,
                    bundle_index,
                    proposal_id,
                    share_index,
                    vc_tree_position,
                );
            }
            // Listed exhaustively on purpose: a new step must be classified
            // here rather than silently dropped.
            NextStep::Delegate { .. }
            | NextStep::AdvanceDelegation { .. }
            | NextStep::AdvanceImportedDelegation { .. }
            | NextStep::CastVote { .. }
            | NextStep::ConfirmShare { .. } => {}
        }
    }
    Ok(work)
}

fn push_submit_share_work(
    work: &mut Vec<VoteRecoveryWork>,
    bundle_index: u32,
    proposal_id: u32,
    share_index: u32,
    vc_tree_position: u64,
) {
    if let Some(existing) = work.iter_mut().find(|item| {
        item.kind == VoteRecoveryWorkKind::SubmitShares
            && item.bundle_index == bundle_index
            && item.proposal_id == proposal_id
    }) {
        existing.share_indexes.push(share_index);
        existing.share_indexes.sort_unstable();
        existing.share_indexes.dedup();
        return;
    }
    work.push(VoteRecoveryWork {
        kind: VoteRecoveryWorkKind::SubmitShares,
        bundle_index,
        proposal_id,
        tx_hash: None,
        vc_tree_position: Some(vc_tree_position),
        share_indexes: vec![share_index],
    });
}

/// Derived predicates describing what kind of work a plan still contains.
///
/// Every arm is listed explicitly. Adding a [`NextStep`] variant must be a
/// compile error here: a host that scans step kinds through an allowlist reads
/// an unrecognised kind as "no work", which silently strands a round, so the
/// classification has to live in one place the compiler checks.
pub(crate) struct PlanWorkSummary {
    pub(crate) needs_delegation_signing: bool,
    pub(crate) has_in_flight_delegation: bool,
    pub(crate) delegation_bundles_needing_work: Vec<u32>,
    pub(crate) delegation_bundles_needing_signing: Vec<u32>,
    pub(crate) needs_vote_polling: bool,
    pub(crate) has_remaining_vote_or_share_work: bool,
    pub(crate) has_recoverable_vote_or_share_work: bool,
}

pub(crate) fn summarize_plan_work(
    steps: &[NextStep],
    blocking_share_work: bool,
) -> PlanWorkSummary {
    // Collected as sets so a bundle with several delegation steps is named
    // once, and reported ascending so a host can compare two plans directly.
    let mut needing_work = std::collections::BTreeSet::new();
    let mut needing_signing = std::collections::BTreeSet::new();
    let mut summary = PlanWorkSummary {
        needs_delegation_signing: false,
        has_in_flight_delegation: false,
        delegation_bundles_needing_work: Vec::new(),
        delegation_bundles_needing_signing: Vec::new(),
        needs_vote_polling: false,
        has_remaining_vote_or_share_work: false,
        has_recoverable_vote_or_share_work: false,
    };
    for step in steps {
        match step {
            NextStep::Delegate { bundle_index, .. } => {
                needing_work.insert(*bundle_index);
            }
            NextStep::AdvanceDelegation { bundle_index, .. } => {
                summary.needs_delegation_signing = true;
                summary.has_in_flight_delegation = true;
                needing_work.insert(*bundle_index);
                // In flight is not signed and done: advancing one re-signs the
                // locked generation, so the bundle still needs the voter's
                // signing material. Leaving it out here would disagree with
                // the flag set on the line above.
                needing_signing.insert(*bundle_index);
            }
            NextStep::AdvanceImportedDelegation { bundle_index, .. } => {
                summary.has_in_flight_delegation = true;
                needing_work.insert(*bundle_index);
                // The one delegation step that never asks the voter for a
                // signer: an imported capability is already broadcast, so this
                // adopts its package hash and polls.
            }
            NextStep::CastVote { .. }
            | NextStep::AdvanceVote { .. }
            | NextStep::AdvanceVoteBatch { .. }
            | NextStep::SubmitShares { .. } => {
                summary.needs_vote_polling = true;
                summary.has_remaining_vote_or_share_work = true;
                summary.has_recoverable_vote_or_share_work = true;
            }
            NextStep::ConfirmShare { .. } => {
                summary.has_recoverable_vote_or_share_work = true;
                if blocking_share_work {
                    summary.has_remaining_vote_or_share_work = true;
                }
            }
        }
    }
    summary.delegation_bundles_needing_work = needing_work.into_iter().collect();
    summary.delegation_bundles_needing_signing = needing_signing.into_iter().collect();
    summary
}
