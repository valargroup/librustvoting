//! Complete-ballot voting for recoverable version 1 authorities.
//!
//! A recoverable bundle has one voting transaction. The transaction contains
//! every proposal with a `Choice` intent after all authenticated proposals are
//! terminal; `Skipped` proposals are intentionally absent. This keeps recovery
//! binary: the bundle's initial VAN is either unspent or was consumed by its
//! only ballot transaction.

use std::collections::BTreeMap;

use crate::{
    round::VotingDb,
    session::Decision,
    vote::{
        AtomicVoteBatch, DraftVote, PreparedAtomicVoteBatch, SignedVoteBatch, VanWitness,
        DEFAULT_BATCH_PROOF_CONCURRENCY,
    },
    VotingError,
};

use super::RecoverableBundleUseV1;

/// Round-level readiness for the one recoverable ballot transaction per bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoverableBallotReadinessV1 {
    /// These authenticated proposal IDs still lack a terminal intent.
    WaitingForIntents { proposal_ids: Vec<u32> },
    /// Every proposal is skipped, so there is no transaction to submit.
    AllSkipped,
    /// Each bundle may now prepare one batch containing these ordered IDs.
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
    /// `drafts` must contain every proposal whose durable ballot intent is
    /// `Choice`, in ascending proposal order. Every other authenticated
    /// proposal must have a durable `Skipped` intent.
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

