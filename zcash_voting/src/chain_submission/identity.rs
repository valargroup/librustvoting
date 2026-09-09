use std::{fmt, str::FromStr};

use thiserror::Error;

use crate::types::{validate_vote_round_id_bytes, Network, MAX_PROPOSAL_ID, MIN_PROPOSAL_ID};

const DIGEST_BYTES: usize = 32;
const CANONICAL_HASH_HEX_BYTES: usize = DIGEST_BYTES * 2;

/// The chain effect selected by a submission identity.
///
/// For a batch, the digest binds its complete ordered membership. The full
/// generation digest additionally binds proofs, commitments, and all other
/// semantic inputs described by the chain-submission invariants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChainSubmissionTarget {
    Delegation,
    Vote {
        proposal_id: u32,
    },
    VoteBatch {
        ordered_batch_digest: [u8; 32],
    },
    /// One delegation and its dependent ordered casts, committed atomically.
    DelegateAndVoteBatch {
        ordered_batch_digest: [u8; 32],
    },
}

impl ChainSubmissionTarget {
    /// Digest of the complete ordered cast set, when this is a batch target.
    pub(crate) fn batch_digest(self) -> Option<[u8; 32]> {
        match self {
            Self::VoteBatch {
                ordered_batch_digest,
            }
            | Self::DelegateAndVoteBatch {
                ordered_batch_digest,
            } => Some(ordered_batch_digest),
            _ => None,
        }
    }

    pub(crate) fn is_combined(self) -> bool {
        matches!(self, Self::DelegateAndVoteBatch { .. })
    }
}

/// Stable identity shared by all attempts for one submission meaning.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChainSubmissionIdentity {
    wallet_id: String,
    network: Network,
    vote_round_id: [u8; 32],
    bundle_index: u32,
    target: ChainSubmissionTarget,
}

impl ChainSubmissionIdentity {
    /// Constructs and validates a semantic submission identity.
    ///
    /// The configured vote-chain id is deliberately absent: it selects where a
    /// request is dispatched, not what the request means, so it binds neither
    /// this identity nor the generation digest. The deployment contract permits
    /// only one vote ledger per network, so one identity covers endpoint or
    /// chain-id changes only while they continue to address that same ledger.
    /// Reusing a voting database with an independent ledger under the same
    /// network is unsupported.
    ///
    /// Generation derivation later verifies this identity against the locked
    /// round and recovery inputs before reservation or dispatch.
    pub fn new(
        wallet_id: impl Into<String>,
        network: Network,
        vote_round_id: [u8; 32],
        bundle_index: u32,
        target: ChainSubmissionTarget,
    ) -> Result<Self, ChainSubmissionIdentityError> {
        let wallet_id = wallet_id.into();
        if wallet_id.is_empty() {
            return Err(ChainSubmissionIdentityError::EmptyWalletId);
        }

        if validate_vote_round_id_bytes(&vote_round_id).is_err() {
            return Err(ChainSubmissionIdentityError::InvalidVoteRoundId);
        }

        if let ChainSubmissionTarget::Vote { proposal_id } = target {
            if !(MIN_PROPOSAL_ID..=MAX_PROPOSAL_ID).contains(&proposal_id) {
                return Err(ChainSubmissionIdentityError::InvalidProposalId { proposal_id });
            }
        }

        Ok(Self {
            wallet_id,
            network,
            vote_round_id,
            bundle_index,
            target,
        })
    }

    pub fn wallet_id(&self) -> &str {
        &self.wallet_id
    }

    pub fn network(&self) -> Network {
        self.network
    }

    pub fn vote_round_id(&self) -> &[u8; 32] {
        &self.vote_round_id
    }

    pub fn bundle_index(&self) -> u32 {
        self.bundle_index
    }

    pub fn target(&self) -> ChainSubmissionTarget {
        self.target
    }
}

/// The exact lowercase network name used by durable keys and columns.
pub(crate) fn network_name(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "mainnet",
        Network::Testnet => "testnet",
        Network::Regtest => "regtest",
    }
}

/// Derives the durable primary key for one submission identity.
///
/// This is the only identity key in the system, so two reservations for the
/// same identity collide on the primary key instead of needing a separate
/// namespace and exclusion checks.
///
/// Every variable-length component is length-prefixed, so no two distinct
/// identities can produce the same key by rearranging their field boundaries.
pub(crate) fn submission_identity_key(identity: &ChainSubmissionIdentity) -> Vec<u8> {
    let mut key = b"zcash_voting.chain_submission.identity.v1\0".to_vec();
    for value in [
        identity.wallet_id().as_bytes(),
        network_name(identity.network()).as_bytes(),
        identity.vote_round_id().as_slice(),
    ] {
        key.extend_from_slice(&(value.len() as u64).to_be_bytes());
        key.extend_from_slice(value);
    }
    key.extend_from_slice(&identity.bundle_index().to_be_bytes());
    match identity.target() {
        ChainSubmissionTarget::Delegation => key.push(0),
        ChainSubmissionTarget::Vote { proposal_id } => {
            key.push(1);
            key.extend_from_slice(&proposal_id.to_be_bytes());
        }
        ChainSubmissionTarget::VoteBatch {
            ordered_batch_digest,
        }
        | ChainSubmissionTarget::DelegateAndVoteBatch {
            ordered_batch_digest,
        } => {
            key.push(if identity.target().is_combined() {
                3
            } else {
                2
            });
            key.extend_from_slice(&ordered_batch_digest);
        }
    }
    key
}

