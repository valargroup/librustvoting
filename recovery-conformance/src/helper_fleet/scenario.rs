//! The multi-helper situations the suite drives a round through.
//!
//! Each scenario is a pair of fleets — the one a round starts under and the one
//! it is resumed under — plus, where the point is a crash rather than an
//! outage, the stage to crash at. Written as a taxonomy with stable names and a
//! strict parser for the same reason the crash stages are: a scenario that
//! silently selects nothing reports a green run over untested ground.
//!
//! # The arithmetic every scenario is written against
//!
//! Ten configured helpers put each share on five, allow twelve of sixteen
//! shares per helper initially, and need a planning pool of seven for a
//! complete batch. Those three numbers decide what each scenario can prove, and
//! `tests/helper_fleet_plan.rs` pins them so a scenario cannot keep passing
//! while quietly testing something else.

use std::fmt;
use std::str::FromStr;

use crate::helper_fleet::{HelperAvailability, HelperFleetPlan, SYNTHETIC_HELPER_URLS};
use crate::stages::CrashStage;

/// Helpers that answer in the first half of a split scenario.
///
/// Four, not five. Five would be exactly the target count, so a fleet with half
/// up could meet every share's target on its own and leave nothing for the
/// second half to repair — the scenario would report a pass having exercised no
/// deficit at all. Four is strictly below the target, so a deficit is
/// guaranteed and can only be filled by helpers the first run never tried.
const FIRST_HALF: usize = 4;

/// Helpers that go silent rather than refusing, in the scenario that needs both.
///
/// Two, and deliberately few. A silent helper is bounded only by the per-share
/// fan-out budget, so a fleet of them costs that budget on every share; a
/// handful demonstrates the ambiguous-outcome rule without turning one scenario
/// into the longest run in the suite.
const SILENT: std::ops::Range<usize> = 4..6;

/// The fleet a contracted run is configured with.
///
/// Six of ten, which drops the effective target from five to three. Below the
/// planning pool of seven, so the persisted plan necessarily names helpers that
/// are no longer configured — which is the case the no-replanning rule exists
/// for.
const CONTRACTED: usize = 6;

/// One multi-helper situation a round is driven through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FleetScenario {
    /// Every helper answers, the process is killed just after one accepted, and
    /// the resume must finish the placement.
    ///
    /// The plain form of the question: having already placed a share with some
    /// helpers, does a restarted round send only to the ones still owed?
    FullFleetThenCrash,
    /// Four helpers answer and six refuse, the process is killed mid-delivery,
    /// and then those four refuse while the six answer.
    ///
    /// No helper that served the first run is reachable for the second, so
    /// every remaining share must be placed on a helper never tried — while the
    /// acceptances already recorded, now unreachable, must survive untouched.
    ///
    /// The crash is what makes this a test. Without it the first half simply
    /// finishes the round: a share **confirms at whatever placement it
    /// reaches**, so merely under-delivering leaves nothing outstanding, the
    /// resume reports `NothingToTrack`, and the flip never happens. The first
    /// live run of this scenario passed exactly that way, with all 144 shares
    /// confirmed on the first half and the second half never contacted.
    HalfThenOtherHalf,
    /// As above, crash included, but two of the unreachable helpers go silent
    /// instead of refusing.
    ///
    /// Where the fleet axis meets the hang axis. A refusal is a definite answer
    /// the wallet may act on; silence is not, and the attempt must stay
    /// journaled as outcome-unknown rather than being written off or replayed.
    SilentHelpers,
    /// Nothing is reachable, then everything is.
    ///
    /// The deficit must survive the outage intact: a round that cannot deliver
    /// must not record that it did, and must still owe the whole placement when
    /// the fleet returns.
    WholeFleetDown,
    /// Ten helpers answer, then the run is resumed configured with six.
    ///
    /// Fleet contraction rather than an outage — a different rule with a
    /// different expected outcome. The effective target clamps to the smaller
    /// fleet, helpers no longer configured are never contacted, and the
    /// persisted plan is not redrawn even though it names them.
    FleetContractsThenGrows,
}

