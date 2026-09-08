use crate::{
    chain_submission::{generation_for_delegation, ChainSubmissionIdentity, ChainSubmissionTarget},
    round::VotingDb,
    vote::{DraftVote, PreparedAtomicVoteBatch, VoteSigner},
    VotingError,
};

/// Inputs for a fresh combined transaction. No chain witness is needed.
pub struct DelegateAndVoteBatchRequest<'a> {
    pub round_id: &'a str,
    pub bundle_index: u32,
    pub drafts: &'a [DraftVote],
    /// Delegation SpendAuth signature over the SDK's persisted PCZT sighash.
    pub spend_auth_signature: [u8; 64],
    pub stages: &'a dyn crate::types::VoteCommitStageReporter,
    pub max_proof_concurrency: usize,
}

/// Builds combined cast proofs against the locally verified delegation VAN.
/// The signed result must be persisted before chain submission. Existing
/// standalone delegation evidence refuses preparation; it must be recovered.
/// Captures the active wallet once; account switches cannot retarget its reads.
/// An already persisted combined batch must be restored with
/// [`recover_delegate_and_vote_batch`] instead of prepared again.
pub fn prepare_delegate_and_vote_batch(
    db: &VotingDb,
    signer: VoteSigner<'_>,
    request: DelegateAndVoteBatchRequest<'_>,
) -> Result<PreparedAtomicVoteBatch, VotingError> {
    crate::vote::prepare_delegate_and_vote_batch(
        db,
        signer,
        request,
        &crate::ObservationScope::disabled(),
    )
}

/// Captured public authorization and the immutable setup generation it signs.
#[derive(Clone)]
pub(crate) struct DelegationAuthorization {
    pub(crate) identity: ChainSubmissionIdentity,
    pub(crate) generation_digest: [u8; 32],
    pub(crate) signature: [u8; 64],
    pub(crate) submission: crate::wire::DelegationSubmissionWire,
    pub(crate) van: [u8; 32],
}

impl DelegationAuthorization {
    /// Requires the delegation and its dependent votes to share one storage identity.
    pub(crate) fn validate_scope(
        &self,
        wallet: &str,
        round: &str,
        bundle: u32,
    ) -> Result<(), VotingError> {
        if self.identity.wallet_id() != wallet
            || hex::encode(self.identity.vote_round_id()) != round
            || self.identity.bundle_index() != bundle
        {
            return Err(invalid(
                "combined authorization does not match the vote batch storage identity",
            ));
        }
        Ok(())
    }

