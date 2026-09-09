use base64::{engine::general_purpose::STANDARD, Engine};

use crate::{
    vote_commitment::CastVoteBatchSighashAction, wire::DelegateAndVoteBatchWire, VotingError,
};

impl DelegateAndVoteBatchWire {
    /// Serializes the combined endpoint's envelope after validating its context.
    /// Proof and signature verification remains the durable lifecycle's job.
    pub fn to_json(&self) -> Result<String, VotingError> {
        self.authorization_digest()?;
        serde_json::to_string(self)
            .map_err(|error| invalid(format!("serialize combined request: {error}")))
    }

    /// Computes the authorization digest after checking shared round, synthetic
    /// anchors, canonical effect encodings and ordered proposal membership.
    pub fn authorization_digest(&self) -> Result<[u8; 32], VotingError> {
        let round = decode32(&self.delegation.vote_round_id)?;
        crate::types::validate_vote_round_id_bytes(&round)?;
        let initial_van = decode32(&self.delegation.gov_comm)?;
        crate::types::validate_vote_round_id_bytes(&initial_van)?;
        let mut effects = Vec::with_capacity(self.batch.votes.len());
        for vote in &self.batch.votes {
            if vote.anchor_height != 0 || decode32(&vote.vote_round_id)? != round {
                return Err(invalid(
                    "combined casts must share the delegation round and use anchor zero",
                ));
            }
            effects.push([
                decode32(&vote.r_vpk)?,
                decode32(&vote.van_nullifier)?,
                decode32(&vote.vote_authority_note_new)?,
                decode32(&vote.vote_commitment)?,
            ]);
        }
        let actions = effects
            .iter()
            .zip(&self.batch.votes)
            .map(|(effect, vote)| CastVoteBatchSighashAction {
                r_vpk: &effect[0],
                van_nullifier: &effect[1],
                vote_authority_note_new: &effect[2],
                vote_commitment: &effect[3],
                proposal_id: vote.proposal_id,
            })
            .collect::<Vec<_>>();
        super::delegate_and_vote_batch_sighash(&round, &initial_van, &actions)
    }
}

fn decode32(encoded: &str) -> Result<[u8; 32], VotingError> {
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| invalid("combined request contains invalid base64"))?;
    if STANDARD.encode(&bytes) != encoded {
        return Err(invalid("combined request requires canonical base64"));
    }
    bytes
        .try_into()
        .map_err(|_| invalid("combined request effect must contain 32 bytes"))
}

fn invalid(message: impl Into<String>) -> VotingError {
    VotingError::InvalidInput {
        message: message.into(),
    }
}
