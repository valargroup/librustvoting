//! Stable cast-vote lifecycle API.
//!
//! This module owns the wallet-facing vote flow: build ZKP #2 for one
//! proposal, sign the cast-vote payload, persist crash-recovery material, and
//! reconstruct chain-ready submission fields.

use serde::{Deserialize, Serialize};

use crate::{
    round::VotingDb,
    types::{
        EncryptedShare, Network, ProgressReporter, SharePayload, VoteCommitmentBundle, VotingError,
        WireEncryptedShare,
    },
};

/// Number of siblings in a vote-authority-note witness.
pub const VAN_AUTH_PATH_LEN: usize = 24;

const VOTE_RECOVERY_FORMAT: &str = "zcash_voting_vote_recovery_v1";

/// Wallet-supplied cast-vote intent for one proposal in one bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftVote {
    pub proposal_id: u32,
    pub choice: u32,
    pub num_options: u32,
    pub single_share: bool,
    pub vc_tree_position: u64,
}

/// VAN Merkle witness produced by `precompute::van_witness`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VanWitness {
    pub auth_path: [[u8; 32]; VAN_AUTH_PATH_LEN],
    pub position: u32,
    pub anchor_height: u32,
}

/// Result of building, signing, and persisting one cast-vote.
#[derive(Clone, Debug)]
pub struct VoteCommit {
    pub proposal_id: u32,
    pub van_nullifier: [u8; 32],
    pub vote_authority_note_new: [u8; 32],
    pub vote_commitment: [u8; 32],
    pub proof: Vec<u8>,
    pub anchor_height: u32,
    pub r_vpk: [u8; 32],
    pub vote_auth_sig: [u8; 64],
    pub encrypted_shares: Vec<WireEncryptedShare>,
    pub share_payloads: Vec<SharePayload>,
}

/// Cast-vote signing source.
#[non_exhaustive]
pub enum VoteSigner<'a> {
    Seed {
        seed: &'a [u8],
        network: Network,
        account_index: u32,
    },
}

/// Chain-ready cast-vote submission fields for the vote chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoteSubmission {
    pub vote_round_id: String,
    pub proposal_id: u32,
    pub van_nullifier: [u8; 32],
    pub vote_authority_note_new: [u8; 32],
    pub vote_commitment: [u8; 32],
    pub proof: Vec<u8>,
    pub r_vpk: [u8; 32],
    pub vote_auth_sig: [u8; 64],
    pub anchor_height: u32,
}

/// Library-owned vote recovery material persisted after `commit`.
#[derive(Clone, Debug)]
pub struct VoteRecoveryBundle {
    pub vote_round_id: String,
    pub bundle_index: u32,
    pub proposal_id: u32,
    pub vote_decision: u32,
    pub anchor_height: u32,
    pub vc_tree_position: u64,
    pub single_share: bool,
    pub num_options: u32,
    pub van_nullifier: [u8; 32],
    pub vote_authority_note_new: [u8; 32],
    pub vote_commitment: [u8; 32],
    pub proof: Vec<u8>,
    pub shares_hash: [u8; 32],
    pub r_vpk: [u8; 32],
    pub alpha_v: [u8; 32],
    pub vote_auth_sig: [u8; 64],
    /// Secret local share recovery material. Do not send this struct over the network.
    pub encrypted_shares: Vec<EncryptedShare>,
    pub share_blinds: Vec<[u8; 32]>,
    pub share_comms: Vec<[u8; 32]>,
}

#[derive(Serialize, Deserialize)]
struct VoteRecoveryJson {
    format: String,
    vote_round_id: String,
    bundle_index: u32,
    proposal_id: u32,
    vote_decision: u32,
    anchor_height: u32,
    vc_tree_position: u64,
    single_share: bool,
    num_options: u32,
    van_nullifier: Vec<u8>,
    vote_authority_note_new: Vec<u8>,
    vote_commitment: Vec<u8>,
    proof: Vec<u8>,
    shares_hash: Vec<u8>,
    r_vpk: Vec<u8>,
    alpha_v: Vec<u8>,
    vote_auth_sig: Vec<u8>,
    encrypted_shares: Vec<EncryptedShareJson>,
    share_blinds: Vec<Vec<u8>>,
    share_comms: Vec<Vec<u8>>,
}

