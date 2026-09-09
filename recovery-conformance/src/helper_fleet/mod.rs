//! A helper fleet the suite can turn on and off, one helper at a time.
//!
//! # Why this exists
//!
//! Driven against a single helper, the SDK's placement layer collapses to a
//! degenerate case: the target count is one, the per-helper share quota is the
//! whole commitment, and the minimum planning pool is one server. Every rule
//! about splitting a vote's shares across a fleet, repairing a partial
//! deficit, resuming against a plan whose targets are now unreachable, and
//! never re-POSTing to a helper that already accepted is unreachable code as
//! far as a one-URL run is concerned.
//!
//! # Why the helpers are synthetic
//!
//! Only the staging primary answers the share endpoint; the secondary and the
//! PIR host return 404. So the fleet is [`SYNTHETIC_HELPER_URLS`] — names that
//! resolve to nothing — and reachability is decided in the route rather than
//! by the network. An [`Answers`](HelperAvailability::Answers) helper has its
//! request routed to the real primary, so the POST, the acceptance, and the
//! response are genuine; a [`Refuses`](HelperAvailability::Refuses) or
//! [`NeverAnswers`](HelperAvailability::NeverAnswers) helper never leaves the
//! process.
//!
//! The wallet's durable journal records the synthetic URL either way, and that
//! is the identity `attempting_urls`, `sent_to_urls`, and the persisted
//! planning fleet are all written in terms of — so what recovery reasons over
//! is exactly what these scenarios control.
//!
//! # The one thing this does not model
//!
//! Every answering helper shares one backend, so the fleet has ten identities
//! and one opinion. A scenario where two helpers disagree about a share is not
//! expressible here.

mod availability;
mod contact_log;
mod route;
mod scenario;

pub use availability::HelperAvailability;
pub use contact_log::HelperContacts;
pub use route::HelperFleetRoute;
pub use scenario::{FleetScenario, UnknownScenario};

use std::collections::BTreeMap;

/// The synthetic helper fleet, in configuration order.
///
/// Ten, which is not an arbitrary round number. The target count is half the
/// fleet rounded up and capped at ten, so ten helpers put each share on five
/// and sit exactly on the protocol's cap — pinning the cap against a live fleet
/// rather than only against a unit test. The per-helper initial quota of twelve
/// of sixteen shares then forces a planning pool of seven, so a scenario with
/// half the fleet up sits *below* the pool a complete batch needs, which is the
/// widening rule nothing else exercises.
///
/// `.invalid` is the reserved TLD: these names cannot accidentally resolve, so
/// a bug in the route wrapper fails as a DNS error rather than by quietly
/// reaching some real host. Each is already in canonical helper form — lower
/// case, no default port, no trailing slash — so the URL the journal records is
/// the URL named here.
pub const SYNTHETIC_HELPER_URLS: &[&str] = &[
    "https://helper-01.recovery-conformance.invalid",
    "https://helper-02.recovery-conformance.invalid",
    "https://helper-03.recovery-conformance.invalid",
    "https://helper-04.recovery-conformance.invalid",
    "https://helper-05.recovery-conformance.invalid",
    "https://helper-06.recovery-conformance.invalid",
    "https://helper-07.recovery-conformance.invalid",
    "https://helper-08.recovery-conformance.invalid",
    "https://helper-09.recovery-conformance.invalid",
    "https://helper-10.recovery-conformance.invalid",
];

/// Which synthetic helpers are reachable for one run, and what serves them.
///
/// Empty by default, which is what keeps every existing crash run unchanged:
/// with no synthetic helper named, the route wrapper delegates everything.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HelperFleetPlan {
    /// Real endpoint that answering helpers are routed to.
    ///
    /// Empty when the plan names no helpers. A plan with helpers and no
    /// backend would route an answering helper nowhere, so
    /// [`resolve`](Self::resolve) treats that as unreachable rather than
    /// silently passing the synthetic URL to the network.
    pub backend: String,
    /// Availability per synthetic helper base URL.
    pub availability: BTreeMap<String, HelperAvailability>,
}

impl HelperFleetPlan {
    /// A plan naming no synthetic helpers, for runs that use the real fleet.
    pub fn none() -> Self {
        Self::default()
    }

    /// A fleet of `size` helpers served by `backend`, all answering.
    ///
    /// Panics above [`SYNTHETIC_HELPER_URLS`]'s length, because a fleet larger
    /// than the names available is a mistake in a scenario rather than a
    /// condition to degrade through.
    pub fn all_answering(backend: impl Into<String>, size: usize) -> Self {
        assert!(
            size <= SYNTHETIC_HELPER_URLS.len(),
            "the synthetic fleet has {} helpers, not {size}",
            SYNTHETIC_HELPER_URLS.len()
        );
        Self {
            backend: backend.into(),
            availability: SYNTHETIC_HELPER_URLS[..size]
                .iter()
                .map(|url| ((*url).to_string(), HelperAvailability::Answers))
                .collect(),
        }
    }

    /// The same fleet with `helpers` set to `availability`.
    ///
    /// Named by URL rather than by index so a scenario reads as a statement
    /// about which helpers are up, and so flipping a fleet cannot silently
    /// shift by one.
    pub fn with(mut self, helpers: &[&str], availability: HelperAvailability) -> Self {
        for helper in helpers {
            self.availability
                .insert((*helper).to_string(), availability);
        }
        self
    }

    /// The helper URLs this plan configures, in fleet order.
    ///
    /// This is what a host reports as its configured fleet, and it includes
    /// unreachable helpers: a helper that is down is still configured, and the
    /// difference between "unreachable" and "removed from the fleet" is a
    /// distinction the SDK draws and these scenarios rely on.
    pub fn configured_urls(&self) -> Vec<String> {
        SYNTHETIC_HELPER_URLS
            .iter()
            .filter(|url| self.availability.contains_key(**url))
            .map(|url| (*url).to_string())
            .collect()
    }

    /// The synthetic helper `url` addresses, with its availability.
    ///
    /// `None` for any URL outside the fleet, which is every chain, PIR, and
    /// tree request.
    pub fn resolve(&self, url: &str) -> Option<(&str, HelperAvailability)> {
        if self.backend.is_empty() {
            return None;
        }
        self.availability
            .iter()
            .find(|(base, _)| url.starts_with(base.as_str()))
            .map(|(base, availability)| (base.as_str(), *availability))
    }

    /// `url` with its synthetic helper base replaced by the real backend.
    pub fn route_to_backend(&self, url: &str) -> Option<String> {
        let (base, _) = self.resolve(url)?;
        let suffix = url.strip_prefix(base)?;
        Some(format!("{}{suffix}", self.backend.trim_end_matches('/')))
    }
}
