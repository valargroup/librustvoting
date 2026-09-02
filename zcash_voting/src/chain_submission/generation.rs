//! Semantic generation derivation from durable delegation and vote inputs.

use rusqlite::{named_params, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::{
    delegate::DelegationSigner,
    governance::BUNDLE_NOTE_SLOTS,
    storage::queries,
    types::{EncryptedShare, Network, VotingError},
    vote::VoteRecoveryBundle,
    wire::{DelegationSubmissionWire, VoteCommitmentBatchWire, VoteCommitmentWire},
};

use super::{
    ChainSubmissionGeneration, ChainSubmissionGenerationDigest, ChainSubmissionIdentity,
    ChainSubmissionTarget,
};

const GENERATION_DOMAIN_V1: &[u8] = b"zcash_voting.chain_submission.generation.v1\0";
type PaddedNoteSecret = ([u8; 32], [u8; 32]);

/// Tree leaves expected from a generation, retained in protocol action order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExpectedTreeLayout {
    Delegation {
        delegation_van: [u8; 32],
    },
    Vote {
        successor_van: [u8; 32],
        vote_commitment: [u8; 32],
    },
    VoteBatch {
        final_successor_van: [u8; 32],
        vote_commitments: Vec<[u8; 32]>,
    },
}

impl ExpectedTreeLayout {
    /// Returns the generation's expected tree leaves in protocol action order.
    pub(crate) fn leaves(&self) -> Vec<[u8; 32]> {
        match self {
            Self::Delegation { delegation_van } => vec![*delegation_van],
            Self::Vote {
                successor_van,
                vote_commitment,
            } => vec![*successor_van, *vote_commitment],
            Self::VoteBatch {
                final_successor_van,
                vote_commitments,
            } => std::iter::once(*final_successor_van)
                .chain(vote_commitments.iter().copied())
                .collect(),
        }
    }
}

/// Closed chain request reconstructed from one supported semantic generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ChainSubmissionRequest {
    Delegation(DelegationSubmissionWire),
    Vote(VoteCommitmentWire),
    VoteBatch(VoteCommitmentBatchWire),
}

/// One semantic generation and the confirmation layout it must produce.
///
/// This is everything derivation needs to *identify* a submission. It is
/// deliberately separate from the wire request, which additionally requires a
/// live delegation signer, so the version-17 migration can bind a generation to
/// durable recovery inputs without reconstructing a dispatchable request.
#[derive(Clone)]
pub(crate) struct BoundGeneration {
    generation: ChainSubmissionGeneration,
    expected_layout: ExpectedTreeLayout,
    ordered_proposal_ids: Vec<u32>,
}

impl BoundGeneration {
    /// Returns the identity-bound semantic generation.
    pub(crate) fn generation(&self) -> &ChainSubmissionGeneration {
        &self.generation
    }

    /// Returns the tree layout expected from a successful submission.
    pub(crate) fn expected_layout(&self) -> &ExpectedTreeLayout {
        &self.expected_layout
    }

    /// Returns proposal IDs in the signed batch action order, or an empty slice
    /// for delegation.
    pub(crate) fn ordered_proposal_ids(&self) -> &[u32] {
        &self.ordered_proposal_ids
    }
}

/// A semantic generation and its matching wire request and confirmation layout.
#[derive(Clone)]
pub(super) struct DerivedChainSubmission {
    bound: BoundGeneration,
    request: ChainSubmissionRequest,
}

impl DerivedChainSubmission {
    /// Assembles synthetic generation artifacts for lifecycle tests.
    #[cfg(test)]
    pub(super) fn new(
        generation: ChainSubmissionGeneration,
        request: ChainSubmissionRequest,
        expected_layout: ExpectedTreeLayout,
        ordered_proposal_ids: Vec<u32>,
    ) -> Self {
        Self {
            bound: BoundGeneration {
                generation,
                expected_layout,
                ordered_proposal_ids,
            },
            request,
        }
    }

    /// Returns the identity-bound semantic generation.
    pub(super) fn generation(&self) -> &ChainSubmissionGeneration {
        self.bound.generation()
    }

    /// Returns the closed wire request reconstructed from durable inputs.
    pub(super) fn request(&self) -> &ChainSubmissionRequest {
        &self.request
    }

    /// Returns the tree layout expected from a successful submission.
    pub(super) fn expected_layout(&self) -> &ExpectedTreeLayout {
        self.bound.expected_layout()
    }

    /// Returns proposal IDs in the signed batch action order, or an empty slice
    /// for delegation.
    pub(super) fn ordered_proposal_ids(&self) -> &[u32] {
        self.bound.ordered_proposal_ids()
    }
}

struct GenerationTranscript(Sha256);

impl GenerationTranscript {
    fn new() -> Self {
        let mut hash = Sha256::new();
        hash.update(GENERATION_DOMAIN_V1);
        Self(hash)
    }

    fn field(&mut self, tag: &str, value: &[u8]) {
        let tag_len = u16::try_from(tag.len()).expect("generation field tag fits u16");
        let value_len = u64::try_from(value.len()).expect("generation field value fits u64");
        self.0.update(tag_len.to_be_bytes());
        self.0.update(tag.as_bytes());
        self.0.update(value_len.to_be_bytes());
        self.0.update(value);
    }

    fn u32(&mut self, tag: &str, value: u32) {
        self.field(tag, &value.to_be_bytes());
    }

    fn u64(&mut self, tag: &str, value: u64) {
        self.field(tag, &value.to_be_bytes());
    }

    fn bool(&mut self, tag: &str, value: bool) {
        self.field(tag, &[u8::from(value)]);
    }

    fn bytes32(&mut self, tag: &str, value: &[u8; 32]) {
        self.field(tag, value);
    }

    fn sequence<T>(
        &mut self,
        tag: &str,
        values: &[T],
        mut encode: impl FnMut(&mut Self, usize, &T),
    ) {
        self.u32(
            &format!("{tag}.count"),
            u32::try_from(values.len()).expect("generation sequence length fits u32"),
        );
        for (index, value) in values.iter().enumerate() {
            encode(self, index, value);
        }
    }

    fn finish(self) -> ChainSubmissionGenerationDigest {
        ChainSubmissionGenerationDigest::from_bytes(self.0.finalize().into())
    }
}

