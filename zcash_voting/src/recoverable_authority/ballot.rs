//! One-transaction ballots for recoverable version 1 authorities.
//!
//! Every proposal must have a terminal `Choice` or `Skipped` intent before a
//! bundle can vote. All choices are then carried by one atomic transaction, so
//! recovery only needs to determine whether the bundle's initial VAN was spent.

use std::collections::BTreeMap;

use crate::{
    round::VotingDb,
    session::Decision,
    vote::{
        AtomicVoteBatch, DraftVote, SignedVoteBatch, VanWitness, DEFAULT_BATCH_PROOF_CONCURRENCY,
    },
    VotingError,
};

use super::RecoverableBundleUseV1;

/// Round-level readiness for a recoverable bundle's one ballot transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoverableBallotReadinessV1 {
    /// These chain proposal IDs still lack a terminal intent.
    WaitingForIntents { proposal_ids: Vec<u32> },
    /// Every proposal is skipped, so there is no transaction to submit.
    AllSkipped,
    /// The bundle may submit one atomic transaction with these ordered IDs.
    Ready { proposal_ids: Vec<u32> },
}

/// Inputs for the only vote transaction permitted for one recoverable bundle.
pub struct RecoverableCompleteBallotV1<'a> {
    authority: RecoverableBundleUseV1<'a>,
    drafts: &'a [DraftVote],
    witness: &'a VanWitness,
    stages: &'a dyn crate::types::VoteCommitStageReporter,
    max_proof_concurrency: usize,
}

impl<'a> RecoverableCompleteBallotV1<'a> {
    /// Creates a complete ballot using the default proof concurrency.
    ///
    /// `drafts` must contain every `Choice` intent in ascending proposal order.
    /// Every other proposal in the chain round must have a `Skipped` intent.
    pub fn new(
        authority: RecoverableBundleUseV1<'a>,
        drafts: &'a [DraftVote],
        witness: &'a VanWitness,
        stages: &'a dyn crate::types::VoteCommitStageReporter,
    ) -> Self {
        Self {
            authority,
            drafts,
            witness,
            stages,
            max_proof_concurrency: DEFAULT_BATCH_PROOF_CONCURRENCY,
        }
    }

    /// Overrides the maximum number of vote proofs built concurrently.
    pub fn with_max_proof_concurrency(
        mut self,
        max_proof_concurrency: usize,
    ) -> Result<Self, VotingError> {
        if max_proof_concurrency == 0 {
            return Err(VotingError::InvalidInput {
                message: "max_proof_concurrency must be at least 1".to_string(),
            });
        }
        self.max_proof_concurrency = max_proof_concurrency;
        Ok(self)
    }
}

/// Reports whether the chain round is ready for complete-ballot work.
pub fn recoverable_complete_ballot_readiness_v1(
    db: &VotingDb,
    authority: RecoverableBundleUseV1<'_>,
) -> Result<RecoverableBallotReadinessV1, VotingError> {
    let round_id = authority.current_round().round_id();
    {
        let conn = db.conn();
        authority.validate_persisted_with_conn(
            &conn,
            &db.wallet_id(),
            round_id,
            authority.bundle_index(),
        )?;
    }
    let proposal_count = authority.current_round().proposal_count();
    let (intents, missing) = load_recoverable_ballot_intents(db, round_id, proposal_count)?;
    if !missing.is_empty() {
        return Ok(RecoverableBallotReadinessV1::WaitingForIntents {
            proposal_ids: missing,
        });
    }
    let proposal_ids = intents
        .into_iter()
        .filter_map(|(proposal_id, decision)| {
            matches!(decision, Decision::Choice(_)).then_some(proposal_id)
        })
        .collect::<Vec<_>>();
    if proposal_ids.is_empty() {
        Ok(RecoverableBallotReadinessV1::AllSkipped)
    } else {
        Ok(RecoverableBallotReadinessV1::Ready { proposal_ids })
    }
}

