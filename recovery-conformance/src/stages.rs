//! Where a run is killed, and what durable commit that lands next to.
//!
//! Each stage names one point in a round's life. The taxonomy is not a list of
//! convenient places to stop: every stage sits immediately after, or
//! immediately before, a durable commit named in `docs/chain_submission_invariants.md`
//! and `docs/round_orchestration_invariants.md`, because the whole suite asks
//! one question — given exactly this much durable state, does the round still
//! know what it owes?

use std::fmt;
use std::str::FromStr;

/// One crash point in a round.
///
/// The order of the variants is the order they occur in a round, so a stage
/// that sorts earlier is always reachable from a run driving toward a later
/// one.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum CrashStage {
    /// Delegation proof preparation is about to begin.
    BeforeDelegation,
    /// Notes have been selected; preparation has not written the PCZT.
    AfterNoteSelection,
    /// The PCZT and its signing fields are durable.
    AfterPczt,
    /// The delegation proof is durable and reused on resume.
    AfterProof,
    /// The terminal ballot is ready for combined delegation and casts.
    BeforeCast,
    /// Legacy confirmed-delegation tree sync; excluded from the fresh combined matrix.
    AfterTreeSync,
    /// The delegation payload is signed; cast proofs are not yet durable.
    AfterSigning,
    /// Cast proofs are complete in memory, before durable commitment.
    AfterVoteProof,
    /// The combined authorization and all cast recoveries are durable.
    AfterVoteCommit,
    /// Complete helper plans are durable, before the chain POST.
    AfterHelperPlans,
    /// A combined reservation exists; bytes have not been dispatched.
    BeforeBroadcast,
    /// Alias boundary for the casts in the same combined reservation.
    BeforeVoteBroadcast,
    /// The combined POST was dispatched; its response has not been read.
    AfterBroadcastUnread,
    /// Alias boundary for the casts in the same dispatched envelope.
    AfterVoteBroadcast,
    /// The combined POST response was read but not durably classified.
    AfterBroadcastRead,
    /// One bounded pass recorded the combined candidate; the stage uses a one-pass policy.
    AfterTracking,
    /// The delegation and all cast positions are confirmed together.
    AfterVoteConfirmed,
    /// A helper attempt is durable, before its POST.
    BeforeSharePost,
    /// The helper response has not been durably classified.
    AfterSharePost,
    /// A definite helper acceptance is durable.
    AfterShareAccepted,
}

/// How the harness detects that a stage has been reached.
///
/// The split is not cosmetic. Everything the driver reports passes through
/// `RoundDriveEvent`, but the broadcast boundary is deliberately *not* an
/// event: it lives inside one transport call, between two instructions, and
/// only the transport can observe it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashTrigger {
    /// Fired from the driver's event stream.
    Event,
    /// Fired from inside the chain transport's POST.
    Broadcast {
        submission: SubmissionKind,
        point: BroadcastPoint,
    },
}

/// Which submission a broadcast stage applies to.
///
/// A round POSTs delegations and votes through the same transport, so a
/// broadcast stage that did not name its submission would fire on whichever
/// came first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmissionKind {
    Delegation,
    Vote,
    /// Fresh delegation and all dependent casts in one transaction.
    DelegateAndVoteBatch,
}

/// Where inside one POST the process dies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BroadcastPoint {
    /// Before `ChainPostDispatch::mark_possible`. The bytes never left.
    BeforeDispatch,
    /// After the marker is set, before the response is read.
    AfterDispatch,
    /// After the response is read, before it is durably classified.
    AfterResponse,
}