#[derive(Serialize, Deserialize)]
struct EncryptedShareJson {
    c1: Vec<u8>,
    c2: Vec<u8>,
    share_index: u32,
    plaintext_value: u64,
    randomness: Vec<u8>,
}

/// Build ZKP #2, sign cast-vote, build helper-share payloads, and persist recovery state.
///
/// Repeated calls for the same `(round_id, bundle_index, proposal_id)` return
/// the persisted recovery bundle without rebuilding the proof.
pub fn commit(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    draft: &DraftVote,
    witness: &VanWitness,
    signer: VoteSigner<'_>,
    progress: &dyn ProgressReporter,
) -> Result<VoteCommit, VotingError> {
    if let Some(recovered) = recovery_bundle(db, round_id, bundle_index, draft.proposal_id)? {
        return commit_from_recovery(&recovered);
    }

    let (seed, network, account_index) = match signer {
        VoteSigner::Seed {
            seed,
            network,
            account_index,
        } => (seed, network, account_index),
    };
    let bundle = db.build_vote_commitment(
        round_id,
        bundle_index,
        seed,
        network.id(),
        draft.proposal_id,
        draft.choice,
        draft.num_options,
        &witness.auth_path,
        witness.position,
        witness.anchor_height,
        draft.single_share,
        progress,
    )?;
    let wire_shares = bundle
        .enc_shares
        .iter()
        .map(WireEncryptedShare::from)
        .collect::<Vec<_>>();
    let share_payloads = db.build_share_payloads(
        &wire_shares,
        &bundle,
        draft.choice,
        draft.num_options,
        draft.vc_tree_position,
        draft.single_share,
    )?;
    let signature = crate::vote_commitment::sign_cast_vote_for_account(
        seed,
        network.id(),
        account_index,
        &bundle.vote_round_id,
        &bundle.r_vpk_bytes,
        &bundle.van_nullifier,
        &bundle.vote_authority_note_new,
        &bundle.vote_commitment,
        bundle.proposal_id,
        bundle.anchor_height,
        &bundle.alpha_v,
    )?;
    let vote_auth_sig = array64("vote_auth_sig", signature.vote_auth_sig)?;
    let recovery = VoteRecoveryBundle::from_parts(bundle_index, draft, bundle, vote_auth_sig)?;
    store_recovery_json(
        db,
        round_id,
        bundle_index,
        draft.proposal_id,
        &serialize_recovery(&recovery)?,
        draft.vc_tree_position,
    )?;

    Ok(VoteCommit {
        proposal_id: draft.proposal_id,
        van_nullifier: recovery.van_nullifier,
        vote_authority_note_new: recovery.vote_authority_note_new,
        vote_commitment: recovery.vote_commitment,
        proof: recovery.proof,
        anchor_height: recovery.anchor_height,
        r_vpk: recovery.r_vpk,
        vote_auth_sig: recovery.vote_auth_sig,
        encrypted_shares: wire_shares,
        share_payloads,
    })
}

/// Reconstructs chain-ready cast-vote fields from persisted recovery state.
pub fn submission(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    proposal_id: u32,
) -> Result<VoteSubmission, VotingError> {
    recovery_bundle(db, round_id, bundle_index, proposal_id)?
        .ok_or_else(|| VotingError::InvalidInput {
            message: format!(
                "vote recovery bundle not found for round={round_id}, bundle={bundle_index}, proposal={proposal_id}"
            ),
        })
        .map(|bundle| VoteSubmission {
            vote_round_id: bundle.vote_round_id,
            proposal_id: bundle.proposal_id,
            van_nullifier: bundle.van_nullifier,
            vote_authority_note_new: bundle.vote_authority_note_new,
            vote_commitment: bundle.vote_commitment,
            proof: bundle.proof,
            r_vpk: bundle.r_vpk,
            vote_auth_sig: bundle.vote_auth_sig,
            anchor_height: bundle.anchor_height,
        })
}

/// Records the cast-vote transaction hash and marks the vote submitted.
pub fn record_submission(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    proposal_id: u32,
    tx_hash: &str,
) -> Result<(), VotingError> {
    db.store_vote_tx_hash(round_id, bundle_index, proposal_id, tx_hash)?;
    db.mark_vote_submitted(round_id, bundle_index, proposal_id)
}

