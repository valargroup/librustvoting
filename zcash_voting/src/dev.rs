//! Destructive operations, quarantined behind an opt-in feature.
//!
//! # Why this module exists
//!
//! [`VotingDb::clear_round`] deletes a round and cascades `bundles` away with
//! it. `bundles` holds `van_comm_rand`: 32 bytes sampled from `OsRng`, derived
//! from nothing, with no import path exposed over the FFI, and whose VAN
//! commitment is already published on chain. Once it is gone the round is
//! permanently unvotable. `alpha`, `rseed_signed`, `rseed_output` and
//! `padded_note_secrets` are sampled the same way, and the cascade also takes
//! Keystone signatures that the user's hardware wallet may not be able to
//! produce again.
//!
//! None of that is visible from the name `clear_round`, which reads like
//! dropping a cache entry. This module is where an operation of that kind
//! belongs: reachable, but only when the caller has said so twice -- once by
//! enabling `dev-destructive-apis`, once by writing `dev::` at the call site.
//!
//! # What this module is not
//!
//! **A security boundary.** Cargo features are additive: any crate anywhere in
//! the dependency graph that enables `dev-destructive-apis` enables it for the
//! whole build, and nothing stops a caller from reaching
//! [`VotingDb::clear_round`] directly. The crate already states this about
//! `test-fixtures` (see [`crate::vote::insert_recovery_fixture`]) and the same
//! applies here. What a feature gate buys is that a production build has to
//! *opt in* to compiling the operation at all, and that the opt-in is a
//! reviewable line in a manifest rather than a call buried in a coordinator.
//!
//! It also does nothing for FFI callers. `zcashlc_voting_clear_round` is
//! compiled by the SDK crate, and Swift sees neither this module nor the
//! `#[deprecated]` markers. Crossing that boundary needs a runtime signal --
//! today a log line, and better, a confirmation argument the caller has to pass.
//!
//! # The shape this is heading towards
//!
//! The reason a production flow calls `clear_round` at all is that it wants
//! "start this round over", and a delete is the only thing on offer. That is an
//! *idempotent re-initialisation*: reset the derivable columns, leave the
//! sampled ones alone. Given that, `clear_round` loses its last production
//! caller and can be gated here unconditionally rather than duplicated.
//!
//! Until then this module is additive and nothing is removed.

#![cfg(feature = "dev-destructive-apis")]

use crate::storage::VotingDb;
use crate::types::VotingError;

/// Destroys a round and every secret it holds, with no way to get them back.
///
/// The long name is the point. This is [`VotingDb::clear_round`] under a name
/// that cannot be called by accident by someone skimming a diff, and that a
/// reviewer cannot read past. Prefer renaming the operation over documenting
/// the danger: documentation is not present at the call site, and the call site
/// is where the mistake gets made.
///
/// # This is irreversible
///
/// `van_comm_rand`, `alpha`, `rseed_signed`, `rseed_output` and
/// `padded_note_secrets` are RNG-sampled and are not derived from the wallet
/// seed. Keystone signatures come from a device the user may no longer have.
/// None of it can be restored, by this crate or any other, and a round whose
/// VAN commitment is on chain becomes permanently unvotable.
///
/// # When it is legitimate
///
/// Tests, fixtures, local development against a throwaway database, and
/// tooling that operates on a database the user has explicitly asked to reset.
/// Not a recovery path, and not a response to a read that came back empty --
/// an empty read and a failed read are indistinguishable at this layer, which
/// is exactly how a round gets destroyed by one dropped packet.
pub fn destroy_round_and_its_unrecoverable_secrets(
    db: &VotingDb,
    round_id: &str,
) -> Result<(), VotingError> {
    #[allow(deprecated)]
    db.clear_round(round_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::queries;

    /// The quarantined entry point does the same thing as the deprecated one --
    /// the gate is about who can reach it, not about changing behaviour.
    #[test]
    fn the_quarantined_entry_point_still_deletes_the_round() {
        let db = VotingDb::open(":memory:").unwrap();
        db.set_wallet_id("test-wallet");
        let params = crate::VotingRoundParams {
            vote_round_id: "test-round-1".to_string(),
            snapshot_height: 1000,
            ea_pk: vec![0xEA; 32],
            nc_root: vec![0xAA; 32],
            nullifier_imt_root: vec![0xBB; 32],
        };
        queries::insert_round(
            &db.conn(),
            "test-wallet",
            crate::Network::Testnet,
            &params,
            None,
        )
        .unwrap();

        destroy_round_and_its_unrecoverable_secrets(&db, "test-round-1").unwrap();

        assert!(queries::list_rounds(&db.conn(), "test-wallet")
            .unwrap()
            .is_empty());
    }
}