/// Validation failure for a semantic submission identity.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ChainSubmissionIdentityError {
    #[error("chain submission wallet_id must not be empty")]
    EmptyWalletId,
    #[error("vote_round_id must be a canonical Pallas field encoding")]
    InvalidVoteRoundId,
    #[error(
        "proposal_id must be between {MIN_PROPOSAL_ID} and {MAX_PROPOSAL_ID}, got {proposal_id}"
    )]
    InvalidProposalId { proposal_id: u32 },
}

/// Digest of every semantic input that determines a transaction's chain
/// effect.
///
/// This type deliberately omits `Debug`: the digest is a stable, linkable
/// identifier for privacy-sensitive voting inputs.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChainSubmissionGenerationDigest([u8; DIGEST_BYTES]);

impl ChainSubmissionGenerationDigest {
    #[allow(dead_code, reason = "derived by the SDK generation builder")]
    pub(super) fn from_bytes(bytes: [u8; DIGEST_BYTES]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; DIGEST_BYTES] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }
}

/// One immutable semantic generation and its full identity.
///
/// This type likewise omits `Debug` so generic logging cannot expose its
/// generation digest.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ChainSubmissionGeneration {
    identity: ChainSubmissionIdentity,
    digest: ChainSubmissionGenerationDigest,
}

impl ChainSubmissionGeneration {
    #[allow(dead_code, reason = "constructed by the SDK generation builder")]
    pub(super) fn new(
        identity: ChainSubmissionIdentity,
        digest: ChainSubmissionGenerationDigest,
    ) -> Self {
        Self { identity, digest }
    }

    pub fn identity(&self) -> &ChainSubmissionIdentity {
        &self.identity
    }

    pub fn digest(&self) -> ChainSubmissionGenerationDigest {
        self.digest
    }
}

/// Canonical lowercase 32-byte transaction hash used for status polling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CandidateTransactionHash([u8; DIGEST_BYTES]);

impl CandidateTransactionHash {
    pub fn from_bytes(bytes: [u8; DIGEST_BYTES]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; DIGEST_BYTES] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Display for CandidateTransactionHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl FromStr for CandidateTransactionHash {
    type Err = CandidateTransactionHashError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != CANONICAL_HASH_HEX_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(CandidateTransactionHashError);
        }

        let decoded = hex::decode(value).map_err(|_| CandidateTransactionHashError)?;
        let bytes = decoded
            .try_into()
            .map_err(|_| CandidateTransactionHashError)?;
        Ok(Self(bytes))
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("transaction hash must be exactly 64 lowercase hexadecimal characters")]
pub struct CandidateTransactionHashError;

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(target: ChainSubmissionTarget) -> ChainSubmissionIdentity {
        ChainSubmissionIdentity::new("wallet-1", Network::Testnet, [1; 32], 7, target).unwrap()
    }

    #[test]
    fn identity_keeps_target_semantics_distinct() {
        let delegation = identity(ChainSubmissionTarget::Delegation);
        let vote = identity(ChainSubmissionTarget::Vote { proposal_id: 1 });
        let batch = identity(ChainSubmissionTarget::VoteBatch {
            ordered_batch_digest: [9; 32],
        });

        assert_ne!(delegation, vote);
        assert_ne!(vote, batch);
    }

    #[test]
    fn identity_rejects_invalid_external_identifiers() {
        assert_eq!(
            ChainSubmissionIdentity::new(
                "",
                Network::Testnet,
                [1; 32],
                0,
                ChainSubmissionTarget::Delegation,
            ),
            Err(ChainSubmissionIdentityError::EmptyWalletId)
        );
        assert_eq!(
            ChainSubmissionIdentity::new(
                "wallet-1",
                Network::Testnet,
                [0xff; 32],
                0,
                ChainSubmissionTarget::Delegation,
            ),
            Err(ChainSubmissionIdentityError::InvalidVoteRoundId)
        );
        assert_eq!(
            ChainSubmissionIdentity::new(
                "wallet-1",
                Network::Testnet,
                [1; 32],
                0,
                ChainSubmissionTarget::Vote { proposal_id: 0 },
            ),
            Err(ChainSubmissionIdentityError::InvalidProposalId { proposal_id: 0 })
        );
    }

    #[test]
    fn candidate_hash_accepts_only_canonical_lowercase_hex() {
        let canonical = "ab".repeat(32);
        let hash = CandidateTransactionHash::from_str(&canonical).unwrap();
        assert_eq!(hash.to_string(), canonical);

        for invalid in ["ab".repeat(31), "AB".repeat(32), "gg".repeat(32)] {
            assert_eq!(
                CandidateTransactionHash::from_str(&invalid),
                Err(CandidateTransactionHashError)
            );
        }
    }
}