/// Records the on-chain vote commitment tree position after confirmation.
pub fn record_vc_position(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    proposal_id: u32,
    vc_tree_position: u64,
) -> Result<(), VotingError> {
    let conn = db.conn();
    let wallet_id = db.wallet_id();
    let rows = conn
        .execute(
            "UPDATE votes SET vc_tree_position = :pos
             WHERE round_id = :round_id AND wallet_id = :wallet_id
               AND bundle_index = :bundle_index AND proposal_id = :proposal_id",
            rusqlite::named_params! {
                ":pos": i64::try_from(vc_tree_position).map_err(|_| VotingError::InvalidInput {
                    message: format!("vc_tree_position {vc_tree_position} does not fit in SQLite i64"),
                })?,
                ":round_id": round_id,
                ":wallet_id": wallet_id,
                ":bundle_index": bundle_index as i64,
                ":proposal_id": proposal_id as i64,
            },
        )
        .map_err(|e| VotingError::Internal {
            message: format!("failed to record vote commitment tree position: {e}"),
        })?;
    if rows == 0 {
        return Err(VotingError::InvalidInput {
            message: format!(
                "vote not found for round={round_id}, bundle={bundle_index}, proposal={proposal_id}"
            ),
        });
    }
    Ok(())
}

/// Loads and parses the persisted vote recovery bundle, if present.
pub fn recovery_bundle(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    proposal_id: u32,
) -> Result<Option<VoteRecoveryBundle>, VotingError> {
    let conn = db.conn();
    let wallet_id = db.wallet_id();
    let json: Option<String> = conn
        .query_row(
            "SELECT commitment_bundle_json FROM votes
             WHERE round_id = :round_id AND wallet_id = :wallet_id
               AND bundle_index = :bundle_index AND proposal_id = :proposal_id",
            rusqlite::named_params! {
                ":round_id": round_id,
                ":wallet_id": wallet_id,
                ":bundle_index": bundle_index as i64,
                ":proposal_id": proposal_id as i64,
            },
            |row| row.get(0),
        )
        .map_err(|e| VotingError::Internal {
            message: format!("failed to load vote recovery bundle: {e}"),
        })?;
    json.as_deref().map(parse_recovery).transpose()
}

/// Serializes a recovery bundle using the library-owned JSON format.
pub fn serialize_recovery(bundle: &VoteRecoveryBundle) -> Result<String, VotingError> {
    serde_json::to_string(&VoteRecoveryJson::from(bundle)).map_err(|e| VotingError::Internal {
        message: format!("failed to serialize vote recovery bundle: {e}"),
    })
}

/// Parses a recovery bundle from the library-owned JSON format.
pub fn parse_recovery(json: &str) -> Result<VoteRecoveryBundle, VotingError> {
    let parsed: VoteRecoveryJson =
        serde_json::from_str(json).map_err(|e| VotingError::InvalidInput {
            message: format!("invalid vote recovery JSON: {e}"),
        })?;
    if parsed.format != VOTE_RECOVERY_FORMAT {
        return Err(VotingError::InvalidInput {
            message: format!("unsupported vote recovery format: {}", parsed.format),
        });
    }
    VoteRecoveryBundle::try_from(parsed)
}

fn store_recovery_json(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    proposal_id: u32,
    json: &str,
    vc_tree_position: u64,
) -> Result<(), VotingError> {
    let conn = db.conn();
    let wallet_id = db.wallet_id();
    let rows = conn
        .execute(
            "UPDATE votes SET commitment_bundle_json = :json, vc_tree_position = :pos
             WHERE round_id = :round_id AND wallet_id = :wallet_id
               AND bundle_index = :bundle_index AND proposal_id = :proposal_id",
            rusqlite::named_params! {
                ":json": json,
                ":pos": i64::try_from(vc_tree_position).map_err(|_| VotingError::InvalidInput {
                    message: format!("vc_tree_position {vc_tree_position} does not fit in SQLite i64"),
                })?,
                ":round_id": round_id,
                ":wallet_id": wallet_id,
                ":bundle_index": bundle_index as i64,
                ":proposal_id": proposal_id as i64,
            },
        )
        .map_err(|e| VotingError::Internal {
            message: format!("failed to store vote recovery bundle: {e}"),
        })?;
    if rows == 0 {
        return Err(VotingError::InvalidInput {
            message: format!(
                "vote not found for round={round_id}, bundle={bundle_index}, proposal={proposal_id}"
            ),
        });
    }
    Ok(())
}