/// Reports whether the authenticated round is ready for complete-ballot work.
///
/// This is the recoverable-v1 planning boundary. Generic `session::CastVote`
/// steps remain for legacy singleton flows and must not be used here.
pub fn recoverable_complete_ballot_readiness_v1(
    db: &VotingDb,
    authority: RecoverableBundleUseV1<'_>,
) -> Result<RecoverableBallotReadinessV1, VotingError> {
    authority
        .authority_root()
        .validate_current_selection(authority.authority_selection(), authority.current_round())?;
    let round_id = hex::encode(authority.authority_root().context().vote_round_id());
    {
        let conn = db.conn();
        authority.validate_persisted_round_with_conn(&conn, &db.wallet_id(), &round_id)?;
    }
    let proposal_count = authority.current_round().context().proposal_count();
    let (intents, missing) = load_recoverable_ballot_intents(db, &round_id, proposal_count)?;
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

/// Builds and signs the only atomic ballot allowed for one recoverable bundle.
///
/// The signed round's proposal count defines the complete sequential ID set
/// `1..=proposal_count`. Preparation fails unless each ID has a terminal
/// durable intent and the ordered drafts exactly match its `Choice` subset.
/// An all-skipped ballot has no chain action and must not call this function.
pub fn prepare_recoverable_complete_ballot_v1(
    db: &VotingDb,
    ballot: RecoverableCompleteBallotV1<'_>,
) -> Result<PreparedAtomicVoteBatch, VotingError> {
    let round_id = hex::encode(ballot.authority.authority_root().context().vote_round_id());
    validate_complete_ballot(db, &round_id, &ballot)?;

    let atomic_batch = AtomicVoteBatch::new(
        &round_id,
        ballot.authority.bundle_index(),
        ballot.drafts,
        ballot.witness,
        ballot.stages,
    )
    .with_max_proof_concurrency(ballot.max_proof_concurrency)?;
    crate::vote::prepare_recoverable_atomic_vote_batch_v1(db, ballot.authority, atomic_batch)
}

/// Builds, signs, and atomically persists one complete recoverable ballot.
///
/// The first successful persistence freezes the round's terminal intent set
/// before the signed payload is returned. Later bundles must commit the same
/// complete ballot.
pub fn commit_recoverable_complete_ballot_v1(
    db: &VotingDb,
    ballot: RecoverableCompleteBallotV1<'_>,
) -> Result<SignedVoteBatch, VotingError> {
    let prepared = prepare_recoverable_complete_ballot_v1(db, ballot)?;
    crate::vote::persist_prepared_atomic_vote_batch(db, prepared)
}

/// Durably records CheckTx acceptance for one recoverable complete ballot.
///
/// The write rechecks the complete current intent set and atomically rejects a
/// second submitted batch for the same bundle.
pub fn record_recoverable_complete_ballot_submission_v1(
    db: &VotingDb,
    authority: RecoverableBundleUseV1<'_>,
    batch_digest: &[u8],
    tx_hash: &str,
) -> Result<(), VotingError> {
    authority
        .authority_root()
        .validate_current_selection(authority.authority_selection(), authority.current_round())?;
    let round_id = hex::encode(authority.authority_root().context().vote_round_id());
    {
        let conn = db.conn();
        authority.validate_persisted_round_with_conn(&conn, &db.wallet_id(), &round_id)?;
    }
    crate::vote::record_recoverable_ballot_submission_v1(
        db,
        &round_id,
        authority.bundle_index(),
        batch_digest,
        tx_hash,
    )
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

    let proposal_count = ballot.authority.current_round().context().proposal_count();
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
    for stored_vote in &stored_votes {
        let state = {
            let conn = db.conn();
            crate::storage::queries::load_vote_row_state(
                &conn,
                round_id,
                &db.wallet_id(),
                ballot.authority.bundle_index(),
                stored_vote.proposal_id,
            )?
        }
        .ok_or_else(|| VotingError::Internal {
            message: "stored recoverable ballot vote disappeared while validating".to_string(),
        })?;
        if let Some(recovery_json) = state.commitment_bundle_json.as_deref() {
            let recovery = crate::vote::parse_recovery(recovery_json)?;
            if recovery.recoverable_ballot_proposal_count != Some(proposal_count) {
                return Err(VotingError::InvalidInput {
                    message: format!(
                        "recoverable bundle {} already has non-ballot vote recovery state",
                        ballot.authority.bundle_index()
                    ),
                });
            }
        } else if state.tx_hash.is_some() || state.vc_tree_position.is_some() {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "recoverable bundle {} has submitted vote state without complete-ballot recovery",
                    ballot.authority.bundle_index()
                ),
            });
        }
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
                "ballot intent contains a proposal outside the authenticated range 1..={proposal_count}"
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
            plan_recoverable_self_custody_bundles_v1, BundleMaterialSourceV1,
            RecoverableBundleMaterialV1, RegisteredKeyApplicationV1,
            SoftwareRegisteredKeyRequestV1, VotingAuthorityContextV1, VotingAuthorityRootV1,
            VotingAuthoritySelectionV1,
        },
        types::{Network, NoopProgressReporter, NoteInfo, VotingRoundParams},
    };

    const WALLET_ID: &str = "recoverable-ballot-test";

    struct Fixture {
        root: VotingAuthorityRootV1,
        selection: VotingAuthoritySelectionV1,
        round_auth: crate::config::VerifiedRoundAuthV3,
        bundles: super::super::RecoverableSelfCustodyBundlePlanV1,
    }

    impl Fixture {
        fn authority(&self) -> RecoverableBundleUseV1<'_> {
            RecoverableBundleUseV1::new(
                &self.root,
                &self.selection,
                &self.round_auth,
                RecoverableBundleMaterialV1::RecoverableSelfCustody(
                    self.bundles.bundle(0).unwrap().identity(),
                ),
            )
        }

        fn round_id(&self) -> String {
            hex::encode(self.root.context().vote_round_id())
        }
    }

    fn note(position: u64) -> NoteInfo {
        NoteInfo {
            commitment: vec![position as u8 + 1; 32],
            nullifier: vec![position as u8 + 17; 32],
            value: BALLOT_DIVISOR,
            position,
            diversifier: vec![0; 11],
            rho: vec![0; 32],
            rseed: vec![0; 32],
            scope: 0,
            ufvk_str: String::new(),
        }
    }

    fn fixture() -> (VotingDb, Fixture) {
        let context = VotingAuthorityContextV1::from_fingerprint(
            Network::Testnet,
            0,
            [0x22; 32],
            "vote-chain-test",
            [0x01; 32],
        )
        .unwrap();
        let request =
            SoftwareRegisteredKeyRequestV1::new(RegisteredKeyApplicationV1::new(1), context);
        let root = VotingAuthorityRootV1::from_registered_key_output(&request, [0x55; 64]);
        let round_auth = super::super::test_verified_round_auth_v3(root.context());
        let selection = VotingAuthoritySelectionV1::bind(
            &root,
            BundleMaterialSourceV1::RecoverableSelfCustody,
            &round_auth,
        )
        .unwrap();
        let bundles = plan_recoverable_self_custody_bundles_v1(&[note(0)]).unwrap();
        let fixture = Fixture {
            root,
            selection,
            round_auth,
            bundles,
        };

        let db = VotingDb::open(":memory:").unwrap();
        db.set_wallet_id(WALLET_ID);
        db.create_round(
            Network::Testnet,
            &VotingRoundParams {
                vote_round_id: fixture.round_id(),
                snapshot_height: fixture.round_auth.context().snapshot_height(),
                ea_pk: fixture.round_auth.ea_pk().to_vec(),
                nc_root: vec![0x33; 32],
                nullifier_imt_root: vec![0x44; 32],
            },
            None,
        )
        .unwrap();
        (db, fixture)
    }

    fn draft(proposal_id: u32, choice: u32) -> DraftVote {
        DraftVote {
            proposal_id,
            choice,
            num_options: 3,
            single_share: false,
            vc_tree_position: 0,
        }
    }

    fn witness() -> VanWitness {
        VanWitness {
            auth_path: vec![],
            position: 0,
            anchor_height: 0,
        }
    }

    #[test]
    fn complete_ballot_requires_every_terminal_intent_and_exact_choice_subset() {
        let (db, fixture) = fixture();
        let round_id = fixture.round_id();
        db.set_ballot_intent(&round_id, 1, Decision::Choice(0), 3)
            .unwrap();
        db.set_ballot_intent(&round_id, 2, Decision::Skipped, 3)
            .unwrap();
        assert_eq!(
            recoverable_complete_ballot_readiness_v1(&db, fixture.authority()).unwrap(),
            RecoverableBallotReadinessV1::WaitingForIntents {
                proposal_ids: vec![3],
            }
        );
        let drafts = [draft(1, 0), draft(3, 1)];
        let witness = witness();
        let ballot = RecoverableCompleteBallotV1::new(
            fixture.authority(),
            &drafts,
            &witness,
            &NoopProgressReporter,
        );
        assert!(validate_complete_ballot(&db, &round_id, &ballot)
            .unwrap_err()
            .to_string()
            .contains("one terminal intent"));

        db.set_ballot_intent(&round_id, 3, Decision::Choice(1), 3)
            .unwrap();
        assert_eq!(
            recoverable_complete_ballot_readiness_v1(&db, fixture.authority()).unwrap(),
            RecoverableBallotReadinessV1::Ready {
                proposal_ids: vec![1, 3],
            }
        );
        validate_complete_ballot(&db, &round_id, &ballot).unwrap();

        let wrong = [draft(1, 0), draft(2, 1), draft(3, 1)];
        let ballot = RecoverableCompleteBallotV1::new(
            fixture.authority(),
            &wrong,
            &witness,
            &NoopProgressReporter,
        );
        assert!(validate_complete_ballot(&db, &round_id, &ballot)
            .unwrap_err()
            .to_string()
            .contains("omit skipped proposal 2"));
    }

    #[test]
    fn complete_ballot_requires_ascending_order_and_treats_all_skipped_as_no_transaction() {
        let (db, fixture) = fixture();
        let round_id = fixture.round_id();
        for proposal_id in 1..=3 {
            db.set_ballot_intent(&round_id, proposal_id, Decision::Skipped, 3)
                .unwrap();
        }
        assert_eq!(
            recoverable_complete_ballot_readiness_v1(&db, fixture.authority()).unwrap(),
            RecoverableBallotReadinessV1::AllSkipped
        );
        let witness = witness();
        let ballot = RecoverableCompleteBallotV1::new(
            fixture.authority(),
            &[],
            &witness,
            &NoopProgressReporter,
        );
        assert!(validate_complete_ballot(&db, &round_id, &ballot)
            .unwrap_err()
            .to_string()
            .contains("no vote transaction is required"));

        db.set_ballot_intent(&round_id, 1, Decision::Choice(0), 3)
            .unwrap();
        db.set_ballot_intent(&round_id, 3, Decision::Choice(1), 3)
            .unwrap();
        let reversed = [draft(3, 1), draft(1, 0)];
        let ballot = RecoverableCompleteBallotV1::new(
            fixture.authority(),
            &reversed,
            &witness,
            &NoopProgressReporter,
        );
        assert!(validate_complete_ballot(&db, &round_id, &ballot)
            .unwrap_err()
            .to_string()
            .contains("ascending proposal order"));
    }

    #[test]
    fn submission_recording_revalidates_the_persisted_round() {
        let (db, fixture) = fixture();
        db.conn()
            .execute(
                "UPDATE rounds SET snapshot_height = snapshot_height + 1
                 WHERE round_id = ?1 AND wallet_id = ?2",
                rusqlite::params![fixture.round_id(), WALLET_ID],
            )
            .unwrap();

        let error = record_recoverable_complete_ballot_submission_v1(
            &db,
            fixture.authority(),
            &[0; 32],
            "tx",
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("persisted round parameters do not match"),
            "{error}"
        );
    }
}