    /// Restores the exact public authorization without treating an on-wire
    /// generation as fresh preparation.
    pub(crate) fn recover_submission(
        conn: &rusqlite::Connection,
        wallet: &str,
        round: &str,
        bundle: u32,
        digest: &[u8; 32],
    ) -> Result<crate::wire::DelegationSubmissionWire, VotingError> {
        let (signature, expected): (Vec<u8>, Vec<u8>) = conn.query_row(
            "SELECT spend_auth_signature, delegation_generation_digest FROM delegate_cast_recovery WHERE round_id=?1 AND wallet_id=?2 AND bundle_index=?3 AND batch_digest=?4",
            rusqlite::params![round, wallet, bundle, digest.as_slice()], |row| Ok((row.get(0)?, row.get(1)?)),
        ).map_err(|error| VotingError::Storage { message: format!("recover combined authorization: {error}") })?;
        let network = crate::storage::queries::load_round_network(conn, round, wallet)?;
        let round_bytes = hex::decode(round)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| invalid("invalid combined round"))?;
        let identity = ChainSubmissionIdentity::new(
            wallet,
            network,
            round_bytes,
            bundle,
            ChainSubmissionTarget::Delegation,
        )
        .map_err(|error| invalid(error.to_string()))?;
        let generation = generation_for_delegation(conn, &identity)?;
        if generation.generation().digest().as_bytes().as_slice() != expected {
            return Err(invalid("stored combined delegation changed"));
        }
        let signature = signature
            .try_into()
            .map_err(|_| invalid("stored combined signature must contain 64 bytes"))?;
        crate::wire::DelegationSubmissionWire::try_from(&crate::delegate::submission_with_conn(
            conn, wallet, round, bundle, signature,
        )?)
    }
    pub(crate) fn capture(
        conn: &rusqlite::Connection,
        wallet: &str,
        round: &str,
        bundle: u32,
        signature: [u8; 64],
    ) -> Result<Self, VotingError> {
        let network = crate::storage::queries::load_round_network(conn, round, wallet)?;
        let round_bytes = hex::decode(round)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| invalid("invalid combined round id"))?;
        let identity = ChainSubmissionIdentity::new(
            wallet,
            network,
            round_bytes,
            bundle,
            ChainSubmissionTarget::Delegation,
        )
        .map_err(|error| invalid(error.to_string()))?;
        let generation = generation_for_delegation(conn, &identity)?;
        let submission =
            crate::delegate::submission_with_conn(conn, wallet, round, bundle, signature)?;
        let authorization = Self {
            identity,
            generation_digest: *generation.generation().digest().as_bytes(),
            signature,
            van: submission.gov_comm,
            submission: crate::wire::DelegationSubmissionWire::try_from(&submission)?,
        };
        authorization.validate_fresh(conn)?;
        Ok(authorization)
    }

    pub(crate) fn validate_fresh(&self, conn: &rusqlite::Connection) -> Result<(), VotingError> {
        let round = hex::encode(self.identity.vote_round_id());
        let occupied: bool = conn.query_row(
            "SELECT b.delegation_tx_hash IS NOT NULL OR b.van_leaf_position IS NOT NULL OR EXISTS(SELECT 1 FROM chain_submissions s WHERE s.round_id=b.round_id AND s.wallet_id=b.wallet_id AND s.bundle_index=b.bundle_index) FROM bundles b WHERE b.round_id=?1 AND b.wallet_id=?2 AND b.bundle_index=?3",
            rusqlite::params![round, self.identity.wallet_id(), self.identity.bundle_index()], |row| row.get(0),
        ).map_err(|error| VotingError::Storage { message: format!("load combined delegation eligibility: {error}") })?;
        if occupied {
            return Err(invalid(
                "combined preparation requires a delegation with no chain submission evidence",
            ));
        }
        let current = generation_for_delegation(conn, &self.identity)?;
        if current.generation().digest().as_bytes() != &self.generation_digest {
            return Err(invalid("delegation changed during combined preparation"));
        }
        Ok(())
    }

    pub(crate) fn persist(
        &self,
        conn: &rusqlite::Connection,
        digest: &[u8; 32],
    ) -> Result<(), VotingError> {
        self.validate_fresh(conn)?;
        conn.execute("INSERT INTO delegate_cast_recovery(round_id, wallet_id, bundle_index, batch_digest, delegation_generation_digest, spend_auth_signature) VALUES(?1,?2,?3,?4,?5,?6)",
            rusqlite::params![hex::encode(self.identity.vote_round_id()), self.identity.wallet_id(), self.identity.bundle_index(), digest.as_slice(), self.generation_digest.as_slice(), self.signature.as_slice()])
            .map_err(|error| VotingError::Storage { message: format!("persist combined delegation authorization: {error}") })?;
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> VotingError {
    VotingError::InvalidInput {
        message: message.into(),
    }
}

/// Persists the signed combined envelope and its delegation authorization in
/// one transaction. A standalone batch is refused before writing anything.
pub fn persist_delegate_and_vote_batch(
    db: &VotingDb,
    prepared: PreparedAtomicVoteBatch,
) -> Result<crate::vote::SignedVoteBatch, VotingError> {
    if !prepared.is_delegate_and_vote_batch() {
        return Err(invalid("expected a prepared delegation-and-cast batch"));
    }
    crate::vote::persist_prepared_atomic_vote_batch(db, prepared)
}

/// Restores a complete combined envelope without the delegation signing key.
/// Any member identifies the same ordered transaction; ordinary batches are
/// refused so callers cannot accidentally select a different endpoint.
pub fn recover_delegate_and_vote_batch(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    proposal_id: u32,
) -> Result<crate::vote::SignedVoteBatch, VotingError> {
    let batch = crate::vote::recover_atomic_vote_batch(db, round_id, bundle_index, proposal_id)?;
    if !matches!(
        batch.advance_request()?,
        crate::chain_submission::ChainAdvanceRequest::DelegateAndVoteBatch(_)
    ) {
        return Err(invalid("expected persisted delegation-and-cast recovery"));
    }
    Ok(batch)
}
