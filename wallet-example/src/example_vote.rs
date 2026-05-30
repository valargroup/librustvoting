use anyhow::{Context, Result};
use zcash_voting::prelude::{sync_vote_tree, van_witness, VanWitness, VotingDb};

/// Human-readable VAN-witness stages printed by embedders of this example.
pub const VAN_WITNESS_FLOW: &[&str] = &[
    "Sync the on-chain vote-authority-note tree for the round from the vote node.",
    "Generate the VAN Merkle witness for the confirmed delegation bundle.",
];

/// Caller-owned inputs needed to derive one bundle's VAN witness.
pub struct WalletVanWitnessRequest<'a> {
    pub round_id: &'a str,
    pub bundle_index: u32,
    pub vote_node_url: &'a str,
}

/// Example wallet-side orchestration for deriving a bundle's VAN witness.
///
/// This is the first step of the cast-vote phase. It runs after the delegation
/// transaction for `bundle_index` has confirmed on the vote chain, and produces
/// the `VanWitness` that `zcash_voting::vote::commit` requires as input.
///
/// The witness is anchored at the height returned by the tree sync, so the
/// caller does not need to track an anchor height separately.
pub fn derive_vote_van_witness(
    voting_db: &VotingDb,
    request: WalletVanWitnessRequest<'_>,
) -> Result<VanWitness> {
    // 1. Pull the latest vote-authority-note tree state for the round and keep
    // the synced height as the anchor for the witness produced below.
    let anchor_height = sync_vote_tree(voting_db, request.round_id, request.vote_node_url)
        .context("sync vote tree")?;

    // 2. Derive this bundle's Merkle witness against the freshly synced tree.
    van_witness(
        voting_db,
        request.round_id,
        request.bundle_index,
        anchor_height,
    )
    .context("generate VAN witness")
}