fn commit_from_recovery(bundle: &VoteRecoveryBundle) -> Result<VoteCommit, VotingError> {
    let wire_shares = bundle
        .encrypted_shares
        .iter()
        .map(WireEncryptedShare::from)
        .collect::<Vec<_>>();
    let share_payloads = crate::share::recover_payloads(bundle)?;
    Ok(VoteCommit {
        proposal_id: bundle.proposal_id,
        van_nullifier: bundle.van_nullifier,
        vote_authority_note_new: bundle.vote_authority_note_new,
        vote_commitment: bundle.vote_commitment,
        proof: bundle.proof.clone(),
        anchor_height: bundle.anchor_height,
        r_vpk: bundle.r_vpk,
        vote_auth_sig: bundle.vote_auth_sig,
        encrypted_shares: wire_shares,
        share_payloads,
    })
}

impl VoteRecoveryBundle {
    fn from_parts(
        bundle_index: u32,
        draft: &DraftVote,
        bundle: VoteCommitmentBundle,
        vote_auth_sig: [u8; 64],
    ) -> Result<Self, VotingError> {
        Ok(Self {
            vote_round_id: bundle.vote_round_id,
            bundle_index,
            proposal_id: bundle.proposal_id,
            vote_decision: draft.choice,
            anchor_height: bundle.anchor_height,
            vc_tree_position: draft.vc_tree_position,
            single_share: draft.single_share,
            num_options: draft.num_options,
            van_nullifier: array32("van_nullifier", bundle.van_nullifier)?,
            vote_authority_note_new: array32(
                "vote_authority_note_new",
                bundle.vote_authority_note_new,
            )?,
            vote_commitment: array32("vote_commitment", bundle.vote_commitment)?,
            proof: bundle.proof,
            shares_hash: array32("shares_hash", bundle.shares_hash)?,
            r_vpk: array32("r_vpk", bundle.r_vpk_bytes)?,
            alpha_v: array32("alpha_v", bundle.alpha_v)?,
            vote_auth_sig,
            encrypted_shares: bundle.enc_shares,
            share_blinds: array32_vec("share_blinds", bundle.share_blinds)?,
            share_comms: array32_vec("share_comms", bundle.share_comms)?,
        })
    }
}

impl From<&VoteRecoveryBundle> for VoteRecoveryJson {
    fn from(bundle: &VoteRecoveryBundle) -> Self {
        Self {
            format: VOTE_RECOVERY_FORMAT.to_string(),
            vote_round_id: bundle.vote_round_id.clone(),
            bundle_index: bundle.bundle_index,
            proposal_id: bundle.proposal_id,
            vote_decision: bundle.vote_decision,
            anchor_height: bundle.anchor_height,
            vc_tree_position: bundle.vc_tree_position,
            single_share: bundle.single_share,
            num_options: bundle.num_options,
            van_nullifier: bundle.van_nullifier.to_vec(),
            vote_authority_note_new: bundle.vote_authority_note_new.to_vec(),
            vote_commitment: bundle.vote_commitment.to_vec(),
            proof: bundle.proof.clone(),
            shares_hash: bundle.shares_hash.to_vec(),
            r_vpk: bundle.r_vpk.to_vec(),
            alpha_v: bundle.alpha_v.to_vec(),
            vote_auth_sig: bundle.vote_auth_sig.to_vec(),
            encrypted_shares: bundle
                .encrypted_shares
                .iter()
                .map(EncryptedShareJson::from)
                .collect(),
            share_blinds: bundle.share_blinds.iter().map(|v| v.to_vec()).collect(),
            share_comms: bundle.share_comms.iter().map(|v| v.to_vec()).collect(),
        }
    }
}

impl TryFrom<VoteRecoveryJson> for VoteRecoveryBundle {
    type Error = VotingError;

