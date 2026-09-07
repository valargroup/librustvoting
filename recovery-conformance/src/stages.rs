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
    /// The delegation obligation was selected and nothing has run.
    BeforeDelegation,
    /// Notes have been selected and nothing is written yet.
    ///
    /// Fires on `PcztBuilding`, the first event after selection returns.
    /// `SelectingNotes` announces the intent to select and is emitted before
    /// any note is chosen, so arming on it would repeat `BeforeDelegation`
    /// under a name promising more.
    AfterNoteSelection,
    /// The PCZT is durable: `bundles.pczt_sighash` and its TX1 effects, which
    /// are write-once. A resumed run must reuse them, never rebuild them.
    AfterPczt,
    /// ZKP #1 is durable in `proofs`. Resume must reuse the proof rather than
    /// re-enter PIR and prove again.
    AfterProof,
    /// The delegation payload is signed. For a Keystone signer the signature
    /// is durable, so resume must not re-prompt the device.
    AfterSigning,
    /// A `Submitting` reservation exists and the request bytes provably never
    /// reached the network. The sharp case: see [`CrashStage::is_sharp`].
    BeforeBroadcast,
    /// The dispatch marker is set and the response was never read. The
    /// delegation may be on chain and the wallet holds no hash for it.
    AfterBroadcastUnread,
    /// The response body was read but never durably classified.
    AfterBroadcastRead,
    /// The submission is `Tracking` with a candidate hash.
    ///
    /// Reaching this needs a one-pass chain policy. `ChainOutcome` is reported
    /// once per `advance_step`, at the end, carrying the episode's *terminal*
    /// outcome rather than each poll — so under the default 45 passes the
    /// episode polls through to confirmation and a stage waiting to see a
    /// submission still tracking never fires. The run arms a single-pass policy
    /// for this stage only, which ends the episode while the submission is
    /// still pending.
    AfterTracking,

    /// The cast obligation was selected; the delegation is confirmed.
    BeforeCast,
    /// The vote-commitment tree synced. A crash here must leave a consistent
    /// cached tree or none at all, never a partially appended one.
    AfterTreeSync,
    /// ZKP #2 is in flight. Nothing is durable, so the proof is lost by
    /// design; the assertion is that nothing is *damaged*.
    AfterVoteProof,
    /// The vote is committed: `votes.commitment_bundle_json` is durable and no
    /// POST has been reserved.
    ///
    /// **Not currently reachable.** The durable boundary is real and worth
    /// covering — a crash here leaves a committed vote with no helper plan —
    /// but nothing observable marks it. Casting persists the vote, prepares
    /// helper plans, and reserves the chain POST inside a single `advance_step`,
    /// so the driver never re-plans in between, and the step's own progress
    /// stream jumps from `VoteCommit(Signing)` straight to
    /// `HelperPlansPrepared`, which is already the *next* boundary
    /// ([`AfterHelperPlans`](Self::AfterHelperPlans)). Reaching it would need a
    /// The seam is the fleet preflight: vote completion probes every helper
    /// after the vote is durably committed and before its helper plans are
    /// written, so a crash on that request lands between the two commits.
    AfterVoteCommit,
    /// Helper delivery plans and the round's immediate-share designation are
    /// durable. This is the commit that makes a confirmed-vote-without-a-plan
    /// unreachable.
    AfterHelperPlans,
    /// A `Submitting` reservation exists for the vote, pre-dispatch.
    BeforeVoteBroadcast,
    /// The vote POST crossed the dispatch boundary.
    AfterVoteBroadcast,
    /// The vote is confirmed and carries its commitment-tree position.
    AfterVoteConfirmed,

    /// A helper is durably journaled in `attempting_urls` and the POST has not
    /// been sent.
    BeforeSharePost,
    /// The helper answered and the outcome was never written. Indistinguishable
    /// from interruption, and must be treated as ambiguous on resume.
    AfterSharePost,
    /// A helper definitely accepted the share.
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
        Self::AfterSigning,
        Self::BeforeBroadcast,
        Self::AfterBroadcastUnread,
        Self::AfterBroadcastRead,
        Self::AfterTracking,
        Self::BeforeCast,
        Self::AfterTreeSync,
        Self::AfterVoteProof,
        Self::AfterVoteCommit,
        Self::AfterHelperPlans,
        Self::BeforeVoteBroadcast,
        Self::AfterVoteBroadcast,
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
        use SubmissionKind::{Delegation, Vote};
        match self {
            Self::BeforeBroadcast => broadcast(Delegation, BeforeDispatch),
            Self::AfterBroadcastUnread => broadcast(Delegation, AfterDispatch),
            Self::AfterBroadcastRead => broadcast(Delegation, AfterResponse),
            Self::BeforeVoteBroadcast => broadcast(Vote, BeforeDispatch),
            Self::AfterVoteBroadcast => broadcast(Vote, AfterDispatch),
            _ => CrashTrigger::Event,
        }
    }

    /// Whether reaching this stage may already have changed staging.
    ///
    /// This classifies the armed run only. Every full matrix exercise still
    /// needs its own round because its unarmed resume continues to quiescence
    /// and may submit after any crash stage.
    pub fn touches_chain(self) -> bool {
        !matches!(
            self,
            Self::BeforeDelegation
                | Self::AfterNoteSelection
                | Self::AfterPczt
                | Self::AfterProof
                | Self::AfterSigning
                | Self::BeforeBroadcast
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
