//! Behavioral helpers for `crate::wire` DTOs.
//!
//! This module owns conversion/serialization logic (`TryFrom`, `From`,
//! `to_json`, and payload shaping) that depends on internal crate types such as
//! `VotingError`, recovery records, and share payload models.
//!
//! It is kept separate from `wire.rs` so the FRB-scanned `wire` module can stay
//! struct-only and expose a clean, stable cross-language schema.

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};

use crate::{
    delegate::DelegationSubmission,
    phases::WorkflowPhase,
    recovery,
    session,
    share_policy::ShareSubmissionPlan,
    types::{SharePayload, VotingError, WireEncryptedShare},
    vote::SignedVoteCommitment,
    wire::{
        CommitmentBundleRecoveryView, DelegationRecoveryView, DelegationSubmissionWire,
        NextStepView, RoundPlanView, RoundRecoveryStateView, ShareDelegationRecordView,
        ShareSubmissionPlanView, ShareWorkflowRecoveryView, VoteCommitmentWire, VoteRecoveryView,
        VoteShareWire, WireEncryptedShareJson,
    },
};

const MAX_SAFE_JSON_INTEGER: u64 = 0x1f_ffff_ffff_ffff;

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

impl TryFrom<ShareSubmissionPlan> for ShareSubmissionPlanView {
    type Error = VotingError;

    fn try_from(plan: ShareSubmissionPlan) -> Result<Self, Self::Error> {
        let target_count = u32::try_from(plan.target_count).map_err(|_| VotingError::InvalidInput {
            message: format!("target_count {} does not fit u32", plan.target_count),
        })?;
        Ok(Self {
            submit_at: plan.submit_at,
            target_count,
            target_servers: plan.target_servers,
        })
    }
}

impl From<recovery::DelegationRecovery> for DelegationRecoveryView {
    fn from(record: recovery::DelegationRecovery) -> Self {
        Self {
            bundle_index: record.bundle_index,
            phase: record.workflow_phase().as_str().to_string(),
            tx_hash: record.tx_hash,
            van_leaf_position: record.van_leaf_position,
        }
    }
}

impl From<recovery::VoteRecovery> for VoteRecoveryView {
    fn from(record: recovery::VoteRecovery) -> Self {
        Self {
            bundle_index: record.bundle_index,
            proposal_id: record.proposal_id,
            choice: record.choice,
            phase: record.workflow_phase().as_str().to_string(),
            tx_hash: record.tx_hash,
            vc_tree_position: record.vc_tree_position,
            has_commitment_bundle: record.has_commitment_bundle,
        }
    }
}

impl From<recovery::RecoverableCommitmentBundle> for CommitmentBundleRecoveryView {
    fn from(record: recovery::RecoverableCommitmentBundle) -> Self {
        Self {
            bundle_index: record.bundle_index,
            proposal_id: record.proposal_id,
            commitment_bundle_json: record.commitment_bundle_json,
            vc_tree_position: record.vc_tree_position,
        }
    }
}

impl From<crate::types::ShareDelegationRecord> for ShareDelegationRecordView {
    fn from(record: crate::types::ShareDelegationRecord) -> Self {
        Self {
            round_id: record.round_id,
            bundle_index: record.bundle_index,
            proposal_id: record.proposal_id,
            share_index: record.share_index,
            sent_to_urls: record.sent_to_urls,
            nullifier: record.nullifier,
            phase: if record.confirmed {
                WorkflowPhase::Confirmed.as_str().to_string()
            } else {
                WorkflowPhase::SubmittedShare.as_str().to_string()
            },
            confirmed: record.confirmed,
            submit_at: record.submit_at,
            created_at: record.created_at,
        }
    }
}

impl From<recovery::ShareWorkflow> for ShareWorkflowRecoveryView {
    fn from(record: recovery::ShareWorkflow) -> Self {
        Self {
            bundle_index: record.bundle_index,
            proposal_id: record.proposal_id,
            share_index: record.share_index,
            phase: record.workflow_phase().as_str().to_string(),
        }
    }
}