    fn try_from(value: VoteRecoveryJson) -> Result<Self, Self::Error> {
        Ok(Self {
            vote_round_id: value.vote_round_id,
            bundle_index: value.bundle_index,
            proposal_id: value.proposal_id,
            vote_decision: value.vote_decision,
            anchor_height: value.anchor_height,
            vc_tree_position: value.vc_tree_position,
            single_share: value.single_share,
            num_options: value.num_options,
            van_nullifier: array32("van_nullifier", value.van_nullifier)?,
            vote_authority_note_new: array32(
                "vote_authority_note_new",
                value.vote_authority_note_new,
            )?,
            vote_commitment: array32("vote_commitment", value.vote_commitment)?,
            proof: value.proof,
            shares_hash: array32("shares_hash", value.shares_hash)?,
            r_vpk: array32("r_vpk", value.r_vpk)?,
            alpha_v: array32("alpha_v", value.alpha_v)?,
            vote_auth_sig: array64("vote_auth_sig", value.vote_auth_sig)?,
            encrypted_shares: value
                .encrypted_shares
                .into_iter()
                .map(EncryptedShare::from)
                .collect(),
            share_blinds: array32_vec("share_blinds", value.share_blinds)?,
            share_comms: array32_vec("share_comms", value.share_comms)?,
        })
    }
}

impl From<&EncryptedShare> for EncryptedShareJson {
    fn from(share: &EncryptedShare) -> Self {
        Self {
            c1: share.c1.clone(),
            c2: share.c2.clone(),
            share_index: share.share_index,
            plaintext_value: share.plaintext_value,
            randomness: share.randomness.clone(),
        }
    }
}

impl From<EncryptedShareJson> for EncryptedShare {
    fn from(value: EncryptedShareJson) -> Self {
        Self {
            c1: value.c1,
            c2: value.c2,
            share_index: value.share_index,
            plaintext_value: value.plaintext_value,
            randomness: value.randomness,
        }
    }
}

fn array32(label: &str, value: Vec<u8>) -> Result<[u8; 32], VotingError> {
    value
        .try_into()
        .map_err(|value: Vec<u8>| VotingError::Internal {
            message: format!("{label} must be 32 bytes, got {}", value.len()),
        })
}

fn array64(label: &str, value: Vec<u8>) -> Result<[u8; 64], VotingError> {
    value
        .try_into()
        .map_err(|value: Vec<u8>| VotingError::Internal {
            message: format!("{label} must be 64 bytes, got {}", value.len()),
        })
}

