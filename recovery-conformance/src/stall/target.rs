//! Which network request a run hangs on, and where inside it.
//!
//! The taxonomy is a list of *request classes*, not of convenient places to
//! stop. Every variant names one kind of request the SDK makes, and every one
//! has a deadline the SDK is supposed to apply to it. That pairing is the whole
//! point: a crash is an abrupt fault the process can see, while a hang is an
//! answer that never comes, and the only thing between the wallet and a wedged
//! round is a bound it imposes on itself. Naming the classes is how "is this
//! request bounded?" becomes a question a test can ask.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

/// One class of network request a run can be made to hang on.
///
/// Ordered roughly as a round meets them, for the same reason [`CrashStage`]
/// is: a reader scanning the list should see a round's life, not an alphabet.
///
/// [`CrashStage`]: crate::stages::CrashStage
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum StallTarget {
    /// Every lightwalletd RPC, including connection setup.
    ///
    /// The one target not reachable through the route: `lwd.rs` dials tonic
    /// directly rather than through an injected transport, so no wrapper sees
    /// it. Live coverage is excluded until a black-hole listener is added.
    Lightwalletd,
    /// A PIR query, matched by the configured PIR endpoint it addresses.
    ///
    /// The asymmetric one. `pir_client::Transport` takes no timeout argument at
    /// all, so a host supplying its own PIR transport has *no* SDK-side bound;
    /// the budget this target exercises exists only inside `HyperTransport`.
    /// That makes it the class most worth watching for regression.
    PirQuery,
    /// `POST .../shielded-vote/v1/delegate-vote`.
    DelegationPost,
    /// `POST .../shielded-vote/v1/cast-vote` or `.../cast-vote-batch`.
    VotePost,
    /// Fresh delegation and all dependent casts in one POST.
    DelegateAndCastPost,
    /// `GET .../shielded-vote/v1/tx/{hash}` — the submission status poll.
    TransactionLookup,
    /// `GET .../shielded-vote/v1/commitment-tree/...`.
    ///
    /// One class serving two callers, deliberately not split. Chain recovery's
    /// exact-tree scan and the vote-commitment-tree sync issue the same request
    /// to the same path on the same host, and nothing in the request
    /// distinguishes them. Naming two targets that no wrapper could tell apart
    /// would be a taxonomy that lies.
    CommitmentTreeRead,
    /// `GET .../shielded-vote/v1/status` — the helper fleet preflight.
    HelperPreflight,
    /// `POST .../shielded-vote/v1/shares` — share delivery.
    SharePost,
    /// `GET .../shielded-vote/v1/share-status/...` — share confirmation polling.
    ShareStatus,
}

/// Where inside one request the answer stops coming.
///
/// The same distinction [`BroadcastPoint`] draws for crashes, and it carries
/// the same safety claim. The route's dispatch hook is what the SDK classifies
/// by: a failure before it is *definitely unsent* and may be retried freely; a
/// failure after it is *possibly delivered* and must never produce a second
/// generation.
///
/// [`BroadcastPoint`]: crate::stages::BroadcastPoint
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum StallPoint {
    /// The hook is never called: connection setup never completes.
    BeforeDispatch,
    /// The hook is called, then no response ever arrives.
    AfterDispatch,
}

/// `BeforeDispatch` is the default because it is the safer thing to model.
///
/// A stall that never released a byte must be classified as definitely unsent,
/// and a plan built without saying which point it meant should not silently
/// claim the ambiguous case.
impl Default for StallPoint {
    fn default() -> Self {
        Self::BeforeDispatch
    }
}