impl FleetScenario {
    /// Every scenario, in the order they are run.
    pub const ALL: &'static [Self] = &[
        Self::FullFleetThenCrash,
        Self::HalfThenOtherHalf,
        Self::SilentHelpers,
        Self::WholeFleetDown,
        Self::FleetContractsThenGrows,
    ];

    /// The scenario's stable wire name, used in selection and in test names.
    pub fn name(self) -> &'static str {
        match self {
            Self::FullFleetThenCrash => "full-fleet-then-crash",
            Self::HalfThenOtherHalf => "half-then-other-half",
            Self::SilentHelpers => "silent-helpers",
            Self::WholeFleetDown => "whole-fleet-down",
            Self::FleetContractsThenGrows => "fleet-contracts-then-grows",
        }
    }

    /// The stage to crash at, for the scenario whose subject is a crash.
    ///
    /// `None` means the first run is driven to its own quiescence instead. Most
    /// of these scenarios are about an unreachable fleet rather than a killed
    /// process, and adding a crash to them would confuse two faults whose
    /// durable evidence is different.
    pub fn crash_stage(self) -> Option<CrashStage> {
        match self {
            Self::FullFleetThenCrash => Some(CrashStage::AfterShareAccepted),
            // Killed at the first share *outcome*, not the first POST. Both
            // leave the round unfinished, but only this one leaves acceptances
            // behind: `AfterSharePost` fires before anything has been accepted,
            // and a first half holding nothing makes "those acceptances survive
            // the flip" a claim about an empty set. Observed live — a run cut
            // short at the POST placed 730 shares on the second half and zero
            // on the first.
            Self::HalfThenOtherHalf | Self::SilentHelpers => {
                Some(CrashStage::AfterShareAccepted)
            }
            // Nothing can be delivered at all here, so the outstanding work is
            // guaranteed without killing anything.
            Self::WholeFleetDown => None,
            Self::FleetContractsThenGrows => Some(CrashStage::AfterShareAccepted),
        }
    }

    /// Whether the first run must leave the round with work still owed.
    ///
    /// Every scenario here claims the *resumed* run does something, so a first
    /// run that finished the round makes the exercise vacuous: it would assert
    /// truths about a completed round and report a pass. This is the same rule
    /// the crash matrix applies to a stage that stops firing, and it exists
    /// because this suite has already been fooled once — `half-then-other-half`
    /// passed a live run having placed and confirmed all 144 shares on the
    /// first half, with the second half never contacted.
    pub fn must_leave_work_outstanding(self) -> bool {
        true
    }

    /// Whether the first run must leave definite acceptances behind.
    ///
    /// The other half of a flip scenario's claim. Placing the remainder on a
    /// fresh half proves the deficit is filled; it says nothing about whether
    /// acceptances on the *departed* half survived, and that is the property a
    /// buggy implementation would break by treating "cannot reach it now" as
    /// "never had it".
    ///
    /// False where there is nothing to preserve: `whole-fleet-down` accepts
    /// nothing at all by construction.
    pub fn must_leave_acceptances_behind(self) -> bool {
        !matches!(self, Self::WholeFleetDown)
    }

    /// The fleet the first run is driven under.
    pub fn first_fleet(self, backend: &str) -> HelperFleetPlan {
        let all = HelperFleetPlan::all_answering(backend, SYNTHETIC_HELPER_URLS.len());
        match self {
            Self::FullFleetThenCrash | Self::FleetContractsThenGrows => all,
            Self::HalfThenOtherHalf => {
                all.with(&SYNTHETIC_HELPER_URLS[FIRST_HALF..], HelperAvailability::Refuses)
            }
            Self::SilentHelpers => all
                .with(&SYNTHETIC_HELPER_URLS[FIRST_HALF..], HelperAvailability::Refuses)
                .with(&SYNTHETIC_HELPER_URLS[SILENT], HelperAvailability::NeverAnswers),
            Self::WholeFleetDown => all.with(SYNTHETIC_HELPER_URLS, HelperAvailability::Refuses),
        }
    }

    /// The fleet the resumed run is driven under.
    pub fn second_fleet(self, backend: &str) -> HelperFleetPlan {
        let all = HelperFleetPlan::all_answering(backend, SYNTHETIC_HELPER_URLS.len());
        match self {
            Self::FullFleetThenCrash | Self::WholeFleetDown | Self::SilentHelpers => all,
            Self::HalfThenOtherHalf => {
                all.with(&SYNTHETIC_HELPER_URLS[..FIRST_HALF], HelperAvailability::Refuses)
            }
            // A smaller *configured* fleet, not an unreachable one. The helpers
            // it drops are gone from the host's configuration entirely, which
            // is what makes this contraction rather than an outage.
            Self::FleetContractsThenGrows => HelperFleetPlan::all_answering(backend, CONTRACTED),
        }
    }

    /// Whether the second run can reach enough helpers to meet every target.
    ///
    /// False for the contracted fleet, whose effective target is smaller than
    /// the one the shares were planned with. Asserting the original target
    /// there would report the SDK failing to do something the rules forbid.
    pub fn meets_the_full_target(self) -> bool {
        !matches!(self, Self::FleetContractsThenGrows)
    }

    /// Whether the first run leaves a share journaled with an unknown outcome.
    pub fn leaves_an_unknown_outcome(self) -> bool {
        matches!(self, Self::SilentHelpers)
    }
}

impl fmt::Display for FleetScenario {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A selection naming no known scenario.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownScenario(pub String);

impl fmt::Display for UnknownScenario {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown fleet scenario {:?}", self.0)
    }
}

impl std::error::Error for UnknownScenario {}

impl FromStr for FleetScenario {
    type Err = UnknownScenario;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|scenario| scenario.name() == value)
            .ok_or_else(|| UnknownScenario(value.to_string()))
    }
}
