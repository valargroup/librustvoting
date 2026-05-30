//! Stable wire-format JSON serializers for vote-chain and helper endpoints.
//!
//! This module is the canonical owner of protocol field names and byte encoding
//! rules so wallet integrations do not duplicate base64/hex shaping logic.

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde::{Deserialize, Serialize};

use crate::{
    delegate::DelegationSubmission,
    types::{SharePayload, VotingError, WireEncryptedShare},
    vote::SignedVoteCommitment,
};

const MAX_SAFE_JSON_INTEGER: u64 = 0x1f_ffff_ffff_ffff;

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

impl DelegationSubmissionWire {
    pub fn to_json(&self) -> Result<String, VotingError> {
        serde_json::to_string(self).map_err(|e| VotingError::Internal {
            message: format!("serialize delegation wire JSON failed: {e}"),
        })
    }
}

impl VoteCommitmentWire {
    pub fn to_json(&self) -> Result<String, VotingError> {
        serde_json::to_string(self).map_err(|e| VotingError::Internal {
            message: format!("serialize vote commitment wire JSON failed: {e}"),
        })
    }
}

impl VoteShareWire {
    pub fn from_payload(
        payload: &SharePayload,
        vc_tree_position: Option<u64>,
        submit_at: u64,
    ) -> Result<Self, VotingError> {
        Ok(Self {
            shares_hash: b64(&payload.shares_hash),
            proposal_id: payload.proposal_id,
            vote_decision: payload.vote_decision,
            encrypted_share: (&payload.enc_share).into(),
            share_index: payload.enc_share.share_index,
            vc_tree_position: json_safe_u64(
                vc_tree_position.unwrap_or(payload.tree_position),
                "tree_position",
            )?,
            all_encrypted_shares: payload.all_enc_shares.iter().map(Into::into).collect(),
            share_comms: payload.share_comms.iter().map(b64).collect(),
            primary_blind: b64(&payload.primary_blind),
            submit_at: json_safe_u64(submit_at, "submit_at")?,
        })
    }

    pub fn to_json(&self) -> Result<String, VotingError> {
        serde_json::to_string(self).map_err(|e| VotingError::Internal {
            message: format!("serialize vote share wire JSON failed: {e}"),
        })
    }

    pub fn with_late_bound(
        mut self,
        vc_tree_position: Option<u64>,
        submit_at: u64,
    ) -> Result<Self, VotingError> {
        if let Some(position) = vc_tree_position {
            self.vc_tree_position = json_safe_u64(position, "tree_position")?;
        }
        self.submit_at = json_safe_u64(submit_at, "submit_at")?;
        Ok(self)
    }
}

impl From<&WireEncryptedShare> for WireEncryptedShareJson {
    fn from(share: &WireEncryptedShare) -> Self {
        Self {
            c1: b64(&share.c1),
            c2: b64(&share.c2),
            share_index: share.share_index,
        }
    }
}

impl TryFrom<&DelegationSubmission> for DelegationSubmissionWire {
    type Error = VotingError;

    fn try_from(submission: &DelegationSubmission) -> Result<Self, Self::Error> {
        Ok(Self {
            rk: b64(submission.rk),
            spend_auth_sig: b64(submission.spend_auth_sig),
            sighash: b64(submission.sighash),
            nf_signed: b64(submission.nf_signed),
            cmx_new: b64(submission.cmx_new),
            gov_comm: b64(submission.gov_comm),
            gov_nullifiers: submission.gov_nullifiers.iter().map(b64).collect(),
            proof: b64(&submission.proof),
            vote_round_id: b64_hex(&submission.vote_round_id, "vote_round_id")?,
        })
    }
}

impl TryFrom<&SignedVoteCommitment> for VoteCommitmentWire {
    type Error = VotingError;

    fn try_from(commitment: &SignedVoteCommitment) -> Result<Self, Self::Error> {
        Ok(Self {
            van_nullifier: b64(commitment.van_nullifier),
            vote_authority_note_new: b64(commitment.vote_authority_note_new),
            vote_commitment: b64(commitment.vote_commitment),
            proposal_id: commitment.proposal_id,
            proof: b64(&commitment.proof),
            vote_round_id: b64_hex(&commitment.vote_round_id, "vote_round_id")?,
            anchor_height: commitment.anchor_height,
            r_vpk: b64(commitment.r_vpk),
            vote_auth_sig: b64(commitment.vote_auth_sig),
        })
    }
}

impl DelegationSubmission {
    pub fn to_wire_json(&self) -> Result<String, VotingError> {
        DelegationSubmissionWire::try_from(self)?.to_json()
    }
}

impl SignedVoteCommitment {
    pub fn to_wire_json(&self) -> Result<String, VotingError> {
        VoteCommitmentWire::try_from(self)?.to_json()
    }
}

