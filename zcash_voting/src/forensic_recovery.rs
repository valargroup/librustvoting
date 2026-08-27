//! Narrow recovery path for historical delegation rows whose local VAN
//! randomness was discarded after the delegation had already landed.

use std::collections::{HashMap, HashSet};

use ff::PrimeField;
use pasta_curves::pallas;
use rusqlite::{named_params, OptionalExtension, TransactionBehavior};
use zcash_protocol::value::MAX_MONEY;

use crate::{
    action::derive_hotkey_x_coords_from_raw_address,
    delegation_capability::MAX_DELEGATION_CAPABILITY_BUNDLES,
    governance::{construct_van, BALLOT_DIVISOR},
    storage::{queries, RoundPhase, VotingDb},
    tree_sync::{verified_vote_tree_snapshot, VerifiedVoteTreeSnapshot},
    types::{validate_round_params, Network, VotingError, VotingHotkey, VotingRoundParams},
};

/// One delegation bundle reconstructed from preserved database bytes.
///
/// The randomness is privacy-sensitive, so this type deliberately omits
/// `Debug`. Callers must not log instances or their serialized form.
#[derive(Clone, PartialEq, Eq)]
pub struct ForensicDelegationBundle {
    /// Zero-based index in the original delegation batch.
    pub bundle_index: u32,
    /// Exact raw zatoshi weight committed by this bundle.
    pub total_note_value: u64,
    /// Voting hotkey address index. Version 1 requires zero.
    pub address_index: u32,
    /// Canonical 32-byte Pallas encoding of the recovered VAN randomness.
    pub van_comm_rand: [u8; 32],
    /// Canonical 32-byte VAN commitment recovered alongside the randomness.
    pub van_commitment: [u8; 32],
    /// Exact public position where that commitment appears in the round tree.
    pub van_leaf_position: u32,
    /// Original delegation transaction hash when it survived forensic recovery.
    pub delegation_tx_hash: Option<String>,
}

/// Independently trusted context for a historical delegation repair.
pub struct RecoverDelegationFromForensicEvidenceParams<'a> {
    /// Voter-owned hotkey whose public target must reproduce every VAN.
    pub voting_hotkey: &'a VotingHotkey,
    /// Zcash network from the authenticated round configuration.
    pub expected_network: Network,
    /// Complete authenticated parameters for the already-stored round.
    pub expected_round_params: &'a VotingRoundParams,
    /// Vote-chain REST endpoint used for an independent full-tree validation.
    pub node_url: &'a str,
    /// Nonempty recovered subset, in strictly increasing bundle order.
    pub bundles: &'a [ForensicDelegationBundle],
}

/// Result of an atomic historical delegation repair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForensicDelegationRecovery {
    /// Height of the root-validated tree used to authorize the repair.
    pub anchor_height: u32,
    /// Root of the tree used to authorize the repair.
    pub tree_root: [u8; 32],
    /// Number of bundles validated and restored atomically.
    pub bundle_count: u32,
    /// Exact original bundle indices validated by this recovery.
    pub recovered_bundle_indices: Vec<u32>,
    /// True when the database already contained the exact repaired state.
    pub already_recovered: bool,
}

struct ValidatedForensicBundle {
    index: u32,
    total_note_value: u64,
    rand: [u8; 32],
    van: [u8; 32],
    position: u32,
    tx_hash: Option<String>,
}

/// Validates recovered secrets against the voter hotkey and a freshly
/// root-validated public tree, then atomically installs only the minimal state
/// needed to resume voting.
///
/// This function is intentionally unsuitable for routine recovery. It accepts
/// only a nonempty subset of unsigned, never-submitted local delegation rows
/// and preserves every row outside that subset so the ordinary delegation flow
/// can finish bundles that never landed. Conflicting or already-voted state is
/// rejected.
pub fn recover_delegation_from_forensic_evidence(
    db: &VotingDb,
    params: RecoverDelegationFromForensicEvidenceParams<'_>,
) -> Result<ForensicDelegationRecovery, VotingError> {
    validate_context(&params)?;
    let validated = validate_bundles(&params)?;
    let snapshot =
        verified_vote_tree_snapshot(&params.expected_round_params.vote_round_id, params.node_url)?;
    recover_with_verified_snapshot(db, &params, &validated, &snapshot)
}

fn validate_context(
    params: &RecoverDelegationFromForensicEvidenceParams<'_>,
) -> Result<(), VotingError> {
    validate_round_params(params.expected_round_params)?;
    i64::try_from(params.expected_round_params.snapshot_height)
        .map_err(|_| invalid("snapshot_height does not fit in SQLite INTEGER"))?;
    if params.voting_hotkey.network() != params.expected_network {
        return Err(invalid(
            "voting hotkey does not match the trusted recovery network",
        ));
    }
    Ok(())
}

