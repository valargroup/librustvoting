//! Pacing and failure policy for one round run.

use std::{num::NonZeroUsize, time::Duration};

/// How the round driver paces obligations and isolates failures.
///
/// The driver never overrides a step's own policy. `ChainAdvancePolicy` still
/// bounds one advancement episode: its `pending_repoll` and `max_passes` apply
/// *inside* an `advance_step` call. This policy governs what happens between
/// calls.
#[derive(Clone, Debug)]
pub struct RoundDrivePolicy {
    /// Wait between two attempts at the same unfinished obligation.
    ///
    /// Applied whenever a step returns `Pending`, which is more than chain
    /// tracking. All three of these are paced by it:
    ///
    /// - a chain submission still `Tracking`;
    /// - a share confirmation that became tracking-owned between selection
    ///   and the executor's locked plan, which carries no chain outcome;
    /// - a vote already `Confirmed` on chain whose helper delivery is waiting
    ///   on ambiguous attempts.
    ///
    /// So this is helper retry latency as well as chain re-poll latency; there
    /// is one control for both. A submission that ended an episode in recovery
    /// quiesces instead, so a stuck row surfaces to the host rather than being
    /// retried silently for the rest of the round. The wait ends early on
    /// cancellation or an operation-epoch change, so a host that closes the
    /// session does not pay it, and it is not taken at all once the dispatch
    /// budget is spent.
    pub pending_repoll: Duration,

    /// How many bundle-locked obligations run at once.
    ///
    /// Round-locked obligations are never run concurrently whatever this
    /// value: they contend for one lock, so a second in flight would only
    /// queue behind the first while holding a proving thread open.
    /// [`FailureIsolation::StopRound`] also admits only one step at a time so
    /// no later bundle has started when the first failure stops the run.
    pub max_bundle_concurrency: NonZeroUsize,

    /// What a failed obligation does to the rest of the round.
    pub failure_isolation: FailureIsolation,

    /// Step dispatches before the run stops with
    /// [`RoundQuiescence::PassBudgetExhausted`](super::RoundQuiescence).
    ///
    /// The driver refreshes the plan and tally after the final allowed
    /// dispatch before reporting exhaustion. A zero budget therefore still
    /// reads and reports one authoritative plan.
    ///
    /// A safety net against a plan that never shrinks, not a scheduling knob:
    /// the executor already refuses a step its own locked plan still lists but
    /// cannot resolve, so the ordinary livelock is impossible.
    pub max_dispatches: usize,
}

impl Default for RoundDrivePolicy {
    fn default() -> Self {
        Self {
            pending_repoll: Duration::from_secs(2),
            max_bundle_concurrency: NonZeroUsize::new(3).expect("3 is not zero"),
            failure_isolation: FailureIsolation::SkipBundle,
            max_dispatches: 512,
        }
    }
}

/// What the driver does with the rest of the round after one failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailureIsolation {
    /// Skip the failed obligation's bundle for the rest of the run and keep
    /// driving every other bundle. Durable progress the failed step already
    /// made is kept, and every failure is reported together at the end.
    SkipBundle,
    /// Stop at the first failure. The report still carries the durable effects
    /// of every obligation that completed before it.
    StopRound,
}
