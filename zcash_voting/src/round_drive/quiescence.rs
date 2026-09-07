//! Why a round run stopped, and how that reason is chosen.

use crate::{
    round_planning::Obligation,
    session::{NextStep, RoundPlan},
    share_tracking::ShareKey,
    ChainSubmissionResult,
};

use super::{run_ledger::Run, selection};

/// The state a run ended in.
///
/// Exhaustive over the reasons the driver stops, so a host decides what to
/// show or do next from this alone rather than by re-reading the plan.
///
/// The variants a plan with nothing dispatchable can produce are ranked, most
/// urgent first: [`Failures`](Self::Failures),
/// [`PersistedChainTerminal`](Self::PersistedChainTerminal),
/// [`NeedsBundleSetup`](Self::NeedsBundleSetup),
/// [`NeedsBallot`](Self::NeedsBallot),
/// [`BackgroundShareWorkOnly`](Self::BackgroundShareWorkOnly),
/// [`NoWorkLeft`](Self::NoWorkLeft). Anything the host must act on outranks a
/// handoff that asks nothing of it.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum RoundQuiescence {
    /// The plan lists no actionable obligation. Nothing is owed.
    NoWorkLeft,
    /// Ballot choices exist, but no bundle plan has been persisted yet.
    ///
    /// The host must run bundle setup and then drive the round again. No vote
    /// work can be planned until the bundle rows exist.
    NeedsBundleSetup,
    /// Durable chain state the run cannot advance: a rejected or hashless
    /// terminal submission, or a managed one the plan projects no step for.
    ///
    /// Terminal submissions deliberately schedule no retry. The host must
    /// surface the persisted state for manual handling; the report's plan
    /// carries any projected bundle diagnostics.
    ///
    /// Reported whenever nothing dispatchable is left, not only for an empty
    /// plan: a round can hold a rejected submission for one bundle while
    /// another bundle's shares are still being tracked in the background, and
    /// the rejection is the part the host has to act on.
    PersistedChainTerminal,
    /// A cast is due but withheld until the ballot is terminal.
    ///
    /// The driver never clears an unrostered intent itself: clearing one is a
    /// decision about what the voter meant, and the specification makes it the
    /// host's act.
    NeedsBallot {
        open_proposals: Vec<u32>,
        unrostered_intents: Vec<u32>,
    },
    /// Delegation is owed for these bundles but no signature is available:
    /// the host passed no `DelegationStepInputs`, or a Keystone signer has
    /// nothing stored for the bundle. Nothing was dispatched, so the host can
    /// collect signatures and run again.
    NeedsDelegationSignatures { bundles: Vec<u32> },
    /// Only helper shares a helper has already accepted remain, and nothing
    /// above ranks higher. Background tracking finishes them by polling, so
    /// the foreground vote flow may close.
    BackgroundShareWorkOnly { shares: Vec<ShareKey> },
    /// The host cancelled, or moved to another operation epoch.
    ///
    /// Durable effects already made are in the report. A detached prover may
    /// still hold the bundle lock for the epoch just left, so a run started
    /// again immediately can queue behind it.
    Cancelled,
    /// A chain submission ended without a confirmation: rejected, or
    /// dispatched without a usable transaction hash. Nothing further is
    /// planned for it and no retry can help.
    ChainTerminal {
        step: NextStep,
        outcome: ChainSubmissionResult,
    },
    /// An advancement episode ended outside `Tracking`, so recovery is
    /// exhausted for now. The submission is not lost: running again later may
    /// still resolve it, which is why this is not `ChainTerminal`.
    ChainRecoveryStalled {
        step: NextStep,
        outcome: ChainSubmissionResult,
    },
    /// Every remaining obligation belongs to a bundle a failure skipped, or
    /// [`FailureIsolation::StopRound`](super::FailureIsolation) ended the run.
    Failures,
    /// [`RoundDrivePolicy::max_dispatches`](super::RoundDrivePolicy) was
    /// reached with work still planned. `remaining`, the report plan, and its
    /// tally come from the same fresh read after the final allowed dispatch.
    /// An invariant-level event: report it.
    PassBudgetExhausted { remaining: Vec<NextStep> },
}

/// The reason to stop before dispatching anything from this plan, if any.
///
/// Ordered so that anything the host must act on outranks a handoff that asks
/// nothing of it: a recorded failure, then a persisted submission, then the
/// two setup blockers, then a ballot the voter has not finished, and only then
/// the shares a timer will finish on its own.
pub(super) fn quiesce_before_dispatch(
    plan: &RoundPlan,
    obligations: &[Obligation],
    run: &Run,
) -> Option<RoundQuiescence> {
    // Foreground work remains: drive it. Anything below describes a plan this
    // run cannot advance itself.
    let Some(background_shares) = background_share_handoff(plan, obligations, &run.skipped) else {
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
/// it. A share no helper has reached is delivered rather than polled, so it is
/// foreground work. An empty plan yields an empty handoff, since it too has
/// nothing the foreground can dispatch.
///
/// Both questions are asked per share, of the obligation the classifier
/// produced for it, and only of steps this run would admit. Every round-wide
/// answer is wrong here for the same reason: `blocking_recovery` stays true for
/// a terminal submission that plans no step and for a step on a skipped
/// bundle, and `blocking_share_work` stays true for an undelivered share on a
/// bundle a failure isolated. Each of those made a round whose only admissible
/// steps were shares a helper already held poll them for the entire dispatch
/// budget and report `PassBudgetExhausted` in place of what the host had to act
/// on.
fn background_share_handoff(
    plan: &RoundPlan,
    obligations: &[Obligation],
    skipped: &[u32],
) -> Option<Vec<ShareKey>> {
    plan.next_steps
        .iter()
        .filter(|step| !skipped.contains(&selection::bundle_index(step)))
        .map(|step| match step {
            NextStep::ConfirmShare {
                bundle_index,
                proposal_id,
                share_index,
            } => {
                let share = ShareKey {
                    bundle_index: *bundle_index,
                    proposal_id: *proposal_id,
                    share_index: *share_index,
                };
                accepted_by_a_helper(obligations, &share).then_some(share)
            }
            _ => None,
        })
        .collect()
}

/// Whether the classifier says some helper already holds `share`.
///
/// A share nothing accepted cannot be finished by polling: no helper has it.
fn accepted_by_a_helper(obligations: &[Obligation], share: &ShareKey) -> bool {
    obligations.iter().any(|obligation| {
        matches!(
            obligation,
            Obligation::Confirm {
                bundle_index,
                proposal_id,
                share_index,
                accepted: true,
                ..
            } if *bundle_index == share.bundle_index
                && *proposal_id == share.proposal_id
                && *share_index == share.share_index
        )
    })
}
