//! Evidence that a stall actually happened.
//!
//! A run that simply took a long time and a run whose armed request never
//! answered look identical from the outside, and the difference decides whether
//! the exercise proved anything. The route wrapper writes this record — and
//! fsyncs it — *before* it stops answering, because a stalled run may be ended
//! by its budget rather than by returning, and a record written afterwards
//! would never exist.

use std::time::Duration;

use crate::child::Observation;
use crate::stall::StallTarget;

/// One request that stopped answering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StallRecord {
    /// The target's wire name, as the child recorded it.
    pub target: String,
    pub url: String,
    /// Whether the SDK was told the request may have been delivered.
    pub after_dispatch: bool,
    /// The deadline the SDK put on this very request.
    ///
    /// What turns "the run ended" into "the run ended within the bound the SDK
    /// itself claimed", without the suite having to hardcode that bound.
    pub timeout: Duration,
}

impl StallRecord {
    /// Every stall one run recorded, in the order they fired.
    pub fn from_observations(observations: &[Observation]) -> Vec<Self> {
        observations
            .iter()
            .filter_map(|observation| match observation {
                Observation::RequestStalled {
                    target,
                    url,
                    point_after_dispatch,
                    timeout_milliseconds,
                } => Some(Self {
                    target: target.clone(),
                    url: url.clone(),
                    after_dispatch: *point_after_dispatch,
                    timeout: Duration::from_millis(*timeout_milliseconds),
                }),
                _ => None,
            })
            .collect()
    }

    /// Whether this record is for `target`.
    pub fn is(&self, target: StallTarget) -> bool {
        self.target == target.name()
    }
}
