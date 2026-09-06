//! Building the round this suite crashes its way through.
//!
//! A round is created on the vote chain by `MsgCreateVotingSession`, which the
//! chain gates behind coordinator approval. The `svoted` CLI takes that message
//! as a JSON description file, so this package's job is to produce a
//! description that is both *valid* for the chain and *shaped* for the
//! invariants: several proposals, differing option counts, and a bundle layout
//! with a bundle to spare.
//!
//! # Why the round must be recreated, and how often
//!
//! A delegation is consumed on the vote chain per round. The Zcash notes are
//! untouched — TX1 is a PCZT-only signing artifact and is never broadcast — so
//! the voter wallet never needs re-funding. It is the round that is one-shot.
//!
//! That splits the suite in two. A stage that gets a POST onto the wire, and
//! any test that drives a resumed round to quiescence, consumes its round and
//! needs a fresh one. Everything else leaves the chain untouched and can branch
//! from one provisioned round by copying the sidecar file.
//!
//! Round creation also serializes chain-wide: the chain refuses a second
//! `CreateVotingSession` while another ceremony is pending, so the mutative
//! tier cannot provision in parallel.

mod ballot;
mod keyring;

pub use ballot::{
    suite_ballot, RoundDescription, RoundOption, RoundProposal, SnapshotAnchor,
    EXPECTED_BUNDLE_COUNT, EXPECTED_VOTER_NOTES,
};
pub use keyring::VoteManagerKeyring;

/// The coordinator `VOTE_MANAGER_VOTE_SDK` controls, registered on staging.
///
/// The config attestation key is a different account and is deliberately not
/// modelled here: this suite signs its own round entries rather than relying
/// on attestations.
///
/// Checking a derived address against this is the cheapest guard there is: one
/// local computation, no chain interaction, and it fails before anything is
/// broadcast rather than as a rejected transaction. It also pins the coin type
/// — derive with the cosmos default of 118 and the mismatch shows up here
/// rather than as an authorization failure that looks like a permissions
/// problem.
pub const COORDINATOR_ADDRESS: &str = "sv1z4rawnk8ny0pzsewyzm3egdd7296fr8p20fkf8";