fn validate_bundles(
    params: &RecoverDelegationFromForensicEvidenceParams<'_>,
) -> Result<Vec<ValidatedForensicBundle>, VotingError> {
    if params.bundles.is_empty() || params.bundles.len() > MAX_DELEGATION_CAPABILITY_BUNDLES {
        return Err(invalid(format!(
            "forensic recovery must contain 1..={MAX_DELEGATION_CAPABILITY_BUNDLES} bundles"
        )));
    }

    let round_id: [u8; 32] = hex::decode(&params.expected_round_params.vote_round_id)
        .expect("validated round id must decode")
        .try_into()
        .expect("validated round id must contain 32 bytes");
    let target = params.voting_hotkey.delegation_target();
    let (g_d_x, pk_d_x) = derive_hotkey_x_coords_from_raw_address(target.raw_orchard_address())?;
    let mut recovered_total = 0u64;
    let mut commitments = HashSet::with_capacity(params.bundles.len());
    let mut positions = HashSet::with_capacity(params.bundles.len());
    let mut tx_hashes = HashSet::with_capacity(params.bundles.len());
    let mut validated = Vec::with_capacity(params.bundles.len());

    let mut previous_index = None;
    for bundle in params.bundles {
        if bundle.bundle_index as usize >= MAX_DELEGATION_CAPABILITY_BUNDLES {
            return Err(invalid(format!(
                "forensic bundle index {} exceeds the supported bundle range",
                bundle.bundle_index
            )));
        }
        if previous_index.is_some_and(|previous| bundle.bundle_index <= previous) {
            return Err(invalid(
                "forensic bundle indices must be unique and strictly increasing",
            ));
        }
        previous_index = Some(bundle.bundle_index);
        if bundle.address_index != target.address_index() {
            return Err(invalid(
                "forensic bundle address index does not match the voting hotkey",
            ));
        }
        // Delegation stores the raw note sum. VAN construction quantizes it
        // down to whole ballots, so a valid stored weight can have a remainder.
        if bundle.total_note_value < BALLOT_DIVISOR || bundle.total_note_value > MAX_MONEY {
            return Err(invalid(
                "forensic bundle voting weight must yield at least one ballot",
            ));
        }
        recovered_total = recovered_total
            .checked_add(bundle.total_note_value)
            .filter(|total| *total <= MAX_MONEY)
            .ok_or_else(|| invalid("forensic recovery voting weight exceeds MAX_MONEY"))?;
        if pallas::Base::from_repr(bundle.van_comm_rand)
            .is_none()
            .into()
        {
            return Err(invalid(
                "forensic van_comm_rand must be a canonical Pallas field element",
            ));
        }

        let expected_van: [u8; 32] = construct_van(
            &g_d_x,
            &pk_d_x,
            bundle.total_note_value,
            &round_id,
            &bundle.van_comm_rand,
        )?
        .try_into()
        .expect("construct_van returns 32 bytes");
        if expected_van != bundle.van_commitment {
            return Err(invalid(
                "forensic VAN does not match the recovered randomness and voting hotkey",
            ));
        }
        if !commitments.insert(expected_van) {
            return Err(invalid("forensic VAN commitments must be unique"));
        }
        if !positions.insert(bundle.van_leaf_position) {
            return Err(invalid("forensic VAN leaf positions must be unique"));
        }
        if let Some(hash) = &bundle.delegation_tx_hash {
            validate_hash(hash)?;
            if !tx_hashes.insert(hash.clone()) {
                return Err(invalid(
                    "forensic delegation transaction hashes must be unique",
                ));
            }
        }

        validated.push(ValidatedForensicBundle {
            index: bundle.bundle_index,
            total_note_value: bundle.total_note_value,
            rand: bundle.van_comm_rand,
            van: expected_van,
            position: bundle.van_leaf_position,
            tx_hash: bundle.delegation_tx_hash.clone(),
        });
    }
    Ok(validated)
}

fn recover_with_verified_snapshot(
    db: &VotingDb,
    params: &RecoverDelegationFromForensicEvidenceParams<'_>,
    bundles: &[ValidatedForensicBundle],
    snapshot: &VerifiedVoteTreeSnapshot,
) -> Result<ForensicDelegationRecovery, VotingError> {
    validate_snapshot_bindings(bundles, snapshot)?;

    let round_id = &params.expected_round_params.vote_round_id;
    let wallet_id = db.wallet_id();
    let mut conn = db.conn();
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| internal(format!("begin forensic delegation recovery failed: {e}")))?;

    // Repeat all mutable-state checks under the write reservation so another
    // caller cannot advance or replace this round between validation and write.
    let (stored, network) = queries::load_round_params_with_network(&tx, round_id, &wallet_id)?;
    if stored != *params.expected_round_params || network != params.expected_network {
        return Err(invalid(
            "stored round context conflicts with forensic recovery",
        ));
    }
    let stored_count = queries::get_bundle_count(&tx, round_id, &wallet_id)?;
    if stored_count == 0 || bundles.iter().any(|bundle| bundle.index >= stored_count) {
        return Err(invalid(
            "forensic bundle index falls outside the stored delegation batch",
        ));
    }

    let exact_matches = bundles
        .iter()
        .map(|bundle| forensic_bundle_matches(&tx, round_id, &wallet_id, bundle))
        .collect::<Result<Vec<_>, _>>()?;
    if exact_matches.iter().all(|matches| *matches) {
        tx.commit()
            .map_err(|e| internal(format!("commit forensic recovery no-op failed: {e}")))?;
        return Ok(recovery_result(snapshot, bundles, true));
    }

    let phase: i32 = tx
        .query_row(
            "SELECT phase FROM rounds
             WHERE round_id = :round_id AND wallet_id = :wallet_id",
            named_params! { ":round_id": round_id, ":wallet_id": wallet_id },
            |row| row.get(0),
        )
        .map_err(|e| internal(format!("check forensic recovery round phase failed: {e}")))?;
    if phase > RoundPhase::DelegationProved as i32 {
        return Err(invalid(
            "forensic delegation recovery cannot replace a vote-ready round",
        ));
    }

    let downstream_count: i64 = tx
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM votes
                  WHERE round_id = :round_id AND wallet_id = :wallet_id)
               + (SELECT COUNT(*) FROM share_delegations
                  WHERE round_id = :round_id AND wallet_id = :wallet_id)",
            named_params! { ":round_id": round_id, ":wallet_id": wallet_id },
            |row| row.get(0),
        )
        .map_err(|e| internal(format!("check forensic recovery vote state failed: {e}")))?;
    if downstream_count != 0 {
        return Err(invalid(
            "forensic delegation recovery cannot replace state after voting began",
        ));
    }

    let public_commitments = snapshot
        .leaves
        .iter()
        .map(|leaf| leaf.commitment)
        .collect::<HashSet<_>>();
    validate_replaceable_rows(
        &tx,
        round_id,
        &wallet_id,
        bundles,
        &exact_matches,
        &public_commitments,
    )?;

    for (bundle, exact) in bundles.iter().zip(exact_matches) {
        if !exact {
            replace_recovered_bundle(&tx, round_id, &wallet_id, bundle)?;
        }
    }
    tx.execute(
        "UPDATE rounds SET phase = :phase
         WHERE round_id = :round_id AND wallet_id = :wallet_id AND phase < :phase",
        named_params! {
            ":phase": RoundPhase::DelegationProved as i32,
            ":round_id": round_id,
            ":wallet_id": wallet_id,
        },
    )
    .map_err(|e| internal(format!("advance recovered round phase failed: {e}")))?;
    tx.commit()
        .map_err(|e| internal(format!("commit forensic delegation recovery failed: {e}")))?;

    Ok(recovery_result(snapshot, bundles, false))
}