impl StallTarget {
    /// Targets reported by the fresh combined matrix. Standalone POST classes
    /// remain available to hermetic route tests but are not live selections.
    pub const ALL: &'static [Self] = &[
        Self::Lightwalletd,
        Self::PirQuery,
        Self::DelegateAndCastPost,
        Self::TransactionLookup,
        Self::CommitmentTreeRead,
        Self::HelperPreflight,
        Self::SharePost,
        Self::ShareStatus,
    ];

    /// The target's stable wire name, used in selection and in test names.
    pub fn name(self) -> &'static str {
        match self {
            Self::Lightwalletd => "lightwalletd",
            Self::PirQuery => "pir-query",
            Self::DelegationPost => "delegation-post",
            Self::VotePost => "vote-post",
            Self::DelegateAndCastPost => "delegate-and-cast-post",
            Self::TransactionLookup => "transaction-lookup",
            Self::CommitmentTreeRead => "commitment-tree-read",
            Self::HelperPreflight => "helper-preflight",
            Self::SharePost => "share-post",
            Self::ShareStatus => "share-status",
        }
    }

    /// Whether this target is reached through the shared HTTP route.
    ///
    /// False only for [`Lightwalletd`](Self::Lightwalletd), which needs a
    /// black-hole listener rather than a route wrapper.
    pub fn is_routed(self) -> bool {
        !matches!(self, Self::Lightwalletd)
    }

    /// Whether a stall here can leave a possibly-delivered submission.
    ///
    /// True for the combined and legacy POSTs that carry a transaction. A stall after dispatch
    /// on either leaves the wallet unable to prove the bytes never left, which
    /// is the conservative case the whole recovery model exists for; every
    /// other class is a read, and a read that never answers costs only time.
    pub fn carries_a_submission(self) -> bool {
        matches!(
            self,
            Self::DelegationPost | Self::VotePost | Self::DelegateAndCastPost
        )
    }

    /// The deadline the SDK is expected to apply to this class.
    ///
    /// Recorded here so the matrix can size a run's budget from the bound
    /// itself rather than from a number someone guessed. These are the shipped
    /// values; a run that outlives a generous multiple of its target's bound is
    /// reporting that the bound is not being applied.
    pub fn declared_bound(self) -> Duration {
        match self {
            // `LIGHTWALLETD_UNARY_RPC_TIMEOUT`, the longer of the two lwd
            // bounds; connection setup is capped at 10s inside it.
            Self::Lightwalletd => Duration::from_secs(20),
            // `PIR_REQUEST_BUDGET`, shared by both attempts.
            Self::PirQuery => Duration::from_secs(60),
            // `DEFAULT_CHAIN_POST_TIMEOUT`.
            Self::DelegationPost | Self::VotePost | Self::DelegateAndCastPost => {
                Duration::from_secs(150)
            }
            // `DEFAULT_CHAIN_LOOKUP_TIMEOUT`.
            Self::TransactionLookup => Duration::from_secs(10),
            // `TREE_REQUEST_TIMEOUT`, and `RECOVERY_REQUEST_TIMEOUT` is 60s
            // too, so the two callers of this class agree.
            Self::CommitmentTreeRead => Duration::from_secs(60),
            // The preflight's hard deadline, not its 2s soft one: the soft
            // bound only stops the race waiting, it does not end the request.
            Self::HelperPreflight => Duration::from_secs(30),
            // `SHARE_INITIAL_DELIVERY_TIMEOUT_MILLISECONDS`, the fan-out
            // budget, which is what bounds a share whose POST never answers.
            Self::SharePost => Duration::from_secs(60),
            // `SHARE_STATUS_POLL_BUDGET_MILLISECONDS`.
            Self::ShareStatus => Duration::from_secs(10),
        }
    }
}

impl fmt::Display for StallTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A stall selection that names no known target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownStallTarget(pub String);

impl fmt::Display for UnknownStallTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown stall target {:?}", self.0)
    }
}

impl std::error::Error for UnknownStallTarget {}

impl FromStr for StallTarget {
    type Err = UnknownStallTarget;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|target| target.name() == value)
            .ok_or_else(|| UnknownStallTarget(value.to_string()))
    }
}
