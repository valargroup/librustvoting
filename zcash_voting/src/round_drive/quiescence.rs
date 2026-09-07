//! Why a round run stopped.

use crate::{session::NextStep, share_tracking::ShareKey, ChainSubmissionResult};

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
