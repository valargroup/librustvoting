//! Stable wire-format DTOs for vote-chain and helper endpoints.
//!
//! This module is intentionally **struct-only** and is the canonical owner of
//! protocol field names so wallet integrations do not duplicate payload-shaping
//! logic.
//!
//! FRB scans `zcash_voting::wire` directly from `vizor-wallet` to generate
//! Dart bindings. Keeping only plain DTO structs in this module prevents FRB
//! from traversing behavior-level APIs that depend on internal crate types.
//!
//! All conversions, validation, and serialization helpers live in
//! `crate::wire_codec`, while `wire.rs` remains the stable cross-language
//! schema surface.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireEncryptedShareJson {
    pub c1: String,
    pub c2: String,
    pub share_index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationSubmissionWire {
    pub rk: String,
    pub spend_auth_sig: String,
    pub sighash: String,
    #[serde(rename = "signed_note_nullifier")]
    pub nf_signed: String,
    pub cmx_new: String,
    #[serde(rename = "van_cmx")]
    pub gov_comm: String,
    pub gov_nullifiers: Vec<String>,
    pub proof: String,
    pub vote_round_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoteCommitmentWire {
    pub van_nullifier: String,
    pub vote_authority_note_new: String,
    pub vote_commitment: String,
    pub proposal_id: u32,
    pub proof: String,
    pub vote_round_id: String,
    #[serde(rename = "vote_comm_tree_anchor_height")]
    pub anchor_height: u32,
    pub r_vpk: String,
    pub vote_auth_sig: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoteShareWire {
    pub shares_hash: String,
    pub proposal_id: u32,
    pub vote_decision: u32,
    #[serde(rename = "enc_share")]
    pub encrypted_share: WireEncryptedShareJson,
    pub share_index: u32,
    #[serde(rename = "tree_position")]
    pub vc_tree_position: u64,
    #[serde(rename = "all_enc_shares")]
    pub all_encrypted_shares: Vec<WireEncryptedShareJson>,
    pub share_comms: Vec<String>,
    pub primary_blind: String,
    pub submit_at: u64,
}

/// Parameters for a voting round, sourced from vote chain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VotingRoundParams {
    pub vote_round_id: String,
    pub snapshot_height: u64,
    pub ea_pk: Vec<u8>,
    pub nc_root: Vec<u8>,
    pub nullifier_imt_root: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareSubmissionPlanView {
    pub submit_at: u64,
    pub target_count: u32,
    pub target_servers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationRecoveryView {
    pub bundle_index: u32,
    pub phase: String,
    pub tx_hash: Option<String>,
    pub van_leaf_position: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoteRecoveryView {
    pub bundle_index: u32,
    pub proposal_id: u32,
    pub choice: u32,
    pub phase: String,
    pub tx_hash: Option<String>,
    pub vc_tree_position: Option<u64>,
    pub has_commitment_bundle: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitmentBundleRecoveryView {
    pub bundle_index: u32,
    pub proposal_id: u32,
    pub commitment_bundle_json: String,
    pub vc_tree_position: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareDelegationRecordView {
    pub round_id: String,
    pub bundle_index: u32,
    pub proposal_id: u32,
    pub share_index: u32,
    pub sent_to_urls: Vec<String>,
    pub nullifier: Vec<u8>,
    pub phase: String,
    pub confirmed: bool,
    pub submit_at: u64,
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareWorkflowRecoveryView {
    pub bundle_index: u32,
    pub proposal_id: u32,
    pub share_index: u32,
    pub phase: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundRecoveryStateView {
    pub round_id: String,
    pub bundle_count: u32,
    pub delegation: Vec<DelegationRecoveryView>,
    pub votes: Vec<VoteRecoveryView>,
    pub commitment_bundles: Vec<CommitmentBundleRecoveryView>,
    pub shares: Vec<ShareWorkflowRecoveryView>,
    pub share_delegations: Vec<ShareDelegationRecordView>,
    pub unconfirmed_share_delegations: Vec<ShareDelegationRecordView>,
}