impl CrashStage {
    /// Every stage, in round order.
    pub const ALL: &'static [Self] = &[
        Self::BeforeDelegation,
        Self::AfterNoteSelection,
        Self::AfterPczt,
        Self::AfterProof,
        Self::BeforeCast,
        Self::AfterSigning,
        Self::AfterVoteProof,
        Self::AfterVoteCommit,
        Self::AfterHelperPlans,
        Self::BeforeBroadcast,
        Self::BeforeVoteBroadcast,
        Self::AfterBroadcastUnread,
        Self::AfterVoteBroadcast,
        Self::AfterBroadcastRead,
        Self::AfterTracking,
        Self::AfterVoteConfirmed,
        Self::BeforeSharePost,
        Self::AfterSharePost,
        Self::AfterShareAccepted,
    ];

    /// The stage's stable wire name, used by `--stage` and in test names.
    pub fn name(self) -> &'static str {
        match self {
            Self::BeforeDelegation => "before-delegation",
            Self::AfterNoteSelection => "after-note-selection",
            Self::AfterPczt => "after-pczt",
            Self::AfterProof => "after-proof",
            Self::AfterSigning => "after-signing",
            Self::BeforeBroadcast => "before-broadcast",
            Self::AfterBroadcastUnread => "after-broadcast-unread",
            Self::AfterBroadcastRead => "after-broadcast-read",
            Self::AfterTracking => "after-tracking",
            Self::BeforeCast => "before-cast",
            Self::AfterTreeSync => "after-tree-sync",
            Self::AfterVoteProof => "after-vote-proof",
            Self::AfterVoteCommit => "after-vote-commit",
            Self::AfterHelperPlans => "after-helper-plans",
            Self::BeforeVoteBroadcast => "before-vote-broadcast",
            Self::AfterVoteBroadcast => "after-vote-broadcast",
            Self::AfterVoteConfirmed => "after-vote-confirmed",
            Self::BeforeSharePost => "before-share-post",
            Self::AfterSharePost => "after-share-post",
            Self::AfterShareAccepted => "after-share-accepted",
        }
    }

    /// How this stage is detected.
    pub fn trigger(self) -> CrashTrigger {
        use BroadcastPoint::{AfterDispatch, AfterResponse, BeforeDispatch};
        use SubmissionKind::DelegateAndVoteBatch;
        match self {
            Self::BeforeBroadcast => broadcast(DelegateAndVoteBatch, BeforeDispatch),
            Self::AfterBroadcastUnread => broadcast(DelegateAndVoteBatch, AfterDispatch),
            Self::AfterBroadcastRead => broadcast(DelegateAndVoteBatch, AfterResponse),
            Self::BeforeVoteBroadcast => broadcast(DelegateAndVoteBatch, BeforeDispatch),
            Self::AfterVoteBroadcast => broadcast(DelegateAndVoteBatch, AfterDispatch),
            _ => CrashTrigger::Event,
        }
    }

    /// Whether reaching this stage may already have changed staging.
    ///
    /// This classifies the armed run only. Every full matrix exercise still
    /// needs its own round because its unarmed resume continues to quiescence
    /// and may submit after any crash stage.
    pub fn touches_chain(self) -> bool {
        matches!(
            self,
            Self::AfterBroadcastUnread
                | Self::AfterVoteBroadcast
                | Self::AfterBroadcastRead
                | Self::AfterTracking
                | Self::AfterVoteConfirmed
                | Self::BeforeSharePost
                | Self::AfterSharePost
                | Self::AfterShareAccepted
        )
    }

    /// Whether the resume must wait for the dispatched transaction to land.
    ///
    /// True only where the stage's premise is that the chain already holds a
    /// transaction the wallet cannot name. Resuming before inclusion leaves the
    /// tree with nothing to find, so recovery resolves by same-generation retry
    /// instead — legal, but it exercises the retry path under a stage that
    /// exists to exercise the tree one.
    pub fn settles_on_chain_before_resume(self) -> bool {
        matches!(self, Self::AfterBroadcastUnread | Self::AfterBroadcastRead)
    }

    /// Whether this stage is one of the two double-spend-adjacent cases.
    ///
    /// `BeforeBroadcast` is conservative-by-design: nothing was sent, yet the
    /// abandoned reservation must still normalize to `Recovering` rather than
    /// disappear, because a restarted process cannot prove the bytes never
    /// left. `AfterBroadcastUnread` is the real ambiguity: the transaction is
    /// on chain and the wallet has no hash for it.
    pub fn is_sharp(self) -> bool {
        matches!(self, Self::BeforeBroadcast | Self::AfterBroadcastUnread)
    }
}

fn broadcast(submission: SubmissionKind, point: BroadcastPoint) -> CrashTrigger {
    CrashTrigger::Broadcast { submission, point }
}

impl fmt::Display for CrashStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A `--stage` value that names no known stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownStage(pub String);

impl fmt::Display for UnknownStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown crash stage {:?}", self.0)
    }
}

impl std::error::Error for UnknownStage {}

impl FromStr for CrashStage {
    type Err = UnknownStage;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|stage| stage.name() == value)
            .ok_or_else(|| UnknownStage(value.to_string()))
    }
}
