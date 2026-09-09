//! Which helpers a run actually reached.
//!
//! Durable state answers where a share *ended up*. It cannot answer where a run
//! declined to send one again, and that is precisely what the multi-URL
//! invariants are about: a resumed run that correctly skipped a helper leaves
//! no row saying so. These records are the other half of that question, read
//! back from the fsynced crash log the child writes as it goes.

use std::collections::BTreeSet;

use crate::child::Observation;

/// Every helper one run's share delivery touched, by outcome.
///
/// Share POSTs only. Readiness probes and status polls reach every helper on
/// every pass and say nothing about placement, so recording them would bury
/// these under thousands of lines while answering no question an assertion
/// asks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HelperContacts {
    /// Helpers that answered a share POST, whatever the status.
    pub answered: BTreeSet<String>,
    /// Helpers that refused the connection.
    pub refused: BTreeSet<String>,
    /// Helpers that accepted the connection and never answered.
    pub unanswered: BTreeSet<String>,
}

impl HelperContacts {
    /// Reads the contacts out of one run's observations.
    pub fn from_observations(observations: &[Observation]) -> Self {
        let mut contacts = Self::default();
        for observation in observations {
            match observation {
                Observation::HelperPost { url, .. } => {
                    contacts.answered.insert(url.clone());
                }
                Observation::HelperRefused { url } => {
                    contacts.refused.insert(url.clone());
                }
                Observation::HelperUnanswered { url } => {
                    contacts.unanswered.insert(url.clone());
                }
                _ => {}
            }
        }
        contacts
    }

    /// Every helper a share POST was aimed at, reachable or not.
    ///
    /// A refusal counts here: the wallet chose that helper and tried, which is
    /// what makes this the right set for asking whether anything outside the
    /// configured fleet was contacted.
    ///
    /// It is the wrong set for asking whether an attempt was journaled. A
    /// refusal is a definite pre-dispatch failure and the SDK clears its
    /// reservation, correctly — see
    /// [`assert_every_unanswered_helper_was_journalled`]. Use
    /// [`unanswered`](Self::unanswered) for that question.
    ///
    /// [`assert_every_unanswered_helper_was_journalled`]: crate::assertions::assert_every_unanswered_helper_was_journalled
    pub fn attempted(&self) -> BTreeSet<String> {
        self.answered
            .union(&self.refused)
            .chain(self.unanswered.iter())
            .cloned()
            .collect()
    }

    /// Whether this run POSTed a share to any helper at all.
    pub fn is_empty(&self) -> bool {
        self.attempted().is_empty()
    }
}
