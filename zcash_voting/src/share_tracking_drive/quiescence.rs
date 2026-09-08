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
    /// The host cancelled, moved to another operation epoch, or switched the
    /// wallet the sidecar is scoped to. Durable effects already made are in
    /// the report.
    ///
    /// All three say the same thing: the run's subject is no longer what it
    /// was admitted for. A host need not tell them apart.
    Cancelled,
    /// A live run is already driving this round's shares, so this one did
    /// nothing.
    ///
    /// Not a failure: the work this run was started for is in flight, and the
    /// run that holds the round reports it. A host that needs the outcome
    /// should read the holder's report rather than retry, and a host that
    /// starts runs on lifecycle events can ignore this entirely.
    ///
    /// A run *departing* — cancelled, or left behind by an epoch change — does
    /// not produce this in its replacement: the replacement waits for the
    /// round to be released and takes it over, so cancelling a run and
    /// starting another does not leave the round undriven.
    ///
    /// That wait is **unbounded**, and ends only when the departing holder
    /// releases the round or the waiting caller is itself cancelled. A holder
    /// releases by completing or being dropped, so a host that retains a
    /// cancelled run's future without polling it to completion or dropping it
    /// leaves its replacement waiting indefinitely.
    AlreadyDriving,
    /// Passes kept failing. The shares are untouched and a later run may still
    /// succeed, so this is a reason to back off and surface state, not a
    /// terminal verdict on the round.
    Failing {
        /// Consecutive failures that ended the run, most recent last.
        messages: Vec<String>,
    },
    /// The host's [`max_passes`](super::ShareTrackingDrivePolicy::max_passes)
    /// was reached with shares still unconfirmed.
    ///
    /// Only a host that sets one can see this: the budget is off by default,
    /// because vote end is what ends a healthy run and a pass count cannot be
    /// translated into one. Shares that cannot be repaired by retrying are the
    /// expected way to reach a budget, and the report names them in
    /// `unrecoverable`.
    PassBudgetExhausted {
        /// Shares the last pass reported as beyond repair.
        unrecoverable: Vec<ShareKey>,
    },
}