fn hash_identity(transcript: &mut GenerationTranscript, identity: &ChainSubmissionIdentity) {
    transcript.field("identity.wallet_id", identity.wallet_id().as_bytes());
    transcript.field(
        "identity.network",
        match identity.network() {
            Network::Mainnet => b"mainnet",
            Network::Testnet => b"testnet",
            Network::Regtest => b"regtest",
        },
    );
    transcript.bytes32("identity.vote_round_id", identity.vote_round_id());
    transcript.u32("identity.bundle_index", identity.bundle_index());
    match identity.target() {
        ChainSubmissionTarget::Delegation => transcript.field("identity.kind", b"delegation"),
        ChainSubmissionTarget::Vote { proposal_id } => {
            transcript.field("identity.kind", b"vote");
            transcript.u32("identity.proposal_id", proposal_id);
        }
        ChainSubmissionTarget::VoteBatch {
            ordered_batch_digest,
        } => {
            transcript.field("identity.kind", b"vote_batch");
            transcript.bytes32("identity.ordered_batch_digest", &ordered_batch_digest);
        }
    }
}

fn validate_identity_context(
    conn: &rusqlite::Connection,
    identity: &ChainSubmissionIdentity,
) -> Result<String, VotingError> {
    let round_id = hex::encode(identity.vote_round_id());
    let network = queries::load_round_network(conn, &round_id, identity.wallet_id())?;
    if network != identity.network() {
        return Err(VotingError::InvalidInput {
            message: "chain submission identity network does not match the stored round"
                .to_string(),
        });
    }
    Ok(round_id)
}

struct DelegationGenerationInputs {
    note_positions: Vec<u64>,
    note_identity_hashes: Vec<[u8; 32]>,
    van_comm_rand: [u8; 32],
    dummy_nullifiers: Vec<[u8; 32]>,
    rho_signed: [u8; 32],
    padded_note_data: Vec<[u8; 32]>,
    nf_signed: [u8; 32],
    cmx_new: [u8; 32],
    alpha: [u8; 32],
    rseed_signed: [u8; 32],
    rseed_output: [u8; 32],
    gov_comm: [u8; 32],
    total_note_value: u64,
    address_index: u32,
    rk: [u8; 32],
    gov_nullifiers: Vec<[u8; 32]>,
    padded_note_secrets: Vec<PaddedNoteSecret>,
    pczt_sighash: [u8; 32],
    tx1_effects: Vec<u8>,
    proof: Vec<u8>,
}

fn checked_blob32(value: Vec<u8>, field: &str) -> Result<[u8; 32], VotingError> {
    value
        .try_into()
        .map_err(|value: Vec<u8>| VotingError::Internal {
            message: format!("stored {field} must be 32 bytes, got {}", value.len()),
        })
}

fn checked_blob32_sequence(value: Vec<u8>, field: &str) -> Result<Vec<[u8; 32]>, VotingError> {
    if !value.len().is_multiple_of(32) {
        return Err(VotingError::Internal {
            message: format!(
                "stored {field} length must be a multiple of 32, got {}",
                value.len()
            ),
        });
    }
    value
        .chunks_exact(32)
        .map(|chunk| {
            chunk.try_into().map_err(|_| VotingError::Internal {
                message: format!("stored {field} element must be 32 bytes"),
            })
        })
        .collect()
}

fn checked_note_positions(value: Vec<u8>) -> Result<Vec<u64>, VotingError> {
    if !value.len().is_multiple_of(8) {
        return Err(VotingError::Internal {
            message: format!(
                "stored note_positions length must be a multiple of 8, got {}",
                value.len()
            ),
        });
    }
    value
        .chunks_exact(8)
        .map(|chunk| {
            let bytes: [u8; 8] = chunk.try_into().map_err(|_| VotingError::Internal {
                message: "stored note position must be 8 bytes".to_string(),
            })?;
            Ok(u64::from_le_bytes(bytes))
        })
        .collect()
}

fn checked_padded_note_secrets(value: Vec<u8>) -> Result<Vec<PaddedNoteSecret>, VotingError> {
    if !value.len().is_multiple_of(64) {
        return Err(VotingError::Internal {
            message: format!(
                "stored padded_note_secrets length must be a multiple of 64, got {}",
                value.len()
            ),
        });
    }
    value
        .chunks_exact(64)
        .map(|chunk| {
            let rho = chunk[..32].try_into().map_err(|_| VotingError::Internal {
                message: "stored padded note rho must be 32 bytes".to_string(),
            })?;
            let rseed = chunk[32..].try_into().map_err(|_| VotingError::Internal {
                message: "stored padded note rseed must be 32 bytes".to_string(),
            })?;
            Ok((rho, rseed))
        })
        .collect()
}