fn validate_snapshot_bindings(
    bundles: &[ValidatedForensicBundle],
    snapshot: &VerifiedVoteTreeSnapshot,
) -> Result<(), VotingError> {
    let mut positions_by_commitment: HashMap<[u8; 32], Vec<u32>> = HashMap::new();
    for leaf in &snapshot.leaves {
        positions_by_commitment
            .entry(leaf.commitment)
            .or_default()
            .push(leaf.position);
    }
    for bundle in bundles {
        let positions = positions_by_commitment
            .get(&bundle.van)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if positions != [bundle.position] {
            return Err(invalid(format!(
                "forensic VAN for bundle {} must occur exactly once at its claimed tree position",
                bundle.index
            )));
        }
    }
    Ok(())
}

fn validate_replaceable_rows(
    conn: &rusqlite::Connection,
    round_id: &str,
    wallet_id: &str,
    bundles: &[ValidatedForensicBundle],
    exact_matches: &[bool],
    public_commitments: &HashSet<[u8; 32]>,
) -> Result<(), VotingError> {
    if bundles.len() != exact_matches.len() {
        return Err(internal("forensic recovery match count is inconsistent"));
    }

    for (bundle, exact) in bundles.iter().zip(exact_matches) {
        if *exact {
            continue;
        }
        let row = conn
            .query_row(
                "SELECT b.note_positions_blob IS NOT NULL,
                        b.van_leaf_position, b.gov_comm,
                        b.delegation_tx_hash IS NULL,
                        NOT EXISTS (
                            SELECT 1 FROM keystone_signatures ks
                            WHERE ks.round_id = b.round_id
                              AND ks.wallet_id = b.wallet_id
                              AND ks.bundle_index = b.bundle_index
                        )
                 FROM bundles b
                 WHERE b.round_id = :round_id AND b.wallet_id = :wallet_id
                   AND b.bundle_index = :bundle_index",
                named_params! {
                    ":round_id": round_id,
                    ":wallet_id": wallet_id,
                    ":bundle_index": i64::from(bundle.index),
                },
                |row| {
                    Ok((
                        row.get::<_, bool>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                        row.get::<_, bool>(3)?,
                        row.get::<_, bool>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| internal(format!("check replaceable forensic bundle failed: {e}")))?
            .ok_or_else(|| invalid("forensic recovery bundle row is missing"))?;
        let (has_local_notes, position, commitment, has_no_tx_hash, has_no_keystone_signature) =
            row;
        if !has_local_notes || position.is_some() {
            return Err(invalid(
                "forensic recovery may repair only unconfirmed local bundles",
            ));
        }
        if !has_no_tx_hash {
            return Err(invalid(
                "forensic recovery cannot replace a submitted delegation bundle",
            ));
        }
        if !has_no_keystone_signature {
            return Err(invalid(
                "forensic recovery cannot replace a signed delegation bundle",
            ));
        }
        if let Some(commitment) = commitment {
            let commitment: [u8; 32] = commitment
                .try_into()
                .map_err(|_| invalid("stored replacement VAN commitment must contain 32 bytes"))?;
            if pallas::Base::from_repr(commitment).is_none().into() {
                return Err(invalid(
                    "stored replacement VAN commitment is not canonical",
                ));
            }
            if public_commitments.contains(&commitment) {
                return Err(invalid(
                    "stored replacement delegation already appears in the validated tree",
                ));
            }
        }
    }
    Ok(())
}

fn forensic_bundle_matches(
    conn: &rusqlite::Connection,
    round_id: &str,
    wallet_id: &str,
    bundle: &ValidatedForensicBundle,
) -> Result<bool, VotingError> {
    conn.query_row(
        "SELECT COALESCE(b.note_positions_blob IS NOT NULL
                AND b.dummy_nullifiers IS NULL AND b.rho_signed IS NULL
                AND b.padded_note_data IS NULL AND b.nf_signed IS NULL
                AND b.cmx_new IS NULL AND b.alpha IS NULL
                AND b.rseed_signed IS NULL AND b.rseed_output IS NULL
                AND b.rk IS NULL AND b.gov_nullifiers_blob IS NULL
                AND b.padded_note_secrets IS NULL AND b.pczt_sighash IS NULL
                AND b.tx1_effects IS NULL
                AND b.van_comm_rand = :rand AND b.gov_comm = :van
                AND b.total_note_value = :total AND b.address_index = 0
                AND b.van_leaf_position = :position
                AND b.delegation_tx_hash IS :tx_hash
                AND NOT EXISTS (
                    SELECT 1 FROM proofs p
                    WHERE p.round_id = b.round_id AND p.wallet_id = b.wallet_id
                      AND p.bundle_index = b.bundle_index
                ), 0)
         FROM bundles b
         WHERE b.round_id = :round_id AND b.wallet_id = :wallet_id
           AND b.bundle_index = :bundle_index",
        named_params! {
            ":round_id": round_id,
            ":wallet_id": wallet_id,
            ":bundle_index": i64::from(bundle.index),
            ":rand": bundle.rand,
            ":van": bundle.van,
            ":total": bundle.total_note_value as i64,
            ":position": i64::from(bundle.position),
            ":tx_hash": bundle.tx_hash,
        },
        |row| row.get::<_, i64>(0).map(|value| value == 1),
    )
    .optional()
    .map(|value| value.unwrap_or(false))
    .map_err(|e| internal(format!("validate recovered forensic bundle failed: {e}")))
}

fn replace_recovered_bundle(
    conn: &rusqlite::Connection,
    round_id: &str,
    wallet_id: &str,
    bundle: &ValidatedForensicBundle,
) -> Result<(), VotingError> {
    conn.execute(
        "DELETE FROM proofs
         WHERE round_id = :round_id AND wallet_id = :wallet_id
           AND bundle_index = :bundle_index",
        named_params! {
            ":round_id": round_id,
            ":wallet_id": wallet_id,
            ":bundle_index": i64::from(bundle.index),
        },
    )
    .map_err(|e| {
        internal(format!(
            "remove stale forensic delegation proof failed: {e}"
        ))
    })?;

    let updated = conn
        .execute(
            "UPDATE bundles
         SET van_comm_rand = :rand,
             dummy_nullifiers = NULL,
             rho_signed = NULL,
             padded_note_data = NULL,
             nf_signed = NULL,
             cmx_new = NULL,
             alpha = NULL,
             rseed_signed = NULL,
             rseed_output = NULL,
             gov_comm = :van,
             total_note_value = :total,
             address_index = 0,
             van_leaf_position = :position,
             rk = NULL,
             gov_nullifiers_blob = NULL,
             padded_note_secrets = NULL,
             pczt_sighash = NULL,
             tx1_effects = NULL,
             delegation_tx_hash = :tx_hash
         WHERE round_id = :round_id AND wallet_id = :wallet_id
           AND bundle_index = :bundle_index",
            named_params! {
                ":round_id": round_id,
                ":wallet_id": wallet_id,
                ":bundle_index": i64::from(bundle.index),
                ":rand": bundle.rand,
                ":van": bundle.van,
                ":total": bundle.total_note_value as i64,
                ":position": i64::from(bundle.position),
                ":tx_hash": bundle.tx_hash,
            },
        )
        .map_err(|e| internal(format!("update recovered forensic bundle failed: {e}")))?;
    if updated != 1 {
        return Err(internal(
            "forensic recovery did not update exactly one local bundle row",
        ));
    }
    Ok(())
}

fn validate_hash(hash: &str) -> Result<(), VotingError> {
    let decoded = hex::decode(hash)
        .map_err(|_| invalid("delegation_tx_hash must be 64 lowercase hex characters"))?;
    if decoded.len() != 32 || hex::encode(decoded) != hash {
        return Err(invalid(
            "delegation_tx_hash must be 64 lowercase hex characters",
        ));
    }
    Ok(())
}

fn recovery_result(
    snapshot: &VerifiedVoteTreeSnapshot,
    bundles: &[ValidatedForensicBundle],
    already_recovered: bool,
) -> ForensicDelegationRecovery {
    ForensicDelegationRecovery {
        anchor_height: snapshot.anchor_height,
        tree_root: snapshot.root,
        bundle_count: bundles.len() as u32,
        recovered_bundle_indices: bundles.iter().map(|bundle| bundle.index).collect(),
        already_recovered,
    }
}

fn invalid(message: impl Into<String>) -> VotingError {
    VotingError::InvalidInput {
        message: message.into(),
    }
}

fn internal(message: impl Into<String>) -> VotingError {
    VotingError::Internal {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        hotkey::generate_random_voting_hotkey, session::Decision,
        tree_sync::verified_vote_tree_snapshot_with_api,
    };
    use base64::prelude::*;
    use pasta_curves::Fp;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };
    use vote_commitment_tree::{MemoryTreeServer, MerkleHashVote};

    const ROUND_ID: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    const WALLET_ID: &str = "forensic-recovery-wallet";

    type StoredBundleRow = (
        Option<Vec<u8>>,
        Vec<u8>,
        Vec<u8>,
        u64,
        u32,
        u32,
        Option<String>,
    );

    struct Fixture {
        db: VotingDb,
        params: VotingRoundParams,
        hotkey: VotingHotkey,
        bundles: Vec<ForensicDelegationBundle>,
        snapshot: VerifiedVoteTreeSnapshot,
    }

    impl Fixture {
        fn recover(
            &self,
            bundles: &[ForensicDelegationBundle],
        ) -> Result<ForensicDelegationRecovery, VotingError> {
            let params = RecoverDelegationFromForensicEvidenceParams {
                voting_hotkey: &self.hotkey,
                expected_network: Network::Testnet,
                expected_round_params: &self.params,
                node_url: "unused-in-unit-test",
                bundles,
            };
            validate_context(&params)?;
            let validated = validate_bundles(&params)?;
            recover_with_verified_snapshot(&self.db, &params, &validated, &self.snapshot)
        }
    }

    #[test]
    fn restores_complete_batch_and_preserves_ballot_intent() {
        let fixture = fixture();
        fixture
            .db
            .set_ballot_intent(ROUND_ID, 2, Decision::Choice(1), 3)
            .unwrap();
        fixture
            .db
            .conn()
            .execute(
                "INSERT INTO proofs
                 (round_id, wallet_id, bundle_index, proof, success, created_at)
                 VALUES (?1, ?2, 0, X'01', 1, 1)",
                rusqlite::params![ROUND_ID, WALLET_ID],
            )
            .unwrap();

        // This is the exact cleanup that stranded affected pre-3.10.2 rounds:
        // the local bundle rows survive, but their unsigned delegation fields
        // (including the VAN randomness and commitment) are cleared.
        fixture
            .db
            .clear_unsigned_delegation_setup_fields(ROUND_ID)
            .unwrap();

        let result = fixture.recover(&fixture.bundles).unwrap();

        assert!(!result.already_recovered);
        assert_eq!(result.bundle_count, 3);
        assert_eq!(result.recovered_bundle_indices, vec![0, 1, 2]);
        assert_eq!(result.anchor_height, fixture.snapshot.anchor_height);
        assert_eq!(result.tree_root, fixture.snapshot.root);
        assert_eq!(
            fixture.db.get_round_state(ROUND_ID).unwrap().phase,
            RoundPhase::DelegationProved
        );
        assert!(
            fixture
                .db
                .get_round_state(ROUND_ID)
                .unwrap()
                .proof_generated
        );
        assert_eq!(
            fixture.db.ballot_intents(ROUND_ID).unwrap(),
            vec![(2, Decision::Choice(1))]
        );
        let plan = crate::session::resume_plan(&fixture.db, ROUND_ID, &[2]).unwrap();
        assert_eq!(
            plan.next_steps
                .iter()
                .filter(|step| matches!(step, crate::session::NextStep::CastVote { .. }))
                .count(),
            3
        );
        assert!(!plan.next_steps.iter().any(|step| matches!(
            step,
            crate::session::NextStep::Delegate { .. }
                | crate::session::NextStep::PollDelegation { .. }
        )));

        let conn = fixture.db.conn();
        let proof_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM proofs WHERE round_id = ?1 AND wallet_id = ?2",
                rusqlite::params![ROUND_ID, WALLET_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(proof_count, 0);
        for expected in &fixture.bundles {
            let stored: StoredBundleRow = conn
                .query_row(
                    "SELECT note_positions_blob, van_comm_rand, gov_comm,
                            total_note_value, address_index, van_leaf_position,
                            delegation_tx_hash
                     FROM bundles
                     WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = ?3",
                    rusqlite::params![ROUND_ID, WALLET_ID, expected.bundle_index],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                        ))
                    },
                )
                .unwrap();
            assert!(stored.0.is_some());
            assert_eq!(stored.1, expected.van_comm_rand);
            assert_eq!(stored.2, expected.van_commitment);
            assert_eq!(stored.3, expected.total_note_value);
            assert_eq!(stored.4, expected.address_index);
            assert_eq!(stored.5, expected.van_leaf_position);
            assert_eq!(stored.6, expected.delegation_tx_hash);
        }
    }

    #[test]
    fn accepts_raw_bundle_weights_with_sub_ballot_remainders() {
        let fixture = fixture();
        let params = RecoverDelegationFromForensicEvidenceParams {
            voting_hotkey: &fixture.hotkey,
            expected_network: Network::Testnet,
            expected_round_params: &fixture.params,
            node_url: "unused-in-unit-test",
            bundles: &fixture.bundles,
        };

        let validated = validate_bundles(&params).unwrap();

        assert_eq!(
            validated
                .iter()
                .map(|bundle| bundle.total_note_value)
                .collect::<Vec<_>>(),
            vec![130_000_000, 130_000_000, 26_000_000]
        );
        assert!(validated
            .iter()
            .all(|bundle| bundle.total_note_value % BALLOT_DIVISOR != 0));
    }

    #[test]
    fn public_entrypoint_refetches_the_tree_and_restores_post_wipe_state() {
        let fixture = fixture();
        fixture
            .db
            .set_ballot_intent(ROUND_ID, 2, Decision::Choice(1), 3)
            .unwrap();
        fixture
            .db
            .clear_unsigned_delegation_setup_fields(ROUND_ID)
            .unwrap();
        let node_url = start_tree_http_server(&fixture.snapshot);

        let result = recover_delegation_from_forensic_evidence(
            &fixture.db,
            RecoverDelegationFromForensicEvidenceParams {
                voting_hotkey: &fixture.hotkey,
                expected_network: Network::Testnet,
                expected_round_params: &fixture.params,
                node_url: &node_url,
                bundles: &fixture.bundles,
            },
        )
        .unwrap();

        assert!(!result.already_recovered);
        assert_eq!(result.recovered_bundle_indices, vec![0, 1, 2]);
        assert_eq!(result.anchor_height, fixture.snapshot.anchor_height);
        assert_eq!(result.tree_root, fixture.snapshot.root);
        assert!(
            fixture
                .db
                .get_round_state(ROUND_ID)
                .unwrap()
                .proof_generated
        );
        let plan = crate::session::resume_plan(&fixture.db, ROUND_ID, &[2]).unwrap();
        assert_eq!(
            plan.next_steps
                .iter()
                .filter(|step| matches!(step, crate::session::NextStep::CastVote { .. }))
                .count(),
            fixture.bundles.len()
        );
    }

    #[test]
    fn exact_retry_is_a_no_op_even_after_voting_begins() {
        let fixture = fixture();
        fixture.recover(&fixture.bundles).unwrap();
        fixture
            .db
            .conn()
            .execute(
                "INSERT INTO votes
                 (round_id, wallet_id, bundle_index, proposal_id, choice, created_at)
                 VALUES (?1, ?2, 0, 1, 0, 1)",
                rusqlite::params![ROUND_ID, WALLET_ID],
            )
            .unwrap();

        let result = fixture.recover(&fixture.bundles).unwrap();

        assert!(result.already_recovered);
        let vote_count: i64 = fixture
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM votes WHERE round_id = ?1 AND wallet_id = ?2",
                rusqlite::params![ROUND_ID, WALLET_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(vote_count, 1);
    }

    #[test]
    fn restores_on_chain_subset_and_leaves_missing_bundles_retryable() {
        let mut fixture = fixture();
        let mut server = MemoryTreeServer::empty();
        server.append(Fp::from(500)).unwrap();
        server.checkpoint(1).unwrap();
        fixture.bundles[0].van_leaf_position = server.size() as u32;
        let landed_van =
            Option::<Fp>::from(Fp::from_repr(fixture.bundles[0].van_commitment)).unwrap();
        server.append(landed_van).unwrap();
        server.checkpoint(2).unwrap();
        server.append(Fp::from(502)).unwrap();
        server.checkpoint(3).unwrap();
        fixture.snapshot = verified_vote_tree_snapshot_with_api(&server).unwrap();
        fixture
            .db
            .set_ballot_intent(ROUND_ID, 2, Decision::Choice(1), 3)
            .unwrap();
        fixture
            .db
            .clear_unsigned_delegation_setup_fields(ROUND_ID)
            .unwrap();

        let result = fixture.recover(&fixture.bundles[..1]).unwrap();

        assert_eq!(result.bundle_count, 1);
        assert_eq!(result.recovered_bundle_indices, vec![0]);
        assert_eq!(local_bundle_count(&fixture.db), 3);
        let plan = crate::session::resume_plan(&fixture.db, ROUND_ID, &[2]).unwrap();
        let delegation_indices = plan
            .next_steps
            .iter()
            .filter_map(|step| match step {
                crate::session::NextStep::Delegate { bundle_index } => Some(*bundle_index),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(delegation_indices, vec![1, 2]);

        let conn = fixture.db.conn();
        for index in 0..3 {
            let row = conn
                .query_row(
                    "SELECT note_positions_blob IS NOT NULL, van_comm_rand,
                            gov_comm, van_leaf_position
                     FROM bundles
                     WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = ?3",
                    rusqlite::params![ROUND_ID, WALLET_ID, index],
                    |row| {
                        Ok((
                            row.get::<_, bool>(0)?,
                            row.get::<_, Option<Vec<u8>>>(1)?,
                            row.get::<_, Option<Vec<u8>>>(2)?,
                            row.get::<_, Option<u32>>(3)?,
                        ))
                    },
                )
                .unwrap();
            assert!(row.0);
            if index == 0 {
                assert_eq!(row.1, Some(fixture.bundles[0].van_comm_rand.to_vec()));
                assert_eq!(row.2, Some(fixture.bundles[0].van_commitment.to_vec()));
                assert_eq!(row.3, Some(fixture.bundles[0].van_leaf_position));
            } else {
                assert_eq!(row.1, None);
                assert_eq!(row.2, None);
                assert_eq!(row.3, None);
            }
        }
        drop(conn);

        let notes = (0u64..3)
            .map(|index| test_note(index + 50))
            .collect::<Vec<_>>();
        let layout = fixture
            .db
            .ensure_bundles_with_skipped_suffix_with_policy(
                ROUND_ID,
                &notes,
                crate::BundlePolicy::new(1).unwrap(),
            )
            .unwrap();
        assert_eq!(layout.bundle_count, 3);
    }

    #[test]
    fn wrong_randomness_or_tree_position_is_rejected_without_mutation() {
        let fixture = fixture();
        let mut wrong_randomness = fixture.bundles.clone();
        wrong_randomness[0].van_comm_rand = Fp::from(999).to_repr();
        assert!(fixture.recover(&wrong_randomness).is_err());
        assert_eq!(local_bundle_count(&fixture.db), 3);

        let mut wrong_position = fixture.bundles.clone();
        wrong_position[0].van_leaf_position += 1;
        assert!(fixture.recover(&wrong_position).is_err());
        assert_eq!(local_bundle_count(&fixture.db), 3);
    }

    #[test]
    fn subset_indices_must_be_sorted_and_inside_the_stored_batch() {
        let fixture = fixture();
        let unsorted = [fixture.bundles[1].clone(), fixture.bundles[0].clone()];
        let error = fixture.recover(&unsorted).unwrap_err();
        assert!(error.to_string().contains("strictly increasing"), "{error}");

        let mut out_of_range = fixture.bundles[0].clone();
        out_of_range.bundle_index = 3;
        let error = fixture.recover(&[out_of_range]).unwrap_err();
        assert!(error.to_string().contains("outside the stored"), "{error}");
        assert_eq!(local_bundle_count(&fixture.db), 3);
    }

    #[test]
    fn replacement_that_already_appears_on_chain_is_rejected() {
        let fixture = fixture();
        fixture
            .db
            .conn()
            .execute(
                "UPDATE bundles SET gov_comm = ?1
                 WHERE round_id = ?2 AND wallet_id = ?3 AND bundle_index = 0",
                rusqlite::params![fixture.bundles[0].van_commitment, ROUND_ID, WALLET_ID],
            )
            .unwrap();

        let error = fixture
            .recover(&fixture.bundles)
            .expect_err("an on-chain replacement cannot be discarded");

        assert!(error.to_string().contains("already appears"), "{error}");
        assert_eq!(local_bundle_count(&fixture.db), 3);
    }

    #[test]
    fn submitted_replacement_is_rejected_without_mutation() {
        let fixture = fixture();
        let tx_hash = "22".repeat(32);
        fixture
            .db
            .store_delegation_tx_hash(ROUND_ID, 0, &tx_hash)
            .unwrap();

        let error = fixture
            .recover(&fixture.bundles)
            .expect_err("a submitted delegation cannot be discarded");

        assert!(error.to_string().contains("submitted"), "{error}");
        assert_eq!(
            fixture.db.get_delegation_tx_hash(ROUND_ID, 0).unwrap(),
            Some(tx_hash)
        );
        assert_eq!(local_bundle_count(&fixture.db), 3);
    }

    #[test]
    fn signed_replacement_is_rejected_without_mutation() {
        let fixture = fixture();
        fixture
            .db
            .store_keystone_signature(ROUND_ID, 0, &[0x11; 64], &[0x22; 32], &[0x33; 32])
            .unwrap();

        let error = fixture
            .recover(&fixture.bundles)
            .expect_err("a signed delegation cannot be discarded");

        assert!(error.to_string().contains("signed"), "{error}");
        let signatures = fixture.db.get_keystone_signatures(ROUND_ID).unwrap();
        assert_eq!(signatures.len(), 1);
        assert_eq!(signatures[0].bundle_index, 0);
        assert_eq!(local_bundle_count(&fixture.db), 3);
    }

    #[test]
    fn conflicting_recovery_after_voting_begins_is_rejected() {
        let fixture = fixture();
        fixture
            .db
            .conn()
            .execute(
                "INSERT INTO votes
                 (round_id, wallet_id, bundle_index, proposal_id, choice, created_at)
                 VALUES (?1, ?2, 0, 1, 0, 1)",
                rusqlite::params![ROUND_ID, WALLET_ID],
            )
            .unwrap();

        let error = fixture
            .recover(&fixture.bundles)
            .expect_err("recovery must not cascade-delete vote state");

        assert!(error.to_string().contains("after voting began"), "{error}");
        assert_eq!(local_bundle_count(&fixture.db), 3);
    }

    #[test]
    fn vote_ready_round_is_rejected_without_mutation() {
        let fixture = fixture();
        fixture
            .db
            .conn()
            .execute(
                "UPDATE rounds SET phase = ?1 WHERE round_id = ?2 AND wallet_id = ?3",
                rusqlite::params![RoundPhase::VoteReady as i32, ROUND_ID, WALLET_ID],
            )
            .unwrap();

        let error = fixture
            .recover(&fixture.bundles)
            .expect_err("a vote-ready round must not be replaced");

        assert!(error.to_string().contains("vote-ready"), "{error}");
        assert_eq!(local_bundle_count(&fixture.db), 3);
    }

    #[test]
    fn subset_update_failure_rolls_back_every_recovered_bundle() {
        let fixture = fixture();
        fixture
            .db
            .conn()
            .execute_batch(
                "CREATE TRIGGER fail_second_forensic_bundle
                 BEFORE UPDATE OF van_comm_rand ON bundles
                 WHEN NEW.bundle_index = 1
                 BEGIN
                     SELECT RAISE(ABORT, 'injected forensic update failure');
                 END;",
            )
            .unwrap();

        let error = fixture
            .recover(&fixture.bundles)
            .expect_err("the injected second update must abort the transaction");

        assert!(error
            .to_string()
            .contains("injected forensic update failure"));
        assert_eq!(local_bundle_count(&fixture.db), 3);
    }

    fn fixture() -> Fixture {
        let params = VotingRoundParams {
            vote_round_id: ROUND_ID.to_string(),
            snapshot_height: 100,
            ea_pk: vec![2; 32],
            nc_root: vec![3; 32],
            nullifier_imt_root: vec![4; 32],
        };
        let hotkey = generate_random_voting_hotkey(Network::Testnet).unwrap();
        let (g_d_x, pk_d_x) =
            derive_hotkey_x_coords_from_raw_address(hotkey.raw_orchard_address()).unwrap();
        let round_id: [u8; 32] = hex::decode(ROUND_ID).unwrap().try_into().unwrap();
        let raw_bundle_weights = [130_000_000, 130_000_000, 26_000_000];
        let mut bundles = raw_bundle_weights
            .into_iter()
            .enumerate()
            .map(|(index, total_note_value)| {
                let index = index as u32;
                let rand = Fp::from(u64::from(index + 10)).to_repr();
                let van_commitment =
                    construct_van(&g_d_x, &pk_d_x, total_note_value, &round_id, &rand)
                        .unwrap()
                        .try_into()
                        .unwrap();
                ForensicDelegationBundle {
                    bundle_index: index,
                    total_note_value,
                    address_index: hotkey.address_index(),
                    van_comm_rand: rand,
                    van_commitment,
                    van_leaf_position: 0,
                    delegation_tx_hash: (index == 0).then(|| "11".repeat(32)),
                }
            })
            .collect::<Vec<_>>();

        let mut server = MemoryTreeServer::empty();
        let mut height = 1;
        server.append(Fp::from(500)).unwrap();
        server.checkpoint(height).unwrap();
        for bundle in &mut bundles {
            height += 1;
            bundle.van_leaf_position = server.size() as u32;
            let van = Option::<Fp>::from(Fp::from_repr(bundle.van_commitment)).unwrap();
            server.append(van).unwrap();
            server.checkpoint(height).unwrap();
            height += 1;
            server.append(Fp::from(u64::from(height + 500))).unwrap();
            server.checkpoint(height).unwrap();
        }
        let snapshot = verified_vote_tree_snapshot_with_api(&server).unwrap();

        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(WALLET_ID);
        db.create_round(Network::Testnet, &params, None).unwrap();
        for index in 0..bundles.len() as u32 {
            let conn = db.conn();
            queries::insert_bundle(&conn, ROUND_ID, WALLET_ID, index, &[u64::from(index) + 50])
                .unwrap();
            conn.execute(
                "UPDATE bundles
                 SET van_comm_rand = ?1, gov_comm = ?2,
                     total_note_value = ?3, address_index = 0
                 WHERE round_id = ?4 AND wallet_id = ?5 AND bundle_index = ?6",
                rusqlite::params![
                    Fp::from(u64::from(index + 100)).to_repr(),
                    Fp::from(u64::from(index + 200)).to_repr(),
                    BALLOT_DIVISOR,
                    ROUND_ID,
                    WALLET_ID,
                    index,
                ],
            )
            .unwrap();
        }

        Fixture {
            db,
            params,
            hotkey,
            bundles,
            snapshot,
        }
    }

    fn local_bundle_count(db: &VotingDb) -> i64 {
        db.conn()
            .query_row(
                "SELECT COUNT(*) FROM bundles
                 WHERE round_id = ?1 AND wallet_id = ?2
                   AND note_positions_blob IS NOT NULL",
                rusqlite::params![ROUND_ID, WALLET_ID],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn test_note(position: u64) -> crate::NoteInfo {
        crate::NoteInfo {
            commitment: vec![position as u8; 32],
            nullifier: vec![position as u8 + 1; 32],
            value: BALLOT_DIVISOR,
            position,
            diversifier: vec![0x03; 11],
            rho: vec![0x04; 32],
            rseed: vec![0x05; 32],
            scope: 0,
            ufvk_str: "uview1test".to_string(),
        }
    }

    fn start_tree_http_server(snapshot: &VerifiedVoteTreeSnapshot) -> String {
        let mut tree = MemoryTreeServer::empty();
        let mut blocks = Vec::with_capacity(snapshot.leaves.len());
        for leaf in &snapshot.leaves {
            let value = Option::<Fp>::from(Fp::from_repr(leaf.commitment)).unwrap();
            let height = leaf.position + 1;
            tree.append(value).unwrap();
            tree.checkpoint(height).unwrap();
            blocks.push(serde_json::json!({
                "height": height,
                "start_index": leaf.position,
                "leaves": [BASE64_STANDARD.encode(MerkleHashVote::from_fp(value).to_bytes())],
                "root": BASE64_STANDARD.encode(
                    MerkleHashVote::from_fp(tree.root_at_height(height).unwrap()).to_bytes()
                )
            }));
        }
        assert_eq!(tree.root().to_repr(), snapshot.root);

        let latest = serde_json::json!({
            "tree": {
                "next_index": snapshot.leaves.len(),
                "root": BASE64_STANDARD.encode(snapshot.root),
                "height": snapshot.anchor_height
            }
        })
        .to_string();
        let leaves = serde_json::json!({
            "blocks": blocks,
            "next_from_height": 0
        })
        .to_string();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 2048];
                let length = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..length]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");
                let body = if path.ends_with("/latest") {
                    &latest
                } else if path.contains("/leaves?") {
                    &leaves
                } else {
                    panic!("unexpected vote-tree request: {path}");
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        url
    }
}
