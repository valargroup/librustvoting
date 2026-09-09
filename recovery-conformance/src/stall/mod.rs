//! Making one class of network request never answer.
//!
//! The crash matrix models an app that was *killed*. This models the opposite
//! fault: nothing dies, nothing unwinds, and an answer simply never comes. The
//! two are complementary, and the second is the one the SDK defends against
//! entirely on its own — a crash is observable, whereas a hang is only ever
//! ended by a deadline the wallet imposed on itself before making the request.
//!
//! What a stall exercise asks is therefore narrower and sharper than what a
//! crash exercise asks:
//!
//! 1. does the run end at all, or does the request hang forever;
//! 2. is the durable state it leaves classified conservatively — a request that
//!    never reached the network is *definitely unsent*, one that may have is
//!    *possibly delivered*;
//! 3. does the round still converge once the endpoint answers again.
//!
//! Only the first is new. The second and third are the crash matrix's own
//! oracles, reused, which is deliberate: a fault that leaves durable state the
//! existing assertions already understand is a fault this suite can judge.

mod classify;
mod route;
mod stall_record;
mod target;

pub use classify::RequestClassifier;
pub use route::StallingRoute;
pub use stall_record::StallRecord;
pub use target::{StallPoint, StallTarget, UnknownStallTarget};

/// What one run is asked to hang on.
///
/// One target per run, like one crash stage per run, and for the same reason:
/// a run that hung two classes would leave durable state neither target's
/// assertions describe, and the matrix could not say which fault produced it.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StallPlan {
    /// The class to hang, or `None` for a run that stalls nothing.
    pub target: Option<StallTarget>,
    /// Whether the dispatch hook fires before the answer stops coming.
    pub point: StallPoint,
}

impl StallPlan {
    /// A plan that stalls nothing, for control and resume runs.
    pub fn none() -> Self {
        Self::default()
    }

    /// A plan that hangs `target` at `point`.
    pub fn hanging(target: StallTarget, point: StallPoint) -> Self {
        Self {
            target: Some(target),
            point,
        }
    }

    /// The target this plan arms through the route, if any.
    ///
    /// [`StallTarget::Lightwalletd`] is deliberately excluded: it is not
    /// reached through the route, and returning it here would arm a wrapper
    /// that can never fire, which would look like a stall that did not happen
    /// rather than one that was never the route's to make.
    pub fn armed_target(&self) -> Option<StallTarget> {
        self.target.filter(|target| target.is_routed())
    }
}