fn load_delegation_inputs(
    conn: &rusqlite::Connection,
    identity: &ChainSubmissionIdentity,
    round_id: &str,
) -> Result<Option<DelegationGenerationInputs>, VotingError> {
    let stored_inputs = conn
        .query_row(
            "SELECT b.note_positions_blob, b.note_identity_hashes_blob,
                b.van_comm_rand, b.dummy_nullifiers, b.rho_signed,
                b.padded_note_data, b.nf_signed, b.cmx_new, b.alpha,
                b.rseed_signed, b.rseed_output, b.gov_comm,
                b.total_note_value, b.address_index, b.rk,
                b.gov_nullifiers_blob, b.padded_note_secrets,
                b.pczt_sighash, b.tx1_effects, p.proof
           FROM bundles b
           JOIN proofs p
             ON p.round_id = b.round_id
            AND p.wallet_id = b.wallet_id
            AND p.bundle_index = b.bundle_index
          WHERE b.round_id = :round_id
            AND b.wallet_id = :wallet_id
            AND b.bundle_index = :bundle_index
            AND p.success = 1",
            named_params! {
                ":round_id": round_id,
                ":wallet_id": identity.wallet_id(),
                ":bundle_index": identity.bundle_index() as i64,
            },
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, Vec<u8>>(6)?,
                    row.get::<_, Vec<u8>>(7)?,
                    row.get::<_, Vec<u8>>(8)?,
                    row.get::<_, Vec<u8>>(9)?,
                    row.get::<_, Vec<u8>>(10)?,
                    row.get::<_, Vec<u8>>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, i64>(13)?,
                    row.get::<_, Vec<u8>>(14)?,
                    row.get::<_, Vec<u8>>(15)?,
                    row.get::<_, Vec<u8>>(16)?,
                    row.get::<_, Vec<u8>>(17)?,
                    row.get::<_, Vec<u8>>(18)?,
                    row.get::<_, Vec<u8>>(19)?,
                ))
            },
        )
        .optional()
        .map_err(|error| VotingError::Internal {
            message: format!("failed to load delegation generation inputs: {error}"),
        })?;

    stored_inputs
        .map(
            |(
            note_positions,
            note_identity_hashes,
            van_comm_rand,
            dummy_nullifiers,
            rho_signed,
            padded_note_data,
            nf_signed,
            cmx_new,
            alpha,
            rseed_signed,
            rseed_output,
            gov_comm,
            total_note_value,
            address_index,
            rk,
            gov_nullifiers,
            padded_note_secrets,
            pczt_sighash,
            tx1_effects,
            proof,
        )| {
            let note_positions = checked_note_positions(note_positions)?;
            let note_identity_hashes =
                checked_blob32_sequence(note_identity_hashes, "note_identity_hashes")?;
            if note_identity_hashes.len() != note_positions.len() {
                return Err(VotingError::Internal {
                    message: format!(
                        "stored note identity count {} does not match note position count {}",
                        note_identity_hashes.len(),
                        note_positions.len()
                    ),
                });
            }
            let expected_padded_count = BUNDLE_NOTE_SLOTS
                .checked_sub(note_positions.len())
                .ok_or_else(|| VotingError::Internal {
                    message: format!(
                        "stored note position count {} exceeds bundle capacity {BUNDLE_NOTE_SLOTS}",
                        note_positions.len()
                    ),
                })?;
            let dummy_nullifiers =
                checked_blob32_sequence(dummy_nullifiers, "dummy_nullifiers")?;
            let padded_note_data =
                checked_blob32_sequence(padded_note_data, "padded_note_data")?;
            let padded_note_secrets = checked_padded_note_secrets(padded_note_secrets)?;
            for (field, count) in [
                ("dummy_nullifiers", dummy_nullifiers.len()),
                ("padded_note_data", padded_note_data.len()),
                ("padded_note_secrets", padded_note_secrets.len()),
            ] {
                if count != expected_padded_count {
                    return Err(VotingError::Internal {
                        message: format!(
                            "stored {field} count {count} does not match expected padded note count {expected_padded_count}"
                        ),
                    });
                }
            }
            let gov_nullifiers =
                checked_blob32_sequence(gov_nullifiers, "gov_nullifiers")?;
            if gov_nullifiers.len() != BUNDLE_NOTE_SLOTS {
                return Err(VotingError::Internal {
                    message: format!(
                        "stored gov_nullifier count {} does not match bundle capacity {BUNDLE_NOTE_SLOTS}",
                        gov_nullifiers.len()
                    ),
                });
            }
            crate::tx1::validate_tx1_effects(&tx1_effects)?;
            Ok(DelegationGenerationInputs {
                note_positions,
                note_identity_hashes,
                van_comm_rand: checked_blob32(van_comm_rand, "van_comm_rand")?,
                dummy_nullifiers,
                rho_signed: checked_blob32(rho_signed, "rho_signed")?,
                padded_note_data,
                nf_signed: checked_blob32(nf_signed, "nf_signed")?,
                cmx_new: checked_blob32(cmx_new, "cmx_new")?,
                alpha: checked_blob32(alpha, "alpha")?,
                rseed_signed: checked_blob32(rseed_signed, "rseed_signed")?,
                rseed_output: checked_blob32(rseed_output, "rseed_output")?,
                gov_comm: checked_blob32(gov_comm, "gov_comm")?,
                total_note_value: u64::try_from(total_note_value).map_err(|_| {
                    VotingError::Internal {
                        message: format!(
                            "stored total_note_value must be non-negative, got {total_note_value}"
                        ),
                    }
                })?,
                address_index: u32::try_from(address_index).map_err(|_| VotingError::Internal {
                    message: format!("stored address_index must fit u32, got {address_index}"),
                })?,
                rk: checked_blob32(rk, "rk")?,
                gov_nullifiers,
                padded_note_secrets,
                pczt_sighash: checked_blob32(pczt_sighash, "pczt_sighash")?,
                tx1_effects,
                proof,
            })
            },
        )
        .transpose()
}

fn hash_delegation_inputs(
    transcript: &mut GenerationTranscript,
    inputs: &DelegationGenerationInputs,
) {
    transcript.sequence(
        "delegation.note_positions",
        &inputs.note_positions,
        |transcript, index, position| {
            transcript.u64(&format!("delegation.note_positions.{index}"), *position);
        },
    );
    transcript.sequence(
        "delegation.note_identity_hashes",
        &inputs.note_identity_hashes,
        |transcript, index, identity_hash| {
            transcript.bytes32(
                &format!("delegation.note_identity_hashes.{index}"),
                identity_hash,
            );
        },
    );
    transcript.bytes32("delegation.van_comm_rand", &inputs.van_comm_rand);
    transcript.sequence(
        "delegation.dummy_nullifiers",
        &inputs.dummy_nullifiers,
        |transcript, index, nullifier| {
            transcript.bytes32(&format!("delegation.dummy_nullifiers.{index}"), nullifier);
        },
    );
    transcript.bytes32("delegation.rho_signed", &inputs.rho_signed);
    transcript.sequence(
        "delegation.padded_note_data",
        &inputs.padded_note_data,
        |transcript, index, commitment| {
            transcript.bytes32(&format!("delegation.padded_note_data.{index}"), commitment);
        },
    );
    transcript.bytes32("delegation.nf_signed", &inputs.nf_signed);
    transcript.bytes32("delegation.cmx_new", &inputs.cmx_new);
    transcript.bytes32("delegation.alpha", &inputs.alpha);
    transcript.bytes32("delegation.rseed_signed", &inputs.rseed_signed);
    transcript.bytes32("delegation.rseed_output", &inputs.rseed_output);
    transcript.bytes32("delegation.gov_comm", &inputs.gov_comm);
    transcript.u64("delegation.total_note_value", inputs.total_note_value);
    transcript.u32("delegation.address_index", inputs.address_index);
    transcript.bytes32("delegation.rk", &inputs.rk);
    transcript.sequence(
        "delegation.gov_nullifiers",
        &inputs.gov_nullifiers,
        |transcript, index, nullifier| {
            transcript.bytes32(&format!("delegation.gov_nullifiers.{index}"), nullifier);
        },
    );
    transcript.sequence(
        "delegation.padded_note_secrets",
        &inputs.padded_note_secrets,
        |transcript, index, (rho, rseed)| {
            transcript.bytes32(&format!("delegation.padded_note_secrets.{index}.rho"), rho);
            transcript.bytes32(
                &format!("delegation.padded_note_secrets.{index}.rseed"),
                rseed,
            );
        },
    );
    transcript.bytes32("delegation.pczt_sighash", &inputs.pczt_sighash);
    transcript.field("delegation.tx1_effects", &inputs.tx1_effects);
    transcript.field("delegation.proof", &inputs.proof);
}

