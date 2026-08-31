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
    if let Some(draft) = ballot
        .drafts
        .iter()
        .find(|draft| !(1..=proposal_count).contains(&draft.proposal_id))
    {
        return Err(VotingError::InvalidInput {
            message: format!(
                "recoverable ballot draft proposal {} is outside the chain range 1..={proposal_count}",
                draft.proposal_id
            ),
        });
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        governance::BALLOT_DIVISOR,
        recoverable_authority::{
            recoverable_bundle_policy_v1, test_validated_recoverable_voting_round_v1,
            RecoverableBundleMaterialV1, RecoverableSelfCustodyBundleV1,
            RegisteredKeyApplicationV1, SoftwareRegisteredKeyRequestV1,
            ValidatedRecoverableVotingRoundV1, VotingAuthorityContextV1,
            VotingAuthorityRootBindingV1, VotingAuthorityRootV1,
        },
        types::{Network, NoopProgressReporter, NoteInfo},
        wire::VotingRoundParams,
    };

    const ROUND_ID: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    const CHAIN_ID: &str = "vote-chain-test";
    const WALLET_ID: &str = "wallet";

    struct Fixture {
        db: VotingDb,
        root: VotingAuthorityRootV1,
        binding: VotingAuthorityRootBindingV1,
        round: ValidatedRecoverableVotingRoundV1,
        bundle: RecoverableSelfCustodyBundleV1,
    }

    impl Fixture {
        fn new() -> Self {
            let params = round_params();
            let context = VotingAuthorityContextV1::from_fingerprint(
                Network::Testnet,
                0,
                [0x22; 32],
                CHAIN_ID,
                [0x01; 32],
            )
            .unwrap();
            let request = SoftwareRegisteredKeyRequestV1::new(
                RegisteredKeyApplicationV1::new(0xA11C),
                context,
            );
            let root = VotingAuthorityRootV1::from_registered_key_output(&request, [0x55; 64]);
            let binding = VotingAuthorityRootBindingV1::bind(&root);
            let round = test_validated_recoverable_voting_round_v1(
                Network::Testnet,
                CHAIN_ID,
                params.clone(),
                [0xAB; 32],
                3,
            );
            let note = note();
            let bundle =
                RecoverableSelfCustodyBundleV1::from_canonical_bundle(0, vec![note.clone()])
                    .unwrap();

            let db = VotingDb::open_in_memory().unwrap();
            db.set_wallet_id(WALLET_ID);
            db.create_round(Network::Testnet, &params, None).unwrap();
            db.ensure_bundles_with_policy(ROUND_ID, &[note], recoverable_bundle_policy_v1())
                .unwrap();

            let authority = RecoverableBundleUseV1::new(
                &root,
                &binding,
                &round,
                RecoverableBundleMaterialV1::RecoverableSelfCustody(bundle.identity()),
            )
            .unwrap();
            let expected = authority.expected_material().unwrap();
            db.conn()
                .execute(
                    "UPDATE bundles SET van_comm_rand = ?1, gov_comm = ?2,
                        total_note_value = ?3, address_index = 0
                     WHERE round_id = ?4 AND wallet_id = ?5 AND bundle_index = 0",
                    rusqlite::params![
                        expected.van_blinding.as_slice(),
                        expected.van.as_slice(),
                        expected.total_note_value as i64,
                        ROUND_ID,
                        WALLET_ID,
                    ],
                )
                .unwrap();

            Self {
                db,
                root,
                binding,
                round,
                bundle,
            }
        }

        fn authority(&self) -> RecoverableBundleUseV1<'_> {
            RecoverableBundleUseV1::new(
                &self.root,
                &self.binding,
                &self.round,
                RecoverableBundleMaterialV1::RecoverableSelfCustody(self.bundle.identity()),
            )
            .unwrap()
        }

        fn set_complete_intents(&self) {
            self.db
                .set_ballot_intent(ROUND_ID, 1, Decision::Choice(0), 2)
                .unwrap();
            self.db
                .set_ballot_intent(ROUND_ID, 2, Decision::Skipped, 2)
                .unwrap();
            self.db
                .set_ballot_intent(ROUND_ID, 3, Decision::Choice(2), 3)
                .unwrap();
        }
    }

    fn round_params() -> VotingRoundParams {
        VotingRoundParams {
            vote_round_id: ROUND_ID.to_string(),
            snapshot_height: 1000,
            ea_pk: vec![0xEA; 32],
            nc_root: vec![0xAA; 32],
            nullifier_imt_root: vec![0xBB; 32],
        }
    }

    fn note() -> NoteInfo {
        NoteInfo {
            commitment: vec![0x01; 32],
            nullifier: vec![0x02; 32],
            value: BALLOT_DIVISOR,
            position: 0,
            diversifier: vec![0x03; 11],
            rho: vec![0x04; 32],
            rseed: vec![0x05; 32],
            scope: 0,
            ufvk_str: "uview1test".to_string(),
        }
    }

    fn draft(proposal_id: u32, choice: u32, num_options: u32) -> DraftVote {
        DraftVote {
            proposal_id,
            choice,
            num_options,
            single_share: false,
            vc_tree_position: 0,
        }
    }

    #[test]
    fn readiness_waits_for_every_chain_proposal() {
        let fixture = Fixture::new();
        assert_eq!(
            recoverable_complete_ballot_readiness_v1(&fixture.db, fixture.authority()).unwrap(),
            RecoverableBallotReadinessV1::WaitingForIntents {
                proposal_ids: vec![1, 2, 3]
            }
        );

        fixture.set_complete_intents();
        assert_eq!(
            recoverable_complete_ballot_readiness_v1(&fixture.db, fixture.authority()).unwrap(),
            RecoverableBallotReadinessV1::Ready {
                proposal_ids: vec![1, 3]
            }
        );
    }

    #[test]
    fn complete_ballot_matches_choices_and_omits_skips() {
        let fixture = Fixture::new();
        fixture.set_complete_intents();
        let witness = VanWitness {
            auth_path: Vec::new(),
            position: 0,
            anchor_height: 0,
        };
        let drafts = [draft(1, 0, 2), draft(3, 2, 3)];
        let ballot = RecoverableCompleteBallotV1::new(
            fixture.authority(),
            &drafts,
            &witness,
            &NoopProgressReporter,
        );
        validate_complete_ballot(&fixture.db, ROUND_ID, &ballot).unwrap();

        let missing_choice = [draft(1, 0, 2)];
        let ballot = RecoverableCompleteBallotV1::new(
            fixture.authority(),
            &missing_choice,
            &witness,
            &NoopProgressReporter,
        );
        let err = validate_complete_ballot(&fixture.db, ROUND_ID, &ballot).unwrap_err();
        assert!(
            err.to_string().contains("Choice intent for proposal 3"),
            "{err}"
        );

        let includes_skip = [draft(1, 0, 2), draft(2, 0, 2), draft(3, 2, 3)];
        let ballot = RecoverableCompleteBallotV1::new(
            fixture.authority(),
            &includes_skip,
            &witness,
            &NoopProgressReporter,
        );
        let err = validate_complete_ballot(&fixture.db, ROUND_ID, &ballot).unwrap_err();
        assert!(err.to_string().contains("omit skipped proposal 2"), "{err}");

        let outside_range = [draft(1, 0, 2), draft(3, 2, 3), draft(4, 0, 2)];
        let ballot = RecoverableCompleteBallotV1::new(
            fixture.authority(),
            &outside_range,
            &witness,
            &NoopProgressReporter,
        );
        let err = validate_complete_ballot(&fixture.db, ROUND_ID, &ballot).unwrap_err();
        assert!(
            err.to_string().contains("outside the chain range 1..=3"),
            "{err}"
        );
    }

    #[test]
    fn all_skipped_needs_no_transaction_and_drafts_must_be_ordered() {
        let fixture = Fixture::new();
        for proposal_id in 1..=3 {
            fixture
                .db
                .set_ballot_intent(ROUND_ID, proposal_id, Decision::Skipped, 3)
                .unwrap();
        }
        assert_eq!(
            recoverable_complete_ballot_readiness_v1(&fixture.db, fixture.authority()).unwrap(),
            RecoverableBallotReadinessV1::AllSkipped
        );

        fixture
            .db
            .set_ballot_intent(ROUND_ID, 1, Decision::Choice(0), 3)
            .unwrap();
        fixture
            .db
            .set_ballot_intent(ROUND_ID, 3, Decision::Choice(1), 3)
            .unwrap();
        let witness = VanWitness {
            auth_path: Vec::new(),
            position: 0,
            anchor_height: 0,
        };
        let reversed = [draft(3, 1, 3), draft(1, 0, 3)];
        let ballot = RecoverableCompleteBallotV1::new(
            fixture.authority(),
            &reversed,
            &witness,
            &NoopProgressReporter,
        );
        let err = validate_complete_ballot(&fixture.db, ROUND_ID, &ballot).unwrap_err();
        assert!(
            err.to_string().contains("ascending proposal order"),
            "{err}"
        );
    }

    #[test]
    fn readiness_rejects_mismatched_persisted_bundle_material() {
        let fixture = Fixture::new();
        fixture
            .db
            .conn()
            .execute(
                "UPDATE bundles SET gov_comm = ?1
                 WHERE round_id = ?2 AND wallet_id = ?3 AND bundle_index = 0",
                rusqlite::params![vec![0xFFu8; 32], ROUND_ID, WALLET_ID],
            )
            .unwrap();

        let err =
            recoverable_complete_ballot_readiness_v1(&fixture.db, fixture.authority()).unwrap_err();
        assert!(
            err.to_string()
                .contains("persisted bundle material does not match"),
            "{err}"
        );
    }
}