impl SharePayload {
    pub fn to_wire_json(
        &self,
        vc_tree_position: Option<u64>,
        submit_at: u64,
    ) -> Result<String, VotingError> {
        VoteShareWire::from_payload(self, vc_tree_position, submit_at)?.to_json()
    }
}

fn b64(bytes: impl AsRef<[u8]>) -> String {
    BASE64_STANDARD.encode(bytes.as_ref())
}

fn b64_hex(hex_value: &str, field: &str) -> Result<String, VotingError> {
    let normalized = hex_value.strip_prefix("0x").unwrap_or(hex_value);
    let bytes = hex::decode(normalized).map_err(|e| VotingError::InvalidInput {
        message: format!("{field} is not valid hex: {e}"),
    })?;
    Ok(b64(bytes))
}

fn json_safe_u64(value: u64, field: &str) -> Result<u64, VotingError> {
    if value > MAX_SAFE_JSON_INTEGER {
        return Err(VotingError::InvalidInput {
            message: format!("field {field} is too large to encode as JSON integer"),
        });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vote::SignedVoteCommitment;

    fn decode_b64(value: &str) -> Vec<u8> {
        BASE64_STANDARD.decode(value).unwrap()
    }

    #[test]
    fn delegation_submission_wire_json_shape() {
        let submission = DelegationSubmission {
            proof: vec![0xAA; 8],
            rk: [0x01; 32],
            nf_signed: [0x02; 32],
            cmx_new: [0x03; 32],
            gov_comm: [0x04; 32],
            gov_nullifiers: [[0x05; 32]; crate::BUNDLE_NOTE_SLOTS],
            alpha: [0; 32],
            vote_round_id: "0a0b".to_string(),
            spend_auth_sig: [0x06; 64],
            sighash: [0x07; 32],
        };

        let json = submission.to_wire_json().unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value.get("signed_note_nullifier").is_some());
        assert!(value.get("van_cmx").is_some());
        assert_eq!(
            decode_b64(value.get("vote_round_id").unwrap().as_str().unwrap()),
            vec![0x0a, 0x0b]
        );
    }

    #[test]
    fn vote_commitment_wire_json_shape() {
        let commitment = SignedVoteCommitment {
            proposal_id: 7,
            choice: 1,
            vote_round_id: "0c0d".to_string(),
            van_nullifier: [0x11; 32],
            vote_authority_note_new: [0x12; 32],
            vote_commitment: [0x13; 32],
            proof: vec![0x14; 8],
            encrypted_shares: vec![],
            share_payloads: vec![],
            anchor_height: 123,
            shares_hash: [0x15; 32],
            share_comms: vec![],
            r_vpk: [0x16; 32],
            vote_auth_sig: [0x17; 64],
            commitment_bundle_json: "{}".to_string(),
        };

        let json = commitment.to_wire_json().unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value
                .get("vote_comm_tree_anchor_height")
                .unwrap()
                .as_u64()
                .unwrap(),
            123
        );
        assert_eq!(value.get("proposal_id").unwrap().as_u64().unwrap(), 7);
    }

    #[test]
    fn vote_share_wire_json_shape() {
        let payload = SharePayload {
            shares_hash: vec![0x21; 32],
            proposal_id: 9,
            vote_decision: 2,
            enc_share: WireEncryptedShare {
                c1: vec![0x22; 32],
                c2: vec![0x23; 32],
                share_index: 1,
            },
            tree_position: 99,
            all_enc_shares: vec![WireEncryptedShare {
                c1: vec![0x24; 32],
                c2: vec![0x25; 32],
                share_index: 1,
            }],
            share_comms: vec![vec![0x26; 32]],
            primary_blind: vec![0x27; 32],
        };

        let json = payload.to_wire_json(None, 123).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value.get("tree_position").unwrap().as_u64().unwrap(), 99);
        assert_eq!(value.get("submit_at").unwrap().as_u64().unwrap(), 123);
        assert!(value.get("enc_share").is_some());
        assert!(value.get("all_enc_shares").is_some());
    }

    #[test]
    fn vote_share_wire_json_rejects_large_json_integer() {
        let payload = SharePayload {
            shares_hash: vec![0x21; 32],
            proposal_id: 1,
            vote_decision: 1,
            enc_share: WireEncryptedShare {
                c1: vec![0x22; 32],
                c2: vec![0x23; 32],
                share_index: 0,
            },
            tree_position: MAX_SAFE_JSON_INTEGER + 1,
            all_enc_shares: vec![],
            share_comms: vec![],
            primary_blind: vec![0x27; 32],
        };

        let err = payload.to_wire_json(None, 10).unwrap_err();
        assert!(err
            .to_string()
            .contains("field tree_position is too large to encode as JSON integer"));
    }
}