fn delegation_generation(
    identity: &ChainSubmissionIdentity,
    inputs: &DelegationGenerationInputs,
) -> ChainSubmissionGeneration {
    let mut transcript = GenerationTranscript::new();
    hash_identity(&mut transcript, identity);
    hash_delegation_inputs(&mut transcript, inputs);
    ChainSubmissionGeneration::new(identity.clone(), transcript.finish())
}

/// Binds one complete persisted delegation generation without a signer.
///
/// The SpendAuth signature is excluded from the generation digest, so a
/// delegation is fully identified by its durable setup, proof, nullifier, and
/// VAN-randomizer inputs alone. A restarted software delegation may therefore be
/// re-signed and still verify against this same generation.
///
/// Returns `Ok(None)` only when no successful proof and complete setup row
/// exists. Malformed or inconsistent persisted inputs remain errors.
pub(crate) fn complete_generation_for_delegation(
    conn: &rusqlite::Connection,
    identity: &ChainSubmissionIdentity,
) -> Result<Option<BoundGeneration>, VotingError> {
    if identity.target() != ChainSubmissionTarget::Delegation {
        return Err(VotingError::InvalidInput {
            message: "delegation generation requires a delegation identity".to_string(),
        });
    }
    let round_id = validate_identity_context(conn, identity)?;
    let Some(inputs) = load_delegation_inputs(conn, identity, &round_id)? else {
        return Ok(None);
    };
    Ok(Some(BoundGeneration {
        generation: delegation_generation(identity, &inputs),
        expected_layout: ExpectedTreeLayout::Delegation {
            delegation_van: inputs.gov_comm,
        },
        ordered_proposal_ids: vec![],
    }))
}

/// Requires one complete persisted delegation generation.
///
/// Unlike [`complete_generation_for_delegation`], absence is an error because
/// runtime derivation cannot construct a request without the durable setup and
/// successful proof.
pub(crate) fn generation_for_delegation(
    conn: &rusqlite::Connection,
    identity: &ChainSubmissionIdentity,
) -> Result<BoundGeneration, VotingError> {
    complete_generation_for_delegation(conn, identity)?.ok_or_else(|| VotingError::InvalidInput {
        message: format!(
            "complete delegation generation not found for round={}, bundle={}",
            hex::encode(identity.vote_round_id()),
            identity.bundle_index()
        ),
    })
}

/// Reconstructs and hashes one persisted delegation generation.
///
/// The supplied signature must verify against the stored PCZT sighash. This
/// function reads durable state without mutating submission lifecycle rows.
pub(super) fn derive_delegation(
    conn: &rusqlite::Connection,
    identity: &ChainSubmissionIdentity,
    signer: DelegationSigner,
) -> Result<DerivedChainSubmission, VotingError> {
    let bound = generation_for_delegation(conn, identity)?;
    let round_id = hex::encode(identity.vote_round_id());
    let submission = crate::delegate::submission_with_conn(
        conn,
        identity.wallet_id(),
        &round_id,
        identity.bundle_index(),
        signer,
    )?;
    let request = DelegationSubmissionWire::try_from(&submission)?;
    Ok(DerivedChainSubmission {
        bound,
        request: ChainSubmissionRequest::Delegation(request),
    })
}

fn load_validated_vote(
    conn: &rusqlite::Connection,
    identity: &ChainSubmissionIdentity,
    round_id: &str,
    proposal_id: u32,
) -> Result<VoteRecoveryBundle, VotingError> {
    let recovery = crate::vote::recovery_bundle_with_conn(
        conn,
        identity.wallet_id(),
        round_id,
        identity.bundle_index(),
        proposal_id,
    )?
    .ok_or_else(|| VotingError::InvalidInput {
        message: format!(
            "vote recovery bundle not found for round={round_id}, bundle={}, proposal={proposal_id}",
            identity.bundle_index()
        ),
    })?;
    let state = queries::load_vote_row_state(
        conn,
        round_id,
        identity.wallet_id(),
        identity.bundle_index(),
        proposal_id,
    )?
    .ok_or_else(|| VotingError::InvalidInput {
        message: format!(
            "vote not found for round={round_id}, bundle={}, proposal={proposal_id}",
            identity.bundle_index()
        ),
    })?;
    crate::vote::validate_recovery_matches_stored_vote(
        &recovery,
        round_id,
        identity.bundle_index(),
        proposal_id,
        state.choice,
        state.commitment.as_deref(),
    )?;
    Ok(recovery)
}

fn hash_encrypted_share(
    transcript: &mut GenerationTranscript,
    prefix: &str,
    share: &EncryptedShare,
) {
    transcript.field(&format!("{prefix}.c1"), &share.c1);
    transcript.field(&format!("{prefix}.c2"), &share.c2);
    transcript.u32(&format!("{prefix}.share_index"), share.share_index);
    transcript.u64(&format!("{prefix}.plaintext_value"), share.plaintext_value);
    transcript.field(&format!("{prefix}.randomness"), &share.randomness);
}

fn hash_vote_recovery(
    transcript: &mut GenerationTranscript,
    prefix: &str,
    recovery: &VoteRecoveryBundle,
) {
    transcript.field(
        &format!("{prefix}.vote_round_id"),
        recovery.vote_round_id.as_bytes(),
    );
    transcript.u32(&format!("{prefix}.bundle_index"), recovery.bundle_index);
    transcript.u32(&format!("{prefix}.proposal_id"), recovery.proposal_id);
    transcript.u32(&format!("{prefix}.vote_decision"), recovery.vote_decision);
    transcript.u32(&format!("{prefix}.anchor_height"), recovery.anchor_height);
    transcript.bool(&format!("{prefix}.single_share"), recovery.single_share);
    transcript.u32(&format!("{prefix}.num_options"), recovery.num_options);
    transcript.bytes32(&format!("{prefix}.van_nullifier"), &recovery.van_nullifier);
    transcript.bytes32(
        &format!("{prefix}.vote_authority_note_new"),
        &recovery.vote_authority_note_new,
    );
    transcript.bytes32(
        &format!("{prefix}.vote_commitment"),
        &recovery.vote_commitment,
    );
    transcript.field(&format!("{prefix}.proof"), &recovery.proof);
    transcript.bytes32(&format!("{prefix}.shares_hash"), &recovery.shares_hash);
    transcript.bytes32(&format!("{prefix}.r_vpk"), &recovery.r_vpk);
    transcript.bytes32(&format!("{prefix}.alpha_v"), &recovery.alpha_v);
    transcript.field(&format!("{prefix}.vote_auth_sig"), &recovery.vote_auth_sig);
    transcript.sequence(
        &format!("{prefix}.encrypted_shares"),
        &recovery.encrypted_shares,
        |transcript, index, share| {
            hash_encrypted_share(
                transcript,
                &format!("{prefix}.encrypted_shares.{index}"),
                share,
            );
        },
    );
    transcript.sequence(
        &format!("{prefix}.share_blinds"),
        &recovery.share_blinds,
        |transcript, index, blind| {
            transcript.bytes32(&format!("{prefix}.share_blinds.{index}"), blind);
        },
    );
    transcript.sequence(
        &format!("{prefix}.share_comms"),
        &recovery.share_comms,
        |transcript, index, commitment| {
            transcript.bytes32(&format!("{prefix}.share_comms.{index}"), commitment);
        },
    );
    match recovery.batch.as_ref() {
        None => transcript.field(&format!("{prefix}.batch"), b"singleton"),
        Some(batch) => {
            transcript.field(&format!("{prefix}.batch"), b"member");
            transcript.bytes32(&format!("{prefix}.batch.digest"), &batch.digest);
            transcript.u32(&format!("{prefix}.batch.index"), batch.index);
            transcript.u32(&format!("{prefix}.batch.size"), batch.size);
        }
    }
}

