//! Naming the request class a URL belongs to.
//!
//! Split out from the route wrapper and given plain string arguments so it can
//! be tested without building an HTTP request or reaching the network. The
//! classification is the part that rots: paths move, a new endpoint appears,
//! and a wrapper that silently classifies it as "not my target" turns a stall
//! test into a no-op.

use crate::stall::StallTarget;

/// Path prefix every wallet-facing vote and helper route shares.
const API_PREFIX: &str = "/shielded-vote/v1/";

/// Decides which request class a request belongs to.
///
/// PIR is matched by endpoint rather than by path: the PIR client owns its own
/// URL shapes, and matching the configured endpoints is both stabler and the
/// same thing the fleet itself is configured with.
#[derive(Clone, Debug)]
pub struct RequestClassifier {
    pir_urls: Vec<String>,
}

impl RequestClassifier {
    /// Builds a classifier that recognizes `pir_urls` as PIR endpoints.
    pub fn new(pir_urls: Vec<String>) -> Self {
        Self { pir_urls }
    }

    /// The class `url` belongs to, or `None` for a request no target names.
    ///
    /// `None` is not a failure. Plenty of traffic belongs to no target — a
    /// health check, a redirect, an endpoint added later — and such a request
    /// must pass through untouched rather than be swept into the nearest
    /// class.
    pub fn classify(&self, method: &str, url: &str) -> Option<StallTarget> {
        if self.is_pir(url) {
            return Some(StallTarget::PirQuery);
        }
        let path = url.split(API_PREFIX).nth(1)?;
        // The endpoint is the first path segment after the version, so a query
        // string or a nested identifier does not change which class a request
        // is in: `tx/{hash}` and `commitment-tree/{round}/leaves?..` are one
        // segment each here.
        let endpoint = path.split(['/', '?']).next()?;
        let posting = method.eq_ignore_ascii_case("POST");
        match (endpoint, posting) {
            ("delegate-vote", true) => Some(StallTarget::DelegationPost),
            ("cast-vote" | "cast-vote-batch", true) => Some(StallTarget::VotePost),
            ("shares", true) => Some(StallTarget::SharePost),
            ("tx", false) => Some(StallTarget::TransactionLookup),
            ("commitment-tree", false) => Some(StallTarget::CommitmentTreeRead),
            ("status", false) => Some(StallTarget::HelperPreflight),
            ("share-status", false) => Some(StallTarget::ShareStatus),
            // A known endpoint reached by the wrong method is deliberately
            // unclassified rather than forced into its usual class: it is not
            // the request the target names, and stalling it would report a
            // boundary the run never crossed.
            _ => None,
        }
    }

    /// Whether `url` addresses one of the configured PIR endpoints.
    fn is_pir(&self, url: &str) -> bool {
        self.pir_urls
            .iter()
            .any(|endpoint| !endpoint.is_empty() && url.starts_with(endpoint.trim_end_matches('/')))
    }
}
