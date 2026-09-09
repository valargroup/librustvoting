//! What one helper does when the wallet reaches for it.
//!
//! Three states, because three is what the SDK's classification actually
//! distinguishes. A refusal is a *definite* failure the wallet may retry
//! elsewhere without ambiguity; a silence is an *unknown* outcome it must
//! journal and reconcile; an answer is a placement. Collapsing the first two
//! into one "down" state would erase the distinction these scenarios exist to
//! test.

/// How a synthetic helper behaves for one run.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum HelperAvailability {
    /// The request is routed to the real backend and really answered.
    ///
    /// The wallet's journal still records the synthetic URL, which is what
    /// recovery reasons over; only the connection goes elsewhere.
    Answers,
    /// The connection is refused before any byte is dispatched.
    ///
    /// Definite, and the SDK may treat it as such: nothing was delivered, so
    /// another helper may be tried immediately with no ambiguity to carry.
    Refuses,
    /// The connection is accepted and no answer ever comes.
    ///
    /// The ambiguous case. Whether the helper took the share is unknowable, so
    /// the attempt must stay journaled as outcome-unknown rather than being
    /// written off — and must not be re-POSTed outside the deliberate,
    /// duplicate-safe overdue retry.
    NeverAnswers,
}

impl HelperAvailability {
    /// Whether a share POSTed here can be definitely accepted.
    pub fn can_accept(self) -> bool {
        matches!(self, Self::Answers)
    }

    /// Whether reaching this helper leaves the outcome unknown.
    ///
    /// True only for [`NeverAnswers`](Self::NeverAnswers). A refusal is a
    /// definite answer, even though it is not a helpful one.
    pub fn leaves_outcome_unknown(self) -> bool {
        matches!(self, Self::NeverAnswers)
    }
}