fn vote_generation(
    identity: &ChainSubmissionIdentity,
    recoveries: &[VoteRecoveryBundle],
) -> ChainSubmissionGeneration {
    let mut transcript = GenerationTranscript::new();
    hash_identity(&mut transcript, identity);
    transcript.sequence("votes", recoveries, |transcript, index, recovery| {
        hash_vote_recovery(transcript, &format!("votes.{index}"), recovery);
    });
    ChainSubmissionGeneration::new(identity.clone(), transcript.finish())
}

/// Reconstructs and hashes one persisted singleton-vote generation.
///
/// The identity, stored vote row, and recovery bundle must agree. Confirmation
/// positions are excluded, and no durable state is changed.
pub(super) fn derive_vote(
    conn: &rusqlite::Connection,
    identity: &ChainSubmissionIdentity,
) -> Result<DerivedChainSubmission, VotingError> {
    let (bound, recovery) = validated_singleton_vote(conn, identity)?;
    let request = crate::vote::wire_submission_from_recovery(&recovery)?;
    Ok(DerivedChainSubmission {
        bound,
        request: ChainSubmissionRequest::Vote(request),
    })
}

/// Binds one persisted singleton-vote generation without building a request.
pub(crate) fn generation_for_vote(
    conn: &rusqlite::Connection,
    identity: &ChainSubmissionIdentity,
) -> Result<BoundGeneration, VotingError> {
    validated_singleton_vote(conn, identity).map(|(bound, _)| bound)
}

fn validated_singleton_vote(
    conn: &rusqlite::Connection,
    identity: &ChainSubmissionIdentity,
) -> Result<(BoundGeneration, VoteRecoveryBundle), VotingError> {
    let ChainSubmissionTarget::Vote { proposal_id } = identity.target() else {
        return Err(VotingError::InvalidInput {
            message: "singleton vote generation requires a vote identity".to_string(),
        });
    };
    let round_id = validate_identity_context(conn, identity)?;
    let recovery = load_validated_vote(conn, identity, &round_id, proposal_id)?;
    crate::vote::ensure_singleton_vote_recovery(&recovery)?;
    let bound = BoundGeneration {
        generation: vote_generation(identity, std::slice::from_ref(&recovery)),
        expected_layout: ExpectedTreeLayout::Vote {
            successor_van: recovery.vote_authority_note_new,
            vote_commitment: recovery.vote_commitment,
        },
        ordered_proposal_ids: vec![proposal_id],
    };
    Ok((bound, recovery))
}

/// Reconstructs and hashes a complete persisted atomic vote batch.
///
/// Members and their wire actions retain signed batch order. Missing,
/// inconsistent, or duplicate members are rejected without durable writes.
pub(super) fn derive_vote_batch(
    conn: &rusqlite::Connection,
    identity: &ChainSubmissionIdentity,
) -> Result<DerivedChainSubmission, VotingError> {
    let (bound, recoveries) = validated_vote_batch(conn, identity)?;
    let mut requests = Vec::with_capacity(recoveries.len());
    for recovery in &recoveries {
        requests.push(crate::vote::wire_submission_from_recovery(recovery)?);
    }
    Ok(DerivedChainSubmission {
        bound,
        request: ChainSubmissionRequest::VoteBatch(VoteCommitmentBatchWire { votes: requests }),
    })
}

/// Binds a complete persisted atomic vote batch without building requests.
pub(crate) fn generation_for_vote_batch(
    conn: &rusqlite::Connection,
    identity: &ChainSubmissionIdentity,
) -> Result<BoundGeneration, VotingError> {
    validated_vote_batch(conn, identity).map(|(bound, _)| bound)
}

