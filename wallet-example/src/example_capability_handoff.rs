//! Role-separated delegation capability handoff.
//!
//! This module connects the lower-level wallet examples into the sequence an
//! integrator implements across the voter and funds controller applications:
//!
//! 1. The voter calls [`voter_create_target`] and sends the resulting JSON.
//! 2. The funds controller calls [`controller_validate_target`], then passes
//!    that target through
//!    [`crate::example_delegation::prepare_delegation_bundle_for_public_target`],
//!    [`crate::example_delegation::precompute_delegation_bundle`], and its
//!    selected software or external-signer helper in
//!    [`crate::example_delegation`]. Its chain adapter turns each resulting
//!    submission into the exact signed transaction bytes it will broadcast.
//! 3. The funds controller calls [`controller_export_capability`] with those
//!    bytes. It durably stores `canonical_json()` and `digest()`, then delivers
//!    the exact JSON. Broadcasting the signed delegation transactions may
//!    happen concurrently.
//! 4. The voter calls [`voter_import_capability`] and returns the resulting
//!    digest as a delivery acknowledgement. The funds controller compares it
//!    with the stored digest and redelivers the same bytes after a mismatch or
//!    missing acknowledgement.
//! 5. After broadcast, the voter drives [`voter_advance_delegation`] for every
//!    bundle until it confirms. The SDK polls the transaction and records the
//!    public VAN position; the voter supplies no hash and no chain events.
//!    Imported capability rounds cannot create votes until every bundle has a
//!    public VAN position.
//! 6. The voter calls [`voter_build_signed_vote`], drives the cast-vote
//!    transaction with [`crate::example_vote::advance_committed_vote`], asks
//!    the SDK to prepare the complete helper-share plan, then submits the
//!    prepared plan through [`crate::example_vote`]. The SDK records the
//!    confirmed tree position itself.
//!
//! Authenticated transport, durable controller outbox storage, helper transport
//! routing, cancellation, and scheduling remain owned by the integrating
//! applications. Chain submission, event interpretation, and confirmation are
//! owned by the SDK lifecycle. Helper plan
//! persistence is SDK-owned.

use anyhow::{Context, Result};
use std::sync::Arc;

use zcash_voting::prelude::{
    AdvanceImportedDelegation, ChainSubmissionClient, ChainSubmissionClientConfig,
    ChainSubmissionControl, ChainSubmissionResult, CommittedVote, DelegationCapabilityDigest,
    DraftVote, ExportedDelegationCapability, Network, RoundBoundVotingHotkeyTarget, VotingDb,
    VotingHotkey,
};
use zcash_voting::VotingRoundParams;

use crate::example_delegation::{
    encode_public_voting_target, export_delegation_capability_package,
    import_delegation_capability_package, validate_public_voting_target,
};
use crate::example_vote::{
    commit_vote_bundle, derive_vote_van_witness, WalletVanWitnessRequest, WalletVoteCommitRequest,
};

/// Voter step: creates the public hotkey target sent to the funds controller.
pub fn voter_create_target(
    voting_hotkey: &VotingHotkey,
    vote_chain_id: &str,
    round_params: &VotingRoundParams,
) -> Result<Vec<u8>> {
    encode_public_voting_target(voting_hotkey, vote_chain_id, round_params)
        .context("create voter public target")
}

/// Funds controller step: validates received target bytes against local context.
pub fn controller_validate_target(
    target_json: &[u8],
    expected_chain_id: &str,
    expected_network: Network,
    expected_round_params: &VotingRoundParams,
) -> Result<RoundBoundVotingHotkeyTarget> {
    validate_public_voting_target(
        target_json,
        expected_chain_id,
        expected_network,
        expected_round_params,
    )
    .context("validate voter public target")
}

/// Funds controller step: binds canonical package bytes to their acknowledgement digest.
pub fn controller_export_capability(
    voting_db: &VotingDb,
    voting_target: &RoundBoundVotingHotkeyTarget,
    signed_delegation_txs: &[Vec<u8>],
) -> Result<ExportedDelegationCapability> {
    export_delegation_capability_package(voting_db, voting_target, signed_delegation_txs)
        .context("export controller delegation capability")
}

/// Voter step: validates and durably imports the exact delivered package bytes.
pub fn voter_import_capability(
    voting_db: &VotingDb,
    capability_json: &[u8],
    voting_hotkey: &VotingHotkey,
    expected_chain_id: &str,
    expected_network: Network,
    expected_round_params: &VotingRoundParams,
    session_json: Option<&str>,
) -> Result<DelegationCapabilityDigest> {
    import_delegation_capability_package(
        voting_db,
        capability_json,
        voting_hotkey,
        expected_chain_id,
        expected_network,
        expected_round_params,
        session_json,
    )
    .context("import voter delegation capability")
}

/// Voter step: advances the imported bundle's delegation submission one pass.
///
/// The SDK adopts the package's already-broadcast transaction hash, polls it,
/// and writes the confirmed VAN position atomically. It never submits this
/// transaction. A voter never supplies signing material, parses chain events,
/// or records a transaction hash. Re-invoke while the result is
/// [`ChainSubmissionResult::Pending`]; the confirmed VAN position is available
/// from the returned confirmation.
pub async fn voter_advance_delegation(
    voting_db: Arc<VotingDb>,
    config: ChainSubmissionClientConfig,
    vote_round_id: [u8; 32],
    bundle_index: u32,
    control: &ChainSubmissionControl,
) -> Result<ChainSubmissionResult> {
    let client = ChainSubmissionClient::new(voting_db, config)
        .map_err(|failure| anyhow::anyhow!("build chain submission client: {failure}"))?;
    client
        .advance_imported_delegation(
            AdvanceImportedDelegation {
                vote_round_id,
                bundle_index,
            },
            control,
        )
        .await
        .map_err(|failure| anyhow::anyhow!("advance delegation chain submission: {failure}"))
}

/// Voter step: syncs the confirmed VAN, builds ZKP2, and signs one vote.
///
/// Drive the cast-vote transaction with
/// [`crate::example_vote::advance_committed_vote`] until it confirms; the SDK
/// records the transaction hash and confirmed tree position itself. Ask the SDK
/// to
/// create and persist one complete helper plan with
/// [`crate::example_vote::prepare_committed_vote_shares`] before calling
/// [`crate::example_vote::submit_committed_vote_shares`]. After restart,
/// recover the vote and call the same preparation API to load the exact plan;
/// pass the complete current helper fleet to submission.
pub fn voter_build_signed_vote(
    voting_db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    vote_node_url: &str,
    draft: &DraftVote,
    voting_hotkey: &VotingHotkey,
) -> Result<CommittedVote> {
    let van_witness = derive_vote_van_witness(
        voting_db,
        WalletVanWitnessRequest {
            round_id,
            bundle_index,
            vote_node_url,
        },
    )
    .context("derive confirmed VAN witness")?;

    commit_vote_bundle(
        voting_db,
        WalletVoteCommitRequest {
            round_id,
            bundle_index,
            draft,
            van_witness: &van_witness,
            voting_hotkey,
        },
    )
    .context("build signed voter commitment")
}
