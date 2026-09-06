//! The suite must never point at the production vote chain.
//!
//! The network default happens to be correct today — `Network::Testnet` maps
//! to `svote-1` — but the mapping is a documented convention, not a guarantee,
//! and a deployment may supply its own chain id from configuration. The one
//! thing this suite does on purpose is kill a process mid-broadcast, so the
//! target is checked rather than inferred.

use recovery_conformance::environment::{assert_targets_staging, STAGING_CHAIN_ID, ZCASH_NETWORK};
use zcash_voting::Network;

#[test]
fn the_suite_is_anchored_to_zcash_testnet() {
    // Staging PIR indexes testnet: its published snapshot height is above the
    // Zcash mainnet tip, so it cannot be a mainnet height. A mainnet network
    // here would scan a chain the voter wallet holds no notes on, which reads
    // as "no eligible notes" rather than as a misconfiguration.
    assert_eq!(ZCASH_NETWORK, Network::Testnet);
}

#[test]
fn the_staging_chain_id_matches_the_network_convention() {
    assert_eq!(ZCASH_NETWORK.default_vote_chain_id(), STAGING_CHAIN_ID);
    assert_eq!(STAGING_CHAIN_ID, "svote-1");
}

#[test]
fn staging_is_accepted() {
    assert_targets_staging(STAGING_CHAIN_ID);
}

#[test]
#[should_panic(expected = "production vote chain")]
fn production_is_refused() {
    assert_targets_staging("zvote-1");
}

#[test]
#[should_panic(expected = "only runs against the staging vote chain")]
fn an_unknown_chain_is_refused() {
    assert_targets_staging("some-other-chain");
}