fn validated_vote_batch(
    conn: &rusqlite::Connection,
    identity: &ChainSubmissionIdentity,
) -> Result<(BoundGeneration, Vec<VoteRecoveryBundle>), VotingError> {
    let ChainSubmissionTarget::VoteBatch {
        ordered_batch_digest,
    } = identity.target()
    else {
        return Err(VotingError::InvalidInput {
            message: "vote batch generation requires a vote_batch identity".to_string(),
        });
    };
    let round_id = validate_identity_context(conn, identity)?;
    let recoveries = crate::vote::load_vote_batch_recoveries_with_conn(
        conn,
        identity.wallet_id(),
        &round_id,
        identity.bundle_index(),
        ordered_batch_digest,
    )?;
    let expected_anchor = recoveries[0].anchor_height;
    let mut proposal_ids = Vec::with_capacity(recoveries.len());
    for recovery in &recoveries {
        if recovery.anchor_height != expected_anchor {
            return Err(VotingError::InvalidInput {
                message: "persisted atomic vote batch has inconsistent anchor heights".to_string(),
            });
        }
        let validated = load_validated_vote(conn, identity, &round_id, recovery.proposal_id)?;
        if validated.batch.as_ref().map(|batch| batch.digest) != Some(ordered_batch_digest) {
            return Err(VotingError::InvalidInput {
                message: "persisted atomic vote batch membership changed during derivation"
                    .to_string(),
            });
        }
        proposal_ids.push(recovery.proposal_id);
    }
    if proposal_ids
        .iter()
        .collect::<std::collections::HashSet<_>>()
        .len()
        != proposal_ids.len()
    {
        return Err(VotingError::InvalidInput {
            message: "persisted atomic vote batch contains duplicate proposals".to_string(),
        });
    }
    let final_successor_van = recoveries
        .last()
        .expect("validated vote batch is non-empty")
        .vote_authority_note_new;
    let vote_commitments = recoveries
        .iter()
        .map(|recovery| recovery.vote_commitment)
        .collect();
    let bound = BoundGeneration {
        generation: vote_generation(identity, &recoveries),
        expected_layout: ExpectedTreeLayout::VoteBatch {
            final_successor_van,
            vote_commitments,
        },
        ordered_proposal_ids: proposal_ids,
    };
    Ok((bound, recoveries))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::round::{RoundParams, VotingDb};
    use base64::Engine as _;

    fn delegation_inputs() -> DelegationGenerationInputs {
        DelegationGenerationInputs {
            note_positions: vec![1, 0x0102_0304_0506_0708],
            note_identity_hashes: vec![[0x01; 32], [0x02; 32]],
            van_comm_rand: [0x03; 32],
            dummy_nullifiers: vec![[0x04; 32], [0x05; 32], [0x06; 32]],
            rho_signed: [0x07; 32],
            padded_note_data: vec![[0x08; 32], [0x09; 32], [0x0a; 32]],
            nf_signed: [0x0b; 32],
            cmx_new: [0x0c; 32],
            alpha: [0x0d; 32],
            rseed_signed: [0x0e; 32],
            rseed_output: [0x0f; 32],
            gov_comm: [0x10; 32],
            total_note_value: 0x0102_0304_0506_0708,
            address_index: 0x0102_0304,
            rk: [0x11; 32],
            gov_nullifiers: vec![[0x12; 32]; BUNDLE_NOTE_SLOTS],
            padded_note_secrets: vec![
                ([0x13; 32], [0x14; 32]),
                ([0x15; 32], [0x16; 32]),
                ([0x17; 32], [0x18; 32]),
            ],
            pczt_sighash: [0x19; 32],
            tx1_effects: vec![0x1a; 64],
            proof: vec![0x1b; 96],
        }
    }

    fn recovery(proposal_id: u32) -> VoteRecoveryBundle {
        VoteRecoveryBundle {
            vote_round_id: "1111111111111111111111111111111111111111111111111111111111111111"
                .to_string(),
            bundle_index: 0,
            proposal_id,
            vote_decision: 2,
            anchor_height: 123,
            vc_tree_position: 456,
            single_share: false,
            num_options: 3,
            van_nullifier: [0x10; 32],
            vote_authority_note_new: [proposal_id as u8; 32],
            vote_commitment: [proposal_id as u8 + 0x20; 32],
            proof: vec![0x13; 96],
            shares_hash: [0x14; 32],
            r_vpk: [0x15; 32],
            alpha_v: [0x16; 32],
            vote_auth_sig: [0x17; 64],
            encrypted_shares: vec![EncryptedShare {
                c1: vec![0x21; 32],
                c2: vec![0x22; 32],
                share_index: 0,
                plaintext_value: 5,
                randomness: vec![0x23; 32],
            }],
            share_blinds: vec![[0x41; 32]],
            share_comms: vec![[0x51; 32]],
            batch: None,
        }
    }

    fn identity(target: ChainSubmissionTarget) -> ChainSubmissionIdentity {
        ChainSubmissionIdentity::new("wallet-1", Network::Testnet, [1; 32], 7, target).unwrap()
    }

    fn delegation_signature(
        seed: &[u8],
        alpha: &crate::backend::pasta_curves::pallas::Scalar,
        sighash: &[u8; 32],
    ) -> ([u8; 32], [u8; 64]) {
        use crate::backend::orchard::{
            keys::{SpendAuthorizingKey, SpendingKey},
            primitives::redpallas::{SpendAuth, VerificationKey},
        };
        use crate::backend::zcash_keys::keys::UnifiedSpendingKey;
        use zip32::AccountId;

        let account = AccountId::try_from(0).unwrap();
        let usk = UnifiedSpendingKey::from_seed(&Network::Testnet, seed, account).unwrap();
        let sk: SpendingKey = *usk.orchard();
        let randomized = SpendAuthorizingKey::from(&sk).randomize(alpha);
        let rk: [u8; 32] = (&VerificationKey::<SpendAuth>::from(&randomized)).into();
        let signature = randomized.sign(voting_crypto_deps::rand::rngs::OsRng, sighash);
        (rk, (&signature).into())
    }

    fn persisted_delegation() -> (
        VotingDb,
        ChainSubmissionIdentity,
        DelegationSigner,
        [u8; 64],
    ) {
        use crate::backend::pasta_curves::group::ff::PrimeField;

        const ROUND_ID: &str = "1111111111111111111111111111111111111111111111111111111111111111";
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id("wallet-1");
        db.create_round(
            Network::Testnet,
            &RoundParams {
                vote_round_id: ROUND_ID.to_string(),
                snapshot_height: 100,
                ea_pk: vec![0xea; 32],
                nc_root: vec![0xaa; 32],
                nullifier_imt_root: vec![0xbb; 32],
            },
            None,
        )
        .unwrap();

        let alpha = crate::backend::pasta_curves::pallas::Scalar::from(7);
        let alpha_bytes = alpha.to_repr();
        let sighash = [0x19; 32];
        let (rk, signature) = delegation_signature(&[0x42; 64], &alpha, &sighash);
        let gov_nullifiers = vec![vec![0x12; 32]; BUNDLE_NOTE_SLOTS];
        {
            let conn = db.conn();
            queries::insert_bundle(&conn, ROUND_ID, "wallet-1", 0, &[1, 2, 3, 4, 5]).unwrap();
            conn.execute(
                "UPDATE bundles SET note_identity_hashes_blob = ?1
                 WHERE round_id = ?2 AND wallet_id = ?3 AND bundle_index = 0",
                (vec![0x01; 32 * BUNDLE_NOTE_SLOTS], ROUND_ID, "wallet-1"),
            )
            .unwrap();
            queries::store_delegation_data_with_pczt_fields(
                &conn,
                ROUND_ID,
                "wallet-1",
                0,
                &[0x03; 32],
                &[],
                &[0x07; 32],
                &[],
                &[0x0b; 32],
                &[0x0c; 32],
                &alpha_bytes,
                &[0x0e; 32],
                &[0x0f; 32],
                &[0x10; 32],
                50,
                0,
                &[],
                &sighash,
                &crate::tx1::placeholder_tx1_effects(),
                &rk,
                &gov_nullifiers,
            )
            .unwrap();
            queries::store_proof_result_fields_with_van_comm(
                &conn,
                ROUND_ID,
                "wallet-1",
                0,
                &rk,
                &gov_nullifiers,
                &[0x0b; 32],
                &[0x0c; 32],
                &[0x10; 32],
            )
            .unwrap();
            queries::store_proof(&conn, ROUND_ID, "wallet-1", 0, &[0x1b; 96]).unwrap();
        }
        let identity = ChainSubmissionIdentity::new(
            "wallet-1",
            Network::Testnet,
            [0x11; 32],
            0,
            ChainSubmissionTarget::Delegation,
        )
        .unwrap();
        (
            db,
            identity,
            DelegationSigner::signature(signature, sighash),
            signature,
        )
    }

    /// Anchors the transcript encoding to an independently written byte
    /// sequence rather than to whatever the current code happens to emit.
    ///
    /// The frozen generation digests below are recorded outputs, so they cannot
    /// by themselves detect a framing change. This test states the version-1
    /// framing directly: the ASCII domain and NUL byte, then, per field, a
    /// big-endian `u16` tag length, the tag, a big-endian `u64` value length,
    /// and the value. If the framing ever changes, this fails first and says
    /// exactly which byte moved.
    #[test]
    fn generation_transcript_encodes_exact_framing_bytes() {
        let mut expected = b"zcash_voting.chain_submission.generation.v1\0".to_vec();
        // field("fixture.bytes", [0, 1, 2, 3])
        expected.extend_from_slice(&13_u16.to_be_bytes());
        expected.extend_from_slice(b"fixture.bytes");
        expected.extend_from_slice(&4_u64.to_be_bytes());
        expected.extend_from_slice(&[0, 1, 2, 3]);
        // u64("fixture.number", 0x0102_0304_0506_0708)
        expected.extend_from_slice(&14_u16.to_be_bytes());
        expected.extend_from_slice(b"fixture.number");
        expected.extend_from_slice(&8_u64.to_be_bytes());
        expected.extend_from_slice(&0x0102_0304_0506_0708_u64.to_be_bytes());

        let mut transcript = GenerationTranscript::new();
        transcript.field("fixture.bytes", &[0, 1, 2, 3]);
        transcript.u64("fixture.number", 0x0102_0304_0506_0708);

        assert_eq!(
            transcript.finish().to_hex(),
            hex::encode(Sha256::digest(&expected)),
            "generation transcript framing changed"
        );
    }

    /// The identity prefix binds no configured vote-chain id.
    ///
    /// A submission identity has no chain-id field to vary, so this asserts the
    /// stronger structural fact: the complete identity transcript is exactly
    /// the wallet, network, round, bundle, and target, in that order.
    #[test]
    fn identity_transcript_binds_no_vote_chain_id() {
        let mut expected = b"zcash_voting.chain_submission.generation.v1\0".to_vec();
        let mut field = |tag: &str, value: &[u8]| {
            expected.extend_from_slice(&(tag.len() as u16).to_be_bytes());
            expected.extend_from_slice(tag.as_bytes());
            expected.extend_from_slice(&(value.len() as u64).to_be_bytes());
            expected.extend_from_slice(value);
        };
        field("identity.wallet_id", b"wallet-1");
        field("identity.network", b"testnet");
        field("identity.vote_round_id", &[1; 32]);
        field("identity.bundle_index", &7_u32.to_be_bytes());
        field("identity.kind", b"vote");
        field("identity.proposal_id", &2_u32.to_be_bytes());

        let mut transcript = GenerationTranscript::new();
        hash_identity(
            &mut transcript,
            &identity(ChainSubmissionTarget::Vote { proposal_id: 2 }),
        );

        assert_eq!(
            transcript.finish().to_hex(),
            hex::encode(Sha256::digest(&expected))
        );
    }

    #[test]
    fn generation_transcript_framing_matches_frozen_vector() {
        let identity = identity(ChainSubmissionTarget::Vote { proposal_id: 2 });
        let mut transcript = GenerationTranscript::new();
        hash_identity(&mut transcript, &identity);
        transcript.field("fixture.bytes", &[0, 1, 2, 3]);
        transcript.u64("fixture.number", 0x0102_0304_0506_0708);

        assert_eq!(
            transcript.finish().to_hex(),
            "957d976f8656c57b9ef059045d5dccd1beb8e980c74176173010f0dab2606132"
        );
    }

    #[test]
    fn generation_digest_v1_matches_frozen_vector() {
        let delegation = delegation_generation(
            &identity(ChainSubmissionTarget::Delegation),
            &delegation_inputs(),
        );
        let singleton = vote_generation(
            &identity(ChainSubmissionTarget::Vote { proposal_id: 1 }),
            &[recovery(1)],
        );
        let batch_digest = [0xab; 32];
        let mut first = recovery(1);
        first.batch = Some(crate::vote::VoteBatchRecovery {
            digest: batch_digest,
            index: 0,
            size: 2,
        });
        let mut second = recovery(2);
        second.batch = Some(crate::vote::VoteBatchRecovery {
            digest: batch_digest,
            index: 1,
            size: 2,
        });
        let batch = vote_generation(
            &identity(ChainSubmissionTarget::VoteBatch {
                ordered_batch_digest: batch_digest,
            }),
            &[first, second],
        );

        assert_eq!(
            delegation.digest().to_hex(),
            "e04a9aab05cc403c3e4fd5818c38439a49da4239f22726ba7a8331ac5dcd4145"
        );
        assert_eq!(
            singleton.digest().to_hex(),
            "e69db505c93cb02ab3e20c81322d1101da17e2c826ff167d1ae61a8d59551048"
        );
        assert_eq!(
            batch.digest().to_hex(),
            "304c59580189347446783a472ef6489751f3fd100578a8667af97ef73bee7335"
        );
    }

    #[test]
    fn delegation_generation_digest_binds_each_typed_sequence() {
        let identity = identity(ChainSubmissionTarget::Delegation);
        let original = delegation_inputs();
        let original_digest = delegation_generation(&identity, &original).digest();

        let mut changed_position = delegation_inputs();
        changed_position.note_positions[0] += 1;
        assert_ne!(
            original_digest.as_bytes(),
            delegation_generation(&identity, &changed_position)
                .digest()
                .as_bytes()
        );

        let mut reordered_nullifiers = delegation_inputs();
        reordered_nullifiers.dummy_nullifiers.swap(0, 1);
        assert_ne!(
            original_digest.as_bytes(),
            delegation_generation(&identity, &reordered_nullifiers)
                .digest()
                .as_bytes()
        );

        let mut changed_secret = delegation_inputs();
        changed_secret.padded_note_secrets[0].1[0] ^= 1;
        assert_ne!(
            original_digest.as_bytes(),
            delegation_generation(&identity, &changed_secret)
                .digest()
                .as_bytes()
        );
    }

    #[test]
    fn malformed_delegation_blob_is_rejected_before_hashing() {
        let error = checked_note_positions(vec![0; 7]).unwrap_err();
        assert!(error.to_string().contains("multiple of 8"));

        let error = checked_blob32_sequence(vec![0; 31], "gov_nullifiers").unwrap_err();
        assert!(error.to_string().contains("multiple of 32"));

        let error = checked_padded_note_secrets(vec![0; 63]).unwrap_err();
        assert!(error.to_string().contains("multiple of 64"));
    }

    #[test]
    fn persisted_delegation_derives_and_rejects_a_forged_signature() {
        let (db, identity, signer, signature) = persisted_delegation();
        let derived = derive_delegation(&db.conn(), &identity, signer).unwrap();
        assert_eq!(derived.expected_layout().leaves(), vec![[0x10; 32]]);
        let ChainSubmissionRequest::Delegation(request) = derived.request() else {
            panic!("expected delegation request");
        };
        assert_eq!(
            request.spend_auth_sig,
            base64::prelude::BASE64_STANDARD.encode(signature)
        );

        let forged = DelegationSigner::signature([0xff; 64], [0x19; 32]);
        let error = match derive_delegation(&db.conn(), &identity, forged) {
            Ok(_) => panic!("forged signature must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("signature does not verify"));
    }

    #[test]
    fn persisted_delegation_rejects_noncanonical_sequence_storage() {
        let (db, identity, signer, _) = persisted_delegation();
        db.conn()
            .execute(
                "UPDATE bundles SET note_positions_blob = X'00'
                 WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = 0",
                (
                    "1111111111111111111111111111111111111111111111111111111111111111",
                    "wallet-1",
                ),
            )
            .unwrap();

        let error = match derive_delegation(&db.conn(), &identity, signer) {
            Ok(_) => panic!("noncanonical sequence storage must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("multiple of 8"));
    }

    #[test]
    fn generation_digest_binds_identity_semantics() {
        let first = identity(ChainSubmissionTarget::Vote { proposal_id: 1 });
        let second = identity(ChainSubmissionTarget::Vote { proposal_id: 2 });
        let digest = |identity: &ChainSubmissionIdentity| {
            let mut transcript = GenerationTranscript::new();
            hash_identity(&mut transcript, identity);
            transcript.field("vote.proof", b"proof");
            transcript.finish()
        };
        assert_ne!(digest(&first).as_bytes(), digest(&second).as_bytes());
        assert_eq!(digest(&first).as_bytes(), digest(&first).as_bytes());
    }

    #[test]
    fn expected_layouts_follow_signed_action_order() {
        let batch = ExpectedTreeLayout::VoteBatch {
            final_successor_van: [9; 32],
            vote_commitments: vec![[1; 32], [2; 32]],
        };
        assert_eq!(batch.leaves(), vec![[9; 32], [1; 32], [2; 32]]);
    }

    #[test]
    fn generation_digest_binds_semantics_and_ignores_confirmation_positions() {
        let identity = identity(ChainSubmissionTarget::Vote { proposal_id: 1 });
        let original = recovery(1);
        let mut confirmed = original.clone();
        confirmed.vc_tree_position = i64::MAX as u64;
        assert_eq!(
            vote_generation(&identity, std::slice::from_ref(&original))
                .digest()
                .as_bytes(),
            vote_generation(&identity, &[confirmed]).digest().as_bytes()
        );

        let mut changed_choice = original;
        changed_choice.vote_decision = 1;
        assert_ne!(
            vote_generation(&identity, &[recovery(1)])
                .digest()
                .as_bytes(),
            vote_generation(&identity, &[changed_choice])
                .digest()
                .as_bytes()
        );
    }

    #[test]
    fn batch_generation_digest_and_layout_preserve_action_order() {
        let batch_digest = [0xAB; 32];
        let identity = identity(ChainSubmissionTarget::VoteBatch {
            ordered_batch_digest: batch_digest,
        });
        let mut first = recovery(1);
        first.batch = Some(crate::vote::VoteBatchRecovery {
            digest: batch_digest,
            index: 0,
            size: 2,
        });
        let mut second = recovery(2);
        second.batch = Some(crate::vote::VoteBatchRecovery {
            digest: batch_digest,
            index: 1,
            size: 2,
        });

        assert_ne!(
            vote_generation(&identity, &[first.clone(), second.clone()])
                .digest()
                .as_bytes(),
            vote_generation(&identity, &[second, first])
                .digest()
                .as_bytes()
        );
    }

    #[test]
    fn persisted_vote_generation_survives_confirmation_projection() {
        const ROUND_ID: &str = "1111111111111111111111111111111111111111111111111111111111111111";
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id("wallet-1");
        db.create_round(
            Network::Testnet,
            &RoundParams {
                vote_round_id: ROUND_ID.to_string(),
                snapshot_height: 100,
                ea_pk: vec![0xEA; 32],
                nc_root: vec![0xAA; 32],
                nullifier_imt_root: vec![0xBB; 32],
            },
            None,
        )
        .unwrap();
        db.conn()
            .execute(
                "INSERT INTO bundles (round_id, wallet_id, bundle_index)
                 VALUES (?1, ?2, 0)",
                (ROUND_ID, "wallet-1"),
            )
            .unwrap();
        crate::vote::insert_recovery_fixture(&db, &recovery(1)).unwrap();
        let identity = ChainSubmissionIdentity::new(
            "wallet-1",
            Network::Testnet,
            [0x11; 32],
            0,
            ChainSubmissionTarget::Vote { proposal_id: 1 },
        )
        .unwrap();

        let before = derive_vote(&db.conn(), &identity).unwrap();
        {
            let mut conn = db.conn();
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            let confirmation =
                crate::chain_submission::result::ValidatedChainSubmissionConfirmation::from_tree(
                    7,
                    vec![789],
                )
                .unwrap();
            crate::chain_submission::confirmation::apply_confirmed_generation(
                &tx,
                &before,
                &confirmation,
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let after = derive_vote(&db.conn(), &identity).unwrap();

        assert_eq!(
            before.generation().digest().as_bytes(),
            after.generation().digest().as_bytes()
        );
        assert_eq!(before.request(), after.request());
        assert_eq!(
            after.expected_layout().leaves(),
            vec![[0x01; 32], [0x21; 32]]
        );
    }
}