fn array32_vec(label: &str, values: Vec<Vec<u8>>) -> Result<Vec<[u8; 32]>, VotingError> {
    values
        .into_iter()
        .enumerate()
        .map(|(idx, value)| array32(&format!("{label}[{idx}]"), value))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        round::RoundParams,
        storage::{queries, VotingDb},
        types::{NoopProgressReporter, NoteInfo},
    };

    const ROUND_ID: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    const WALLET_ID: &str = "wallet";

    fn db_with_vote() -> VotingDb {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(WALLET_ID);
        db.create_round(&round_params()).unwrap();
        db.ensure_bundles(ROUND_ID, &[note(0)]).unwrap();
        queries::store_vote(&db.conn(), ROUND_ID, WALLET_ID, 0, 1, 2, &[0xCA; 32]).unwrap();
        db
    }

    fn round_params() -> RoundParams {
        RoundParams {
            vote_round_id: ROUND_ID.to_string(),
            snapshot_height: 1000,
            ea_pk: vec![0xEA; 32],
            nc_root: vec![0xAA; 32],
            nullifier_imt_root: vec![0xBB; 32],
        }
    }

    fn note(position: u64) -> NoteInfo {
        NoteInfo {
            commitment: vec![0x01; 32],
            nullifier: vec![0x02; 32],
            value: crate::governance::BALLOT_DIVISOR,
            position,
            diversifier: vec![0x03; 11],
            rho: vec![0x04; 32],
            rseed: vec![0x05; 32],
            scope: 0,
            ufvk_str: "uview1test".to_string(),
        }
    }

    fn recovery_bundle_fixture() -> VoteRecoveryBundle {
        VoteRecoveryBundle {
            vote_round_id: ROUND_ID.to_string(),
            bundle_index: 0,
            proposal_id: 1,
            vote_decision: 2,
            anchor_height: 123,
            vc_tree_position: 456,
            single_share: false,
            num_options: 3,
            van_nullifier: [0x10; 32],
            vote_authority_note_new: [0x11; 32],
            vote_commitment: [0x12; 32],
            proof: vec![0x13; 96],
            shares_hash: [0x14; 32],
            r_vpk: [0x15; 32],
            alpha_v: [0x16; 32],
            vote_auth_sig: [0x17; 64],
            encrypted_shares: vec![
                EncryptedShare {
                    c1: vec![0x21; 32],
                    c2: vec![0x22; 32],
                    share_index: 0,
                    plaintext_value: 5,
                    randomness: vec![0x23; 32],
                },
                EncryptedShare {
                    c1: vec![0x31; 32],
                    c2: vec![0x32; 32],
                    share_index: 1,
                    plaintext_value: 6,
                    randomness: vec![0x33; 32],
                },
            ],
            share_blinds: vec![[0x41; 32], [0x42; 32]],
            share_comms: vec![[0x51; 32], [0x52; 32]],
        }
    }

    #[test]
    fn recovery_json_round_trip_preserves_vote_and_share_material() {
        let bundle = recovery_bundle_fixture();

        let json = serialize_recovery(&bundle).unwrap();
        let parsed = parse_recovery(&json).unwrap();

        assert_eq!(parsed.vote_round_id, ROUND_ID);
        assert_eq!(parsed.proposal_id, 1);
        assert_eq!(parsed.vote_auth_sig, [0x17; 64]);
        assert_eq!(parsed.encrypted_shares.len(), 2);
        assert_eq!(parsed.encrypted_shares[0].plaintext_value, 5);
        assert_eq!(parsed.encrypted_shares[0].randomness, vec![0x23; 32]);
        assert_eq!(parsed.share_blinds[1], [0x42; 32]);
        assert_eq!(parsed.share_comms[0], [0x51; 32]);
    }

    #[test]
    fn vote_lifecycle_apis_replay_persisted_recovery_happy_path() {
        let db = db_with_vote();
        let recovery = recovery_bundle_fixture();
        store_recovery_json(
            &db,
            ROUND_ID,
            recovery.bundle_index,
            recovery.proposal_id,
            &serialize_recovery(&recovery).unwrap(),
            recovery.vc_tree_position,
        )
        .unwrap();

        let loaded = recovery_bundle(&db, ROUND_ID, 0, 1).unwrap().unwrap();
        assert_eq!(loaded.vote_commitment, [0x12; 32]);

        let submission = submission(&db, ROUND_ID, 0, 1).unwrap();
        assert_eq!(submission.vote_round_id, ROUND_ID);
        assert_eq!(submission.r_vpk, [0x15; 32]);
        assert_eq!(submission.vote_auth_sig, [0x17; 64]);

        let commit = commit(
            &db,
            ROUND_ID,
            0,
            &DraftVote {
                proposal_id: 1,
                choice: 2,
                num_options: 3,
                single_share: false,
                vc_tree_position: 456,
            },
            &VanWitness {
                auth_path: [[0xAA; 32]; VAN_AUTH_PATH_LEN],
                position: 7,
                anchor_height: 123,
            },
            VoteSigner::Seed {
                seed: &[0x99; 32],
                network: Network::Testnet,
                account_index: 0,
            },
            &NoopProgressReporter,
        )
        .unwrap();
        assert_eq!(commit.vote_commitment, [0x12; 32]);
        assert_eq!(commit.encrypted_shares.len(), 2);
        assert_eq!(commit.share_payloads.len(), 2);

        record_submission(&db, ROUND_ID, 0, 1, "txid").unwrap();
        assert_eq!(
            db.get_vote_tx_hash(ROUND_ID, 0, 1).unwrap().as_deref(),
            Some("txid")
        );

        record_vc_position(&db, ROUND_ID, 0, 1, 789).unwrap();
        let (_, position) = db.get_commitment_bundle(ROUND_ID, 0, 1).unwrap().unwrap();
        assert_eq!(position, 789);
    }
}