/// Builds, signs, and atomically persists one complete recoverable ballot.
///
/// Persistence rechecks all terminal intents under the same write transaction
/// that stores the batch. The first bundle freezes that round-wide ballot;
/// later bundles may persist only the same complete intent set.
pub fn commit_recoverable_complete_ballot_v1(
    db: &VotingDb,
    ballot: RecoverableCompleteBallotV1<'_>,
) -> Result<SignedVoteBatch, VotingError> {
    let round_id = ballot.authority.current_round().round_id();
    validate_complete_ballot(db, round_id, &ballot)?;
    let proposal_count = ballot.authority.current_round().proposal_count();
    let atomic_batch = AtomicVoteBatch::new(
        round_id,
        ballot.authority.bundle_index(),
        ballot.drafts,
        ballot.witness,
        ballot.stages,
    )
    .with_max_proof_concurrency(ballot.max_proof_concurrency)?;
    let prepared =
        crate::vote::prepare_recoverable_atomic_vote_batch_v1(db, ballot.authority, atomic_batch)?;
    crate::vote::persist_prepared_recoverable_atomic_vote_batch_v1(db, prepared, proposal_count)
}

fn validate_complete_ballot(
    db: &VotingDb,
    round_id: &str,
    ballot: &RecoverableCompleteBallotV1<'_>,
) -> Result<(), VotingError> {
    if !ballot.drafts.is_empty() {
        crate::vote::validate_draft_votes(ballot.drafts)?;
    }
    if ballot
        .drafts
        .windows(2)
        .any(|pair| pair[0].proposal_id >= pair[1].proposal_id)
    {
        return Err(VotingError::InvalidInput {
            message: "recoverable ballot drafts must be in ascending proposal order".to_string(),
        });
    }

    let proposal_count = ballot.authority.current_round().proposal_count();
    let (intents, missing) = load_recoverable_ballot_intents(db, round_id, proposal_count)?;
    if !missing.is_empty() {
        return Err(VotingError::InvalidInput {
            message: format!(
                "recoverable ballot requires one terminal intent for each proposal 1..={proposal_count}"
            ),
        });
    }

    let drafts = ballot
        .drafts
        .iter()
        .map(|draft| (draft.proposal_id, draft))
        .collect::<BTreeMap<_, _>>();
    for proposal_id in 1..=proposal_count {
        match (intents[&proposal_id], drafts.get(&proposal_id)) {
            (Decision::Choice(choice), Some(draft)) if draft.choice == choice => {}
            (Decision::Skipped, None) => {}
            (Decision::Choice(_), _) => {
                return Err(VotingError::InvalidInput {
                    message: format!(
                        "recoverable ballot draft does not match the Choice intent for proposal {proposal_id}"
                    ),
                });
            }
            (Decision::Skipped, Some(_)) => {
                return Err(VotingError::InvalidInput {
                    message: format!("recoverable ballot must omit skipped proposal {proposal_id}"),
                });
            }
        }
    }
    if ballot.drafts.is_empty() {
        return Err(VotingError::InvalidInput {
            message: "the complete ballot is all skipped; no vote transaction is required"
                .to_string(),
        });
    }

    let stored_votes = db
        .get_votes(round_id)?
        .into_iter()
        .filter(|vote| vote.bundle_index == ballot.authority.bundle_index())
        .collect::<Vec<_>>();
    if !stored_votes.is_empty()
        && (stored_votes.len() != ballot.drafts.len()
            || stored_votes.iter().any(|vote| {
                drafts
                    .get(&vote.proposal_id)
                    .is_none_or(|draft| draft.choice != vote.choice)
            }))
    {
        return Err(VotingError::InvalidInput {
            message: format!(
                "recoverable bundle {} already has a different or partial vote",
                ballot.authority.bundle_index()
            ),
        });
    }

    Ok(())
}

fn load_recoverable_ballot_intents(
    db: &VotingDb,
    round_id: &str,
    proposal_count: u32,
) -> Result<(BTreeMap<u32, Decision>, Vec<u32>), VotingError> {
    let intents = db
        .ballot_intents(round_id)?
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    if intents
        .keys()
        .any(|proposal_id| !(1..=proposal_count).contains(proposal_id))
    {
        return Err(VotingError::InvalidInput {
            message: format!(
                "ballot intent contains a proposal outside the chain range 1..={proposal_count}"
            ),
        });
    }
    let missing = (1..=proposal_count)
        .filter(|proposal_id| !intents.contains_key(proposal_id))
        .collect();
    Ok((intents, missing))
}