impl From<recovery::RoundRecoverySnapshot> for RoundRecoveryStateView {
    fn from(state: recovery::RoundRecoverySnapshot) -> Self {
        Self {
            round_id: state.round_id,
            bundle_count: state.bundle_count,
            delegation: state.delegation.into_iter().map(Into::into).collect(),
            votes: state.votes.into_iter().map(Into::into).collect(),
            commitment_bundles: state.commitment_bundles.into_iter().map(Into::into).collect(),
            shares: state.shares.into_iter().map(Into::into).collect(),
            share_delegations: state.share_delegations.into_iter().map(Into::into).collect(),
            unconfirmed_share_delegations: state
                .unconfirmed_share_delegations
                .into_iter()
                .map(Into::into)
                .collect(),
        }
    }
}

impl TryFrom<session::NextStep> for NextStepView {
    type Error = VotingError;

    fn try_from(step: session::NextStep) -> Result<Self, Self::Error> {
        let kind = step.kind().to_string();
        match step {
            session::NextStep::Delegate { bundle_index }
            | session::NextStep::PollDelegation { bundle_index } => Ok(Self {
                kind,
                bundle_index,
                proposal_id: 0,
                choice: 0,
                share_index: 0,
            }),
            session::NextStep::CastVote {
                bundle_index,
                proposal_id,
                choice,
            } => Ok(Self {
                kind,
                bundle_index,
                proposal_id,
                choice,
                share_index: 0,
            }),
            session::NextStep::SubmitVote {
                bundle_index,
                proposal_id,
            }
            | session::NextStep::PollVote {
                bundle_index,
                proposal_id,
            } => Ok(Self {
                kind,
                bundle_index,
                proposal_id,
                choice: 0,
                share_index: 0,
            }),
            session::NextStep::SubmitShares {
                bundle_index,
                proposal_id,
                share_index,
            }
            | session::NextStep::ConfirmShare {
                bundle_index,
                proposal_id,
                share_index,
            } => Ok(Self {
                kind,
                bundle_index,
                proposal_id,
                choice: 0,
                share_index,
            }),
        }
    }
}

impl TryFrom<session::RoundPlan> for RoundPlanView {
    type Error = VotingError;

    fn try_from(plan: session::RoundPlan) -> Result<Self, Self::Error> {
        Ok(Self {
            round_id: plan.round_id,
            pending_recovery: plan.pending_recovery,
            next_steps: plan
                .next_steps
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
            open_proposals: plan.open_proposals,
            all_decided: plan.all_decided,
        })
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

    #[test]
    fn round_plan_view_maps_all_supported_next_steps() {
        let plan = session::RoundPlan {
            round_id: "round-1".to_string(),
            pending_recovery: true,
            next_steps: vec![
                session::NextStep::Delegate { bundle_index: 1 },
                session::NextStep::PollDelegation { bundle_index: 2 },
                session::NextStep::CastVote {
                    bundle_index: 3,
                    proposal_id: 11,
                    choice: 1,
                },
                session::NextStep::SubmitVote {
                    bundle_index: 4,
                    proposal_id: 12,
                },
                session::NextStep::PollVote {
                    bundle_index: 5,
                    proposal_id: 13,
                },
                session::NextStep::SubmitShares {
                    bundle_index: 6,
                    proposal_id: 14,
                    share_index: 0,
                },
                session::NextStep::ConfirmShare {
                    bundle_index: 7,
                    proposal_id: 15,
                    share_index: 1,
                },
            ],
            open_proposals: vec![11, 12],
            all_decided: false,
        };

        let view = RoundPlanView::try_from(plan).unwrap();
        assert_eq!(view.round_id, "round-1");
        assert!(view.pending_recovery);
        assert_eq!(view.open_proposals, vec![11, 12]);
        assert!(!view.all_decided);

        let kinds = view
            .next_steps
            .iter()
            .map(|step| step.kind.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                "delegate",
                "poll_delegation",
                "cast_vote",
                "submit_vote",
                "poll_vote",
                "submit_shares",
                "confirm_share"
            ]
        );
        assert_eq!(view.next_steps[0].bundle_index, 1);
        assert_eq!(view.next_steps[2].proposal_id, 11);
        assert_eq!(view.next_steps[2].choice, 1);
        assert_eq!(view.next_steps[6].share_index, 1);
    }
}
