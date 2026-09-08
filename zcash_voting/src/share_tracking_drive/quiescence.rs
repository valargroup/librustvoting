//! Why a share-tracking run stopped.

use crate::share_tracking::ShareKey;

/// The state a tracking run ended in.
///
/// Exhaustive over the reasons the driver stops, so a host decides what to
/// show or do next from this alone rather than by re-reading share rows.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShareTrackingQuiescence {
    /// The round had no unconfirmed share when the run started. Nothing was
    /// polled and nothing is owed.
    NothingToTrack,
    /// Every share the run was tracking is durably confirmed.
    AllConfirmed,
    /// The round's vote end passed. Recovery closes there, so no later pass
    /// could resubmit or confirm anything.
    VoteEndReached,
    /// The host cancelled, or moved to another operation epoch. Durable
    /// effects already made are in the report.
    Cancelled,
    /// Passes kept failing. The shares are untouched and a later run may still
    /// succeed, so this is a reason to back off and surface state, not a
    /// terminal verdict on the round.
    Failing {
        /// Consecutive failures that ended the run, most recent last.
        messages: Vec<String>,
    },
    /// [`max_passes`](super::ShareTrackingDrivePolicy::max_passes) was reached
    /// with shares still unconfirmed.
    ///
    /// Reaching it is an invariant-level event: the vote-end boundary normally
    /// ends a run first. Shares that cannot be repaired by retrying are the
    /// expected way to get here, and the report names them in
    /// `unrecoverable`.
    PassBudgetExhausted {
        /// Shares the last pass reported as beyond repair.
        unrecoverable: Vec<ShareKey>,
    },
}
