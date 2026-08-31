//! Rollback-aware backups for votes awaiting helper-share completion.
//!
//! The wallet owns encryption, authentication, atomic replacement, and the
//! rollback-protected head of this ledger. This module owns the canonical
//! plaintext, validates every identity before restore, and keeps tombstones so
//! a retired pending vote cannot be resurrected from an older backup.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{named_params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    helper::url::{canonical_helper_url_list, canonicalize_helper_base_url},
    share::ShareDeliveryState,
    share_policy::{
        share_submission_target_count, ShareSubmissionPlan,
        SHARE_HELPER_MAX_INITIAL_SHARES_PER_SERVER, SHARE_HELPER_TARGET_COUNT_CAP,
        VOTE_COMMITMENT_SHARE_COUNT,
    },
    types::{validate_vote_round_id_hex, VotingError},
    vote::{parse_recovery, serialize_recovery, VoteRecoveryBundle},
};

const PENDING_VOTE_BACKUP_FORMAT_V1: &str = "zcash_voting_pending_vote_backup_v1";
const PENDING_VOTE_BACKUP_PERSONALIZATION: &[u8; 16] = b"VotePendingBkp1\0";

/// A canonical 32-byte identity or digest used by the pending-backup format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PendingVoteBackupDigest([u8; 32]);

impl PendingVoteBackupDigest {
    /// Wraps already authenticated digest bytes.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the canonical digest bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Serialize for PendingVoteBackupDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&hex::encode(self.0))
    }
}

impl<'de> Deserialize<'de> for PendingVoteBackupDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        if encoded.len() != 64
            || encoded
                .bytes()
                .any(|byte| !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte))
        {
            return Err(serde::de::Error::custom(
                "pending backup digest must be 64 lowercase hexadecimal characters",
            ));
        }
        let decoded = hex::decode(&encoded).map_err(serde::de::Error::custom)?;
        let bytes: [u8; 32] = decoded
            .try_into()
            .map_err(|_| serde::de::Error::custom("pending backup digest must be 32 bytes"))?;
        Ok(Self(bytes))
    }
}

/// Exact authenticated custody-capability identity used by a pending record.
///
/// This type deliberately omits `Debug` because the exact capability digest
/// is linkable privacy-sensitive material.
#[derive(Clone, PartialEq, Eq)]
pub struct PendingVoteCapabilityBindingV1 {
    bundle_index: u32,
    digest: PendingVoteBackupDigest,
}

impl PendingVoteCapabilityBindingV1 {
    /// Binds one bundle from an authority-validated custody capability.
    pub fn from_validated_capability(
        capability: &crate::delegation_capability::ValidatedDelegationCapabilityMaterialV1,
        bundle_index: u32,
    ) -> Result<Self, VotingError> {
        if !capability
            .bundles()
            .iter()
            .any(|bundle| bundle.bundle_index() == bundle_index)
        {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "validated custody capability does not contain bundle {bundle_index}"
                ),
            });
        }
        Ok(Self {
            bundle_index,
            digest: PendingVoteBackupDigest::from_bytes(*capability.digest().as_bytes()),
        })
    }
}

/// Exact bundle-source identity paired with one authority selection.
pub enum PendingVoteBundleBindingV1<'a> {
    RecoverableSelfCustody(&'a super::RecoverableBundleIdentityV1),
    CustodyCapability(&'a PendingVoteCapabilityBindingV1),
}

/// Typed authority, authenticated-round, and bundle expectation for restore.
///
/// This type deliberately omits `Debug` because it contains a
/// capability-derived bundle digest.
#[derive(Clone, PartialEq, Eq)]
pub struct PendingVoteBackupExpectedBindingV1 {
    authority_context_digest: PendingVoteBackupDigest,
    authority_source_digest: PendingVoteBackupDigest,
    bundle_source_digest: PendingVoteBackupDigest,
    round_id: String,
    bundle_index: u32,
}

impl PendingVoteBackupExpectedBindingV1 {
    /// Derives an import expectation from the retained public authority
    /// selection and the exact self-custody bundle or custody capability.
    pub fn derive(
        selection: &super::VotingAuthoritySelectionV1,
        bundle: PendingVoteBundleBindingV1<'_>,
    ) -> Result<Self, VotingError> {
        use super::BundleMaterialSourceV1;

        let (bundle_index, bundle_source_digest, expected_source) = match bundle {
            PendingVoteBundleBindingV1::RecoverableSelfCustody(identity) => (
                identity.bundle_index(),
                digest_serializable(&identity.canonical_transcript())?,
                BundleMaterialSourceV1::RecoverableSelfCustody,
            ),
            PendingVoteBundleBindingV1::CustodyCapability(capability) => (
                capability.bundle_index,
                capability.digest,
                BundleMaterialSourceV1::CustodyCapability,
            ),
        };
        if selection.bundle_source() != expected_source {
            return Err(VotingError::InvalidInput {
                message: "pending vote bundle binding does not match authority selection"
                    .to_string(),
            });
        }
        let round_id = hex::encode(selection.context().vote_round_id());
        validate_vote_round_id_hex(&round_id)?;
        Ok(Self {
            authority_context_digest: digest_serializable(
                &selection.context().canonical_transcript(),
            )?,
            authority_source_digest: digest_serializable(&selection.to_json()?)?,
            bundle_source_digest,
            round_id,
            bundle_index,
        })
    }

    fn matches(&self, binding: &PendingVoteBackupBindingV1) -> bool {
        self.authority_context_digest == binding.authority_context_digest
            && self.authority_source_digest == binding.authority_source_digest
            && self.bundle_source_digest == binding.bundle_source_digest
            && self.round_id == binding.round_id
            && self.bundle_index == binding.bundle_index
    }
}

/// Immutable identity binding for one pending singleton vote or atomic batch.
///
/// The opaque digests are supplied by the authority layer. They bind this
/// record to the selected authority context, authority source, and bundle
/// source without exposing those secret-bearing source records here.
/// This type deliberately omits `Debug` because the bundle digest can identify
/// a privacy-sensitive custody capability.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingVoteBackupBindingV1 {
    pub record_id: PendingVoteBackupDigest,
    pub authority_context_digest: PendingVoteBackupDigest,
    pub authority_source_digest: PendingVoteBackupDigest,
    pub bundle_source_digest: PendingVoteBackupDigest,
    pub round_id: String,
    pub bundle_index: u32,
}

/// The transaction container whose complete recovery material is backed up.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PendingVoteBackupKindV1 {
    Singleton {
        proposal_id: u32,
    },
    AtomicBatch {
        batch_digest: PendingVoteBackupDigest,
        ordered_proposal_ids: Vec<u32>,
    },
}

/// One signed vote action and its monotonic chain-observation state.
///
/// This type deliberately omits `Debug` because the recovery JSON contains
/// ballot and helper-share secrets.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingVoteActionBackupV1 {
    pub proposal_id: u32,
    /// Canonical library-owned recovery JSON. This contains ballot secrets.
    pub recovery_json: String,
    /// Durable CheckTx transaction identity, when recorded.
    pub tx_hash: Option<String>,
    /// Confirmed VC-tree position. `Some(0)` is a real position and is retained.
    pub confirmed_vc_tree_position: Option<u64>,
    /// Original local creation timestamp retained for exact restoration.
    pub created_at: u64,
}

/// Exact durable helper-tracker evidence for one share generation.
///
/// This type deliberately omits `Debug` because the pre-reveal share nullifier
/// is linkable privacy-sensitive material.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingHelperDeliveryBackupV1 {
    pub accepted_urls: Vec<String>,
    pub ambiguous_urls: Vec<String>,
    pub attempting_urls: Vec<String>,
    pub target_count: u32,
    pub nullifier: Vec<u8>,
    pub confirmed: bool,
    pub submit_at: u64,
    pub created_at: u64,
}

/// Original placement plan plus the current durable state for one expected share.
///
/// This type deliberately omits `Debug` because its delivery state may contain
/// a pre-reveal share nullifier.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingHelperShareBackupV1 {
    pub proposal_id: u32,
    pub share_index: u32,
    pub original_plan: ShareSubmissionPlan,
    /// Absent before the first durable helper-delivery row is created.
    pub delivery: Option<PendingHelperDeliveryBackupV1>,
}

/// One original planner result used when first capturing a pending record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingHelperSharePlanV1 {
    pub proposal_id: u32,
    pub share_index: u32,
    pub plan: ShareSubmissionPlan,
}

/// Complete restorable state for one pending singleton vote or atomic batch.
///
/// This type deliberately omits `Debug` because it contains secret recovery
/// material for every action.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingVoteBackupRecordV1 {
    pub binding: PendingVoteBackupBindingV1,
    pub vote_kind: PendingVoteBackupKindV1,
    pub actions: Vec<PendingVoteActionBackupV1>,
    /// Canonical helper fleet used when the original plans were produced.
    pub original_helper_fleet: Vec<String>,
    /// Canonical monotonic union of original and later compatible fleets.
    pub helper_fleet_history: Vec<String>,
    /// One entry for every share expected by every action.
    pub helper_shares: Vec<PendingHelperShareBackupV1>,
}

impl PendingVoteBackupRecordV1 {
    /// Constructs a record and derives its collision-resistant identity from
    /// the immutable authority, source, round, bundle, and vote generation.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        expected_binding: &PendingVoteBackupExpectedBindingV1,
        vote_kind: PendingVoteBackupKindV1,
        actions: Vec<PendingVoteActionBackupV1>,
        original_helper_fleet: Vec<String>,
        helper_shares: Vec<PendingHelperShareBackupV1>,
    ) -> Result<Self, VotingError> {
        let mut binding = PendingVoteBackupBindingV1 {
            record_id: PendingVoteBackupDigest::from_bytes([0; 32]),
            authority_context_digest: expected_binding.authority_context_digest,
            authority_source_digest: expected_binding.authority_source_digest,
            bundle_source_digest: expected_binding.bundle_source_digest,
            round_id: expected_binding.round_id.clone(),
            bundle_index: expected_binding.bundle_index,
        };
        binding.record_id = derive_record_id(&binding, &vote_kind, &actions)?;
        let mut helper_fleet_history = original_helper_fleet.clone();
        helper_fleet_history.sort();
        let record = Self {
            binding,
            vote_kind,
            actions,
            helper_fleet_history,
            original_helper_fleet,
            helper_shares,
        };
        validate_pending_record(&record)?;
        Ok(record)
    }

    /// Returns the stable identity committed by this record.
    pub const fn record_id(&self) -> PendingVoteBackupDigest {
        self.binding.record_id
    }
}

/// Captures a complete singleton or ordered atomic batch from durable storage.
///
/// The caller supplies only the typed authority/bundle expectation and the
/// original helper planner outputs. Vote timestamps, transaction state,
/// recovery material, VC positions, and existing delivery rows are read as one
/// exact snapshot from the wallet database.
pub fn capture_pending_vote_backup_record_v1(
    db: &crate::round::VotingDb,
    authority: super::RecoverableBundleUseV1<'_>,
    vote_kind: PendingVoteBackupKindV1,
    original_helper_fleet: Vec<String>,
    helper_plans: Vec<PendingHelperSharePlanV1>,
) -> Result<PendingVoteBackupRecordV1, VotingError> {
    let expected_binding = expected_binding_for_bundle_use(authority)?;
    let proposal_ids = match &vote_kind {
        PendingVoteBackupKindV1::Singleton { proposal_id } => vec![*proposal_id],
        PendingVoteBackupKindV1::AtomicBatch {
            ordered_proposal_ids,
            ..
        } => ordered_proposal_ids.clone(),
    };
    let wallet_id = db.wallet_id();
    let mut conn = db.conn();
    let tx = conn.transaction().map_err(|error| VotingError::Internal {
        message: format!("begin pending vote capture transaction failed: {error}"),
    })?;
    authority.validate_persisted_with_conn(
        &tx,
        &wallet_id,
        &expected_binding.round_id,
        expected_binding.bundle_index,
    )?;
    let mut actions = Vec::with_capacity(proposal_ids.len());
    for proposal_id in proposal_ids {
        let action: Option<(String, Option<String>, Option<u64>, u64)> = tx
            .query_row(
                "SELECT commitment_bundle_json, tx_hash, vc_tree_position, created_at
                 FROM votes
                 WHERE round_id = :round_id AND wallet_id = :wallet_id
                   AND bundle_index = :bundle_index AND proposal_id = :proposal_id",
                named_params! {
                    ":round_id": &expected_binding.round_id,
                    ":wallet_id": &wallet_id,
                    ":bundle_index": expected_binding.bundle_index,
                    ":proposal_id": proposal_id,
                },
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|error| VotingError::Internal {
                message: format!("capture pending vote action failed: {error}"),
            })?;
        let Some((recovery_json, tx_hash, confirmed_vc_tree_position, created_at)) = action else {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "pending vote action is missing durable recovery for proposal {proposal_id}"
                ),
            });
        };
        actions.push(PendingVoteActionBackupV1 {
            proposal_id,
            recovery_json,
            tx_hash,
            confirmed_vc_tree_position,
            created_at,
        });
    }
    tx.commit().map_err(|error| VotingError::Internal {
        message: format!("finish pending vote capture transaction failed: {error}"),
    })?;
    drop(conn);
    let record = PendingVoteBackupRecordV1::new(
        &expected_binding,
        vote_kind,
        actions,
        original_helper_fleet,
        helper_plans
            .into_iter()
            .map(|share| PendingHelperShareBackupV1 {
                proposal_id: share.proposal_id,
                share_index: share.share_index,
                original_plan: share.plan,
                delivery: None,
            })
            .collect(),
    )?;
    refresh_pending_vote_backup_record_v1(db, &record)
}

fn expected_binding_for_bundle_use(
    authority: super::RecoverableBundleUseV1<'_>,
) -> Result<PendingVoteBackupExpectedBindingV1, VotingError> {
    match authority.bundle_material() {
        super::RecoverableBundleMaterialV1::RecoverableSelfCustody(identity) => {
            PendingVoteBackupExpectedBindingV1::derive(
                authority.authority_selection(),
                PendingVoteBundleBindingV1::RecoverableSelfCustody(identity),
            )
        }
        super::RecoverableBundleMaterialV1::CustodyCapability {
            capability,
            bundle_index,
        } => {
            let capability = PendingVoteCapabilityBindingV1::from_validated_capability(
                capability,
                bundle_index,
            )?;
            PendingVoteBackupExpectedBindingV1::derive(
                authority.authority_selection(),
                PendingVoteBundleBindingV1::CustodyCapability(&capability),
            )
        }
    }
}

/// Why a live pending record was permanently retired.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum PendingVoteBackupRetirementV1 {
    EveryExpectedShareConfirmed,
    ReplacedBeforeSubmission {
        replacement_record_id: PendingVoteBackupDigest,
    },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingVoteActionGenerationEvidenceV1 {
    proposal_id: u32,
    recovery_generation_digest: PendingVoteBackupDigest,
    created_at: u64,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingVoteReplacementEvidenceV1 {
    vote_kind: PendingVoteBackupKindV1,
    actions: Vec<PendingVoteActionGenerationEvidenceV1>,
}

/// Minimal retained evidence that prevents an older live record from returning.
///
/// This type deliberately omits `Debug` because its binding contains a
/// privacy-sensitive bundle digest.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingVoteBackupTombstoneV1 {
    pub binding: PendingVoteBackupBindingV1,
    pub retired_record_digest: PendingVoteBackupDigest,
    pub retirement: PendingVoteBackupRetirementV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replacement_evidence: Option<PendingVoteReplacementEvidenceV1>,
}

/// One live pending record or its permanent tombstone.
///
/// This type deliberately omits `Debug` because its live variant contains
/// secret recovery material.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum PendingVoteBackupEntryV1 {
    Live(PendingVoteBackupRecordV1),
    Retired(PendingVoteBackupTombstoneV1),
}

impl PendingVoteBackupEntryV1 {
    fn binding(&self) -> &PendingVoteBackupBindingV1 {
        match self {
            Self::Live(record) => &record.binding,
            Self::Retired(tombstone) => &tombstone.binding,
        }
    }
}

/// Caller-encrypted ledger containing every live pending record and tombstone.
///
/// Each successor increments `revision` by one and commits to the exact prior
/// digest. The wallet must compare `(revision, digest)` with its independent
/// rollback-protected head before importing this plaintext.
/// This type deliberately omits `Debug` because live entries contain secret
/// recovery material.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingVoteBackupLedgerV1 {
    format: String,
    revision: u64,
    previous_digest: Option<PendingVoteBackupDigest>,
    entries: Vec<PendingVoteBackupEntryV1>,
    digest: PendingVoteBackupDigest,
}

#[derive(Serialize)]
struct PendingVoteBackupLedgerDigestInput<'a> {
    format: &'a str,
    revision: u64,
    previous_digest: Option<PendingVoteBackupDigest>,
    entries: &'a [PendingVoteBackupEntryV1],
}

impl PendingVoteBackupLedgerV1 {
    /// Starts a new rollback chain with one complete pending record.
    pub fn new(record: PendingVoteBackupRecordV1) -> Result<Self, VotingError> {
        validate_pending_record(&record)?;
        let mut ledger = Self {
            format: PENDING_VOTE_BACKUP_FORMAT_V1.to_string(),
            revision: 1,
            previous_digest: None,
            entries: vec![PendingVoteBackupEntryV1::Live(record)],
            digest: PendingVoteBackupDigest::from_bytes([0; 32]),
        };
        ledger.sort_entries();
        ledger.digest = ledger.recompute_digest()?;
        Ok(ledger)
    }

    /// Parses and fully validates a plaintext ledger.
    pub fn from_json(json: &str) -> Result<Self, VotingError> {
        let ledger: Self =
            serde_json::from_str(json).map_err(|error| VotingError::InvalidInput {
                message: format!("invalid pending vote backup JSON: {error}"),
            })?;
        ledger.validate()?;
        Ok(ledger)
    }

    /// Serializes the canonical plaintext. The caller must encrypt and authenticate it.
    pub fn to_json(&self) -> Result<String, VotingError> {
        self.validate()?;
        serde_json::to_string(self).map_err(|error| VotingError::Internal {
            message: format!("failed to serialize pending vote backup: {error}"),
        })
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub const fn digest(&self) -> PendingVoteBackupDigest {
        self.digest
    }

    pub const fn previous_digest(&self) -> Option<PendingVoteBackupDigest> {
        self.previous_digest
    }

    pub fn entries(&self) -> &[PendingVoteBackupEntryV1] {
        &self.entries
    }

    /// Requires this plaintext to match the independently stored rollback head.
    pub fn validate_head(
        &self,
        revision: u64,
        digest: PendingVoteBackupDigest,
    ) -> Result<(), VotingError> {
        self.validate()?;
        if self.revision != revision || self.digest != digest {
            return Err(VotingError::InvalidInput {
                message: "pending vote backup does not match the rollback-protected head"
                    .to_string(),
            });
        }
        Ok(())
    }

    /// Adds or refreshes one live record while retaining every unrelated entry.
    ///
    /// A tombstone is permanent. Reusing its record identifier is rejected.
    pub fn successor_with_record(
        &self,
        record: PendingVoteBackupRecordV1,
    ) -> Result<Self, VotingError> {
        self.validate()?;
        validate_pending_record(&record)?;
        let record_id = record.binding.record_id;
        let mut entries = self.entries.clone();
        match entries
            .iter_mut()
            .find(|entry| entry.binding().record_id == record_id)
        {
            Some(PendingVoteBackupEntryV1::Live(current)) => {
                ensure_same_immutable_binding(&current.binding, &record.binding)?;
                validate_record_successor(current, &record)?;
                *current = record;
            }
            Some(PendingVoteBackupEntryV1::Retired(_)) => {
                return Err(VotingError::InvalidInput {
                    message: "retired pending vote backup record cannot be resurrected".to_string(),
                });
            }
            None => entries.push(PendingVoteBackupEntryV1::Live(record)),
        }
        self.successor(entries)
    }

    /// Retires one record only after completion or a safe pre-submit replacement.
    pub fn successor_with_retirement(
        &self,
        record_id: PendingVoteBackupDigest,
        retirement: PendingVoteBackupRetirementV1,
    ) -> Result<Self, VotingError> {
        self.validate()?;
        let mut entries = self.entries.clone();
        let index = entries
            .iter()
            .position(|entry| entry.binding().record_id == record_id)
            .ok_or_else(|| VotingError::InvalidInput {
                message: "pending vote backup record to retire was not found".to_string(),
            })?;
        let PendingVoteBackupEntryV1::Live(record) = &entries[index] else {
            return Err(VotingError::InvalidInput {
                message: "pending vote backup record is already retired".to_string(),
            });
        };
        validate_retirement(record, &retirement, &entries)?;
        let replacement_evidence = match &retirement {
            PendingVoteBackupRetirementV1::EveryExpectedShareConfirmed => None,
            PendingVoteBackupRetirementV1::ReplacedBeforeSubmission { .. } => {
                Some(replacement_evidence(record)?)
            }
        };
        let tombstone = PendingVoteBackupTombstoneV1 {
            binding: record.binding.clone(),
            retired_record_digest: digest_serializable(record)?,
            retirement,
            replacement_evidence,
        };
        entries[index] = PendingVoteBackupEntryV1::Retired(tombstone);
        self.successor(entries)
    }

    /// Verifies that `next` is the unique immediate successor of this ledger.
    pub fn validate_immediate_successor(&self, next: &Self) -> Result<(), VotingError> {
        self.validate()?;
        next.validate()?;
        if next.revision
            != self
                .revision
                .checked_add(1)
                .ok_or_else(|| VotingError::InvalidInput {
                    message: "pending vote backup revision overflow".to_string(),
                })?
            || next.previous_digest != Some(self.digest)
        {
            return Err(VotingError::InvalidInput {
                message: "pending vote backup is not the immediate digest-linked successor"
                    .to_string(),
            });
        }
        Ok(())
    }

    pub(crate) fn live_record(
        &self,
        record_id: PendingVoteBackupDigest,
    ) -> Result<&PendingVoteBackupRecordV1, VotingError> {
        self.validate()?;
        match self
            .entries
            .iter()
            .find(|entry| entry.binding().record_id == record_id)
        {
            Some(PendingVoteBackupEntryV1::Live(record)) => Ok(record),
            Some(PendingVoteBackupEntryV1::Retired(_)) => Err(VotingError::InvalidInput {
                message: "pending vote backup record is retired".to_string(),
            }),
            None => Err(VotingError::InvalidInput {
                message: "pending vote backup record was not found".to_string(),
            }),
        }
    }

    fn successor(&self, mut entries: Vec<PendingVoteBackupEntryV1>) -> Result<Self, VotingError> {
        let revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| VotingError::InvalidInput {
                message: "pending vote backup revision overflow".to_string(),
            })?;
        entries.sort_by_key(|entry| entry.binding().record_id);
        let mut next = Self {
            format: PENDING_VOTE_BACKUP_FORMAT_V1.to_string(),
            revision,
            previous_digest: Some(self.digest),
            entries,
            digest: PendingVoteBackupDigest::from_bytes([0; 32]),
        };
        next.digest = next.recompute_digest()?;
        Ok(next)
    }

    fn validate(&self) -> Result<(), VotingError> {
        if self.format != PENDING_VOTE_BACKUP_FORMAT_V1 {
            return Err(VotingError::InvalidInput {
                message: format!("unsupported pending vote backup format: {}", self.format),
            });
        }
        if self.revision == 0 {
            return Err(VotingError::InvalidInput {
                message: "pending vote backup revision must be positive".to_string(),
            });
        }
        if self.entries.is_empty() {
            return Err(VotingError::InvalidInput {
                message: "pending vote backup must retain at least one live record or tombstone"
                    .to_string(),
            });
        }
        let mut previous_id = None;
        for entry in &self.entries {
            let record_id = entry.binding().record_id;
            if previous_id.is_some_and(|previous| previous >= record_id) {
                return Err(VotingError::InvalidInput {
                    message: "pending vote backup entries must be uniquely sorted by record_id"
                        .to_string(),
                });
            }
            previous_id = Some(record_id);
            match entry {
                PendingVoteBackupEntryV1::Live(record) => validate_pending_record(record)?,
                PendingVoteBackupEntryV1::Retired(tombstone) => validate_tombstone(tombstone)?,
            }
        }
        for entry in &self.entries {
            if let PendingVoteBackupEntryV1::Retired(tombstone) = entry {
                validate_replacement_reference(tombstone, &self.entries)?;
            }
        }
        let recomputed = self.recompute_digest()?;
        if recomputed != self.digest {
            return Err(VotingError::InvalidInput {
                message: "pending vote backup digest does not match its contents".to_string(),
            });
        }
        Ok(())
    }

    fn sort_entries(&mut self) {
        self.entries.sort_by_key(|entry| entry.binding().record_id);
    }

    fn recompute_digest(&self) -> Result<PendingVoteBackupDigest, VotingError> {
        digest_serializable(&PendingVoteBackupLedgerDigestInput {
            format: &self.format,
            revision: self.revision,
            previous_digest: self.previous_digest,
            entries: &self.entries,
        })
    }
}

/// Refreshes one live record from the exact durable vote and helper rows.
///
/// Immutable authority, source, fleet, and placement-plan fields come from
/// `template`; only monotonic storage-owned observations are refreshed.
pub fn refresh_pending_vote_backup_record_v1(
    db: &crate::round::VotingDb,
    template: &PendingVoteBackupRecordV1,
) -> Result<PendingVoteBackupRecordV1, VotingError> {
    validate_pending_record(template)?;
    let wallet_id = db.wallet_id();
    let conn = db.conn();
    let mut refreshed = template.clone();

    for action in &mut refreshed.actions {
        let stored: Option<(String, Option<String>, Option<u64>, u64)> = conn
            .query_row(
                "SELECT commitment_bundle_json, tx_hash, vc_tree_position, created_at
                 FROM votes
                 WHERE round_id = :round_id AND wallet_id = :wallet_id
                   AND bundle_index = :bundle_index AND proposal_id = :proposal_id",
                named_params! {
                    ":round_id": &template.binding.round_id,
                    ":wallet_id": &wallet_id,
                    ":bundle_index": template.binding.bundle_index,
                    ":proposal_id": action.proposal_id,
                },
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|error| VotingError::Internal {
                message: format!("failed to load pending vote backup action: {error}"),
            })?;
        let Some((recovery_json, tx_hash, confirmed_vc_tree_position, created_at)) = stored else {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "pending vote action is missing from storage for proposal {}",
                    action.proposal_id
                ),
            });
        };
        ensure_same_recovery_generation(&action.recovery_json, &recovery_json)?;
        action.recovery_json = recovery_json;
        action.tx_hash = tx_hash;
        action.confirmed_vc_tree_position = confirmed_vc_tree_position;
        action.created_at = created_at;
    }

    for helper_share in &mut refreshed.helper_shares {
        let stored: Option<(String, String, String, u32, Vec<u8>, bool, u64, u64)> = conn
            .query_row(
                "SELECT sent_to_urls, ambiguous_urls, attempting_urls, target_count,
                        nullifier, confirmed, submit_at, created_at
                 FROM share_delegations
                 WHERE round_id = :round_id AND wallet_id = :wallet_id
                   AND bundle_index = :bundle_index AND proposal_id = :proposal_id
                   AND share_index = :share_index",
                named_params! {
                    ":round_id": &template.binding.round_id,
                    ":wallet_id": &wallet_id,
                    ":bundle_index": template.binding.bundle_index,
                    ":proposal_id": helper_share.proposal_id,
                    ":share_index": helper_share.share_index,
                },
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| VotingError::Internal {
                message: format!("failed to load pending helper backup row: {error}"),
            })?;
        helper_share.delivery = stored
            .map(
                |(
                    accepted_json,
                    ambiguous_json,
                    attempting_json,
                    target_count,
                    nullifier,
                    confirmed,
                    submit_at,
                    created_at,
                )| {
                    Ok(PendingHelperDeliveryBackupV1 {
                        accepted_urls: parse_backup_url_json(&accepted_json, "sent_to_urls")?,
                        ambiguous_urls: parse_backup_url_json(&ambiguous_json, "ambiguous_urls")?,
                        attempting_urls: parse_backup_url_json(
                            &attempting_json,
                            "attempting_urls",
                        )?,
                        target_count,
                        nullifier,
                        confirmed,
                        submit_at,
                        created_at,
                    })
                },
            )
            .transpose()?;
    }
    validate_record_successor(template, &refreshed)?;
    validate_pending_record(&refreshed)?;
    Ok(refreshed)
}

/// A caller-owned durability acknowledgement used by recoverable helper APIs.
///
/// The callback must atomically encrypt, authenticate, and replace the
/// external ledger before returning `Ok(())`. A callback error leaves the
/// supplied in-memory ledger unchanged and stops the protected helper flow.
pub struct PendingVoteBackupCheckpointV1<'a> {
    ledger: &'a mut PendingVoteBackupLedgerV1,
    record_id: PendingVoteBackupDigest,
    persist: &'a mut (dyn FnMut(&PendingVoteBackupLedgerV1) -> Result<(), VotingError> + Send),
    activated: bool,
}

impl<'a> PendingVoteBackupCheckpointV1<'a> {
    /// Selects one live record in the caller-owned ledger.
    pub fn new(
        ledger: &'a mut PendingVoteBackupLedgerV1,
        record_id: PendingVoteBackupDigest,
        persist: &'a mut (dyn FnMut(&PendingVoteBackupLedgerV1) -> Result<(), VotingError> + Send),
    ) -> Result<Self, VotingError> {
        ledger.live_record(record_id)?;
        Ok(Self {
            ledger,
            record_id,
            persist,
            activated: false,
        })
    }

    /// Protects the selected actions from generic cleanup before first use.
    pub(crate) fn activate(&mut self, db: &crate::round::VotingDb) -> Result<(), VotingError> {
        if self.activated {
            return Ok(());
        }
        protect_live_pending_record(db, self.ledger, self.record_id)?;
        self.activated = true;
        Ok(())
    }

    /// Captures and acknowledges one storage transition before execution may
    /// continue to a helper POST, another helper, or a successful return.
    pub fn checkpoint(&mut self, db: &crate::round::VotingDb) -> Result<(), VotingError> {
        self.activate(db)?;
        let refreshed =
            refresh_pending_vote_backup_record_v1(db, self.ledger.live_record(self.record_id)?)?;
        let successor = self.ledger.successor_with_record(refreshed)?;
        (self.persist)(&successor)?;
        *self.ledger = successor;
        Ok(())
    }

    pub(crate) fn validate_share_request(
        &mut self,
        round_id: &str,
        bundle_index: u32,
        proposal_id: u32,
        share_index: u32,
        plan: &ShareSubmissionPlan,
        configured_fleet: &[String],
    ) -> Result<(), VotingError> {
        self.acknowledge_configured_fleet(configured_fleet)?;
        let record = self.ledger.live_record(self.record_id)?;
        if record.binding.round_id != round_id
            || record.binding.bundle_index != bundle_index
            || record
                .helper_shares
                .iter()
                .find(|share| share.proposal_id == proposal_id && share.share_index == share_index)
                .is_none_or(|share| share.original_plan != *plan)
        {
            return Err(VotingError::InvalidInput {
                message: "pending backup checkpoint does not match the committed share request"
                    .to_string(),
            });
        }
        Ok(())
    }

    pub(crate) fn validate_tracking_request(
        &mut self,
        round_id: &str,
        configured_fleet: &[String],
    ) -> Result<(), VotingError> {
        self.acknowledge_configured_fleet(configured_fleet)?;
        let record = self.ledger.live_record(self.record_id)?;
        if record.binding.round_id != round_id {
            return Err(VotingError::InvalidInput {
                message: "pending backup checkpoint does not match the tracked round".to_string(),
            });
        }
        Ok(())
    }

    pub(crate) fn contains_share(
        &self,
        bundle_index: u32,
        proposal_id: u32,
        share_index: u32,
    ) -> Result<bool, VotingError> {
        let record = self.ledger.live_record(self.record_id)?;
        Ok(record.binding.bundle_index == bundle_index
            && record
                .helper_shares
                .iter()
                .any(|share| share.proposal_id == proposal_id && share.share_index == share_index))
    }

    fn acknowledge_configured_fleet(
        &mut self,
        configured_fleet: &[String],
    ) -> Result<(), VotingError> {
        let canonical = canonical_helper_url_list(configured_fleet)?;
        if canonical.is_empty() || canonical.len() != configured_fleet.len() {
            return Err(VotingError::InvalidInput {
                message: "pending backup helper fleet must be nonempty and canonically distinct"
                    .to_string(),
            });
        }
        let mut record = self.ledger.live_record(self.record_id)?.clone();
        let mut changed = false;
        for url in canonical {
            if !record.helper_fleet_history.contains(&url) {
                record.helper_fleet_history.push(url);
                changed = true;
            }
        }
        if changed {
            record.helper_fleet_history.sort();
            let successor = self.ledger.successor_with_record(record)?;
            (self.persist)(&successor)?;
            *self.ledger = successor;
        }
        Ok(())
    }
}

/// Restores all live entries in one externally authenticated ledger.
///
/// The caller supplies its independently protected `(revision, digest)` head.
/// Validation and every vote/share merge happen in one immediate transaction.
/// Existing accepted, ambiguous, confirmed, submitted, or confirmed-position
/// evidence is never downgraded.
pub fn import_pending_vote_backup_ledger_v1(
    db: &crate::round::VotingDb,
    ledger: &PendingVoteBackupLedgerV1,
    expected_bindings: &[PendingVoteBackupExpectedBindingV1],
    protected_revision: u64,
    protected_digest: PendingVoteBackupDigest,
) -> Result<(), VotingError> {
    ledger.validate_head(protected_revision, protected_digest)?;
    let wallet_id = db.wallet_id();
    let mut conn = db.conn();
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| VotingError::Internal {
            message: format!("begin pending vote backup import failed: {error}"),
        })?;
    validate_import_head(&tx, &wallet_id, ledger)?;
    for tombstone in replacement_tombstones_in_application_order(ledger)? {
        if !expected_bindings
            .iter()
            .any(|expected| expected.matches(&tombstone.binding))
        {
            return Err(VotingError::InvalidInput {
                message: "pending vote replacement does not match an expected authority and bundle binding"
                    .to_string(),
            });
        }
        remove_exact_pristine_replacement(&tx, &wallet_id, ledger, tombstone)?;
    }
    for entry in ledger.entries() {
        let PendingVoteBackupEntryV1::Live(record) = entry else {
            continue;
        };
        if !expected_bindings
            .iter()
            .any(|expected| expected.matches(&record.binding))
        {
            return Err(VotingError::InvalidInput {
                message:
                    "pending vote backup does not match an expected authority and bundle binding"
                        .to_string(),
            });
        }
        import_live_record(&tx, &wallet_id, record)?;
        protect_record_with_conn(&tx, &wallet_id, ledger, record, false)?;
    }
    for entry in ledger.entries() {
        if let PendingVoteBackupEntryV1::Retired(tombstone) = entry {
            tx.execute(
                "UPDATE pending_vote_backup_protection
                 SET retired = 1, ledger_revision = :revision, ledger_digest = :digest
                 WHERE wallet_id = :wallet_id AND record_id = :record_id",
                named_params! {
                    ":wallet_id": &wallet_id,
                    ":record_id": tombstone.binding.record_id.as_bytes().as_slice(),
                    ":revision": ledger.revision(),
                    ":digest": ledger.digest().as_bytes().as_slice(),
                },
            )
            .map_err(|error| VotingError::Internal {
                message: format!("retire pending vote protection failed: {error}"),
            })?;
        }
    }
    tx.execute(
        "INSERT INTO pending_vote_backup_heads (wallet_id, revision, digest)
         VALUES (:wallet_id, :revision, :digest)
         ON CONFLICT(wallet_id) DO UPDATE SET revision = excluded.revision, digest = excluded.digest",
        named_params! {
            ":wallet_id": &wallet_id,
            ":revision": ledger.revision(),
            ":digest": ledger.digest().as_bytes().as_slice(),
        },
    )
    .map_err(|error| VotingError::Internal {
        message: format!("store pending vote backup head failed: {error}"),
    })?;
    tx.commit().map_err(|error| VotingError::Internal {
        message: format!("commit pending vote backup import failed: {error}"),
    })
}

fn protect_live_pending_record(
    db: &crate::round::VotingDb,
    ledger: &PendingVoteBackupLedgerV1,
    record_id: PendingVoteBackupDigest,
) -> Result<(), VotingError> {
    let wallet_id = db.wallet_id();
    let record = ledger.live_record(record_id)?;
    let mut conn = db.conn();
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| VotingError::Internal {
            message: format!("begin pending vote protection transaction failed: {error}"),
        })?;
    protect_record_with_conn(&tx, &wallet_id, ledger, record, false)?;
    tx.commit().map_err(|error| VotingError::Internal {
        message: format!("commit pending vote protection transaction failed: {error}"),
    })
}

fn protect_record_with_conn(
    conn: &rusqlite::Connection,
    wallet_id: &str,
    ledger: &PendingVoteBackupLedgerV1,
    record: &PendingVoteBackupRecordV1,
    retired: bool,
) -> Result<(), VotingError> {
    for action in &record.actions {
        conn.execute(
            "INSERT INTO pending_vote_backup_protection
             (wallet_id, record_id, round_id, bundle_index, proposal_id, retired,
              ledger_revision, ledger_digest)
             VALUES (:wallet_id, :record_id, :round_id, :bundle_index, :proposal_id,
                     :retired, :revision, :digest)
             ON CONFLICT(wallet_id, record_id, proposal_id) DO UPDATE SET
                 retired = MAX(pending_vote_backup_protection.retired, excluded.retired),
                 ledger_revision = MAX(pending_vote_backup_protection.ledger_revision, excluded.ledger_revision),
                 ledger_digest = CASE
                     WHEN excluded.ledger_revision >= pending_vote_backup_protection.ledger_revision
                     THEN excluded.ledger_digest ELSE pending_vote_backup_protection.ledger_digest END",
            named_params! {
                ":wallet_id": wallet_id,
                ":record_id": record.binding.record_id.as_bytes().as_slice(),
                ":round_id": &record.binding.round_id,
                ":bundle_index": record.binding.bundle_index,
                ":proposal_id": action.proposal_id,
                ":retired": retired,
                ":revision": ledger.revision(),
                ":digest": ledger.digest().as_bytes().as_slice(),
            },
        )
        .map_err(|error| VotingError::Internal {
            message: format!("protect pending vote backup action failed: {error}"),
        })?;
    }
    Ok(())
}

fn validate_import_head(
    conn: &rusqlite::Connection,
    wallet_id: &str,
    ledger: &PendingVoteBackupLedgerV1,
) -> Result<(), VotingError> {
    let stored: Option<(u64, Vec<u8>)> = conn
        .query_row(
            "SELECT revision, digest FROM pending_vote_backup_heads WHERE wallet_id = ?1",
            [wallet_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| VotingError::Internal {
            message: format!("load pending vote backup head failed: {error}"),
        })?;
    let Some((revision, digest)) = stored else {
        return Ok(());
    };
    if revision > ledger.revision()
        || (revision == ledger.revision() && digest != ledger.digest().as_bytes())
    {
        return Err(VotingError::InvalidInput {
            message:
                "pending vote backup import conflicts with or rolls back the stored digest head"
                    .to_string(),
        });
    }
    Ok(())
}

fn replacement_tombstones_in_application_order(
    ledger: &PendingVoteBackupLedgerV1,
) -> Result<Vec<&PendingVoteBackupTombstoneV1>, VotingError> {
    let tombstones = ledger
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            PendingVoteBackupEntryV1::Retired(tombstone)
                if matches!(
                    &tombstone.retirement,
                    PendingVoteBackupRetirementV1::ReplacedBeforeSubmission { .. }
                ) =>
            {
                Some((tombstone.binding.record_id, tombstone))
            }
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let mut predecessor_counts = tombstones
        .keys()
        .map(|record_id| (*record_id, 0_usize))
        .collect::<BTreeMap<_, _>>();
    for tombstone in tombstones.values() {
        let PendingVoteBackupRetirementV1::ReplacedBeforeSubmission {
            replacement_record_id,
        } = &tombstone.retirement
        else {
            continue;
        };
        if let Some(count) = predecessor_counts.get_mut(replacement_record_id) {
            *count = count.checked_add(1).ok_or_else(|| VotingError::Internal {
                message: "pending vote replacement dependency count overflow".to_string(),
            })?;
        }
    }

    let mut ready = predecessor_counts
        .iter()
        .filter_map(|(record_id, count)| (*count == 0).then_some(*record_id))
        .collect::<BTreeSet<_>>();
    let mut ordered = Vec::with_capacity(tombstones.len());
    while let Some(record_id) = ready.iter().next().copied() {
        ready.remove(&record_id);
        let tombstone =
            tombstones
                .get(&record_id)
                .copied()
                .ok_or_else(|| VotingError::Internal {
                    message: "pending vote replacement dependency disappeared".to_string(),
                })?;
        ordered.push(tombstone);

        let PendingVoteBackupRetirementV1::ReplacedBeforeSubmission {
            replacement_record_id,
        } = &tombstone.retirement
        else {
            continue;
        };
        if let Some(count) = predecessor_counts.get_mut(replacement_record_id) {
            *count = count.checked_sub(1).ok_or_else(|| VotingError::Internal {
                message: "pending vote replacement dependency underflow".to_string(),
            })?;
            if *count == 0 {
                ready.insert(*replacement_record_id);
            }
        }
    }
    if ordered.len() != tombstones.len() {
        return Err(VotingError::InvalidInput {
            message: "pending vote replacement tombstones contain a cycle".to_string(),
        });
    }
    Ok(ordered)
}

struct StoredPristineReplacementAction {
    proposal_id: u32,
    vote_decision: u32,
}

fn remove_exact_pristine_replacement(
    conn: &rusqlite::Connection,
    wallet_id: &str,
    ledger: &PendingVoteBackupLedgerV1,
    tombstone: &PendingVoteBackupTombstoneV1,
) -> Result<(), VotingError> {
    let evidence =
        tombstone
            .replacement_evidence
            .as_ref()
            .ok_or_else(|| VotingError::InvalidInput {
                message: "replacement tombstone is missing retired vote generation evidence"
                    .to_string(),
            })?;
    let protections = {
        let mut statement = conn
            .prepare(
                "SELECT round_id, bundle_index, proposal_id, retired
                 FROM pending_vote_backup_protection
                 WHERE wallet_id = :wallet_id AND record_id = :record_id",
            )
            .map_err(|error| VotingError::Internal {
                message: format!("prepare retired pending vote protection lookup failed: {error}"),
            })?;
        let rows = statement
            .query_map(
                named_params! {
                    ":wallet_id": wallet_id,
                    ":record_id": tombstone.binding.record_id.as_bytes().as_slice(),
                },
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, u32>(2)?,
                        row.get::<_, bool>(3)?,
                    ))
                },
            )
            .map_err(|error| VotingError::Internal {
                message: format!("load retired pending vote protections failed: {error}"),
            })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| VotingError::Internal {
                message: format!("read retired pending vote protections failed: {error}"),
            })?
    };

    let exact_protection_set = protections.len() == evidence.actions.len()
        && evidence.actions.iter().all(|action| {
            protections
                .iter()
                .any(|(round_id, bundle_index, proposal_id, _)| {
                    round_id == &tombstone.binding.round_id
                        && *bundle_index == tombstone.binding.bundle_index
                        && *proposal_id == action.proposal_id
                })
        });
    if exact_protection_set && protections.iter().all(|(_, _, _, retired)| *retired) {
        // Retirement and successor import commit in the same transaction. An
        // exact retired protection set therefore proves that this tombstone
        // was already applied. Later imports still validate and merge the live
        // replacement record below the tombstone pass.
        return Ok(());
    }
    type StoredVote = (
        u32,
        Option<Vec<u8>>,
        u64,
        Option<String>,
        Option<u64>,
        Option<String>,
    );
    let mut stored_votes = Vec::with_capacity(evidence.actions.len());
    let mut stored_helper_share_indices = Vec::with_capacity(evidence.actions.len());
    for action in &evidence.actions {
        let stored: Option<StoredVote> = conn
            .query_row(
                "SELECT choice, commitment, created_at, tx_hash, vc_tree_position,
                        commitment_bundle_json
                 FROM votes
                 WHERE round_id = :round_id AND wallet_id = :wallet_id
                   AND bundle_index = :bundle_index AND proposal_id = :proposal_id",
                named_params! {
                    ":round_id": &tombstone.binding.round_id,
                    ":wallet_id": wallet_id,
                    ":bundle_index": tombstone.binding.bundle_index,
                    ":proposal_id": action.proposal_id,
                },
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| VotingError::Internal {
                message: format!("load pristine retired vote failed: {error}"),
            })?;
        stored_votes.push(stored);
        let action_helper_share_indices = {
            let mut statement = conn
                .prepare(
                    "SELECT share_index FROM share_delegations
                 WHERE round_id = :round_id AND wallet_id = :wallet_id
                   AND bundle_index = :bundle_index AND proposal_id = :proposal_id",
                )
                .map_err(|error| VotingError::Internal {
                    message: format!("prepare pristine retired helper rows failed: {error}"),
                })?;
            let rows = statement
                .query_map(
                    named_params! {
                        ":round_id": &tombstone.binding.round_id,
                        ":wallet_id": wallet_id,
                        ":bundle_index": tombstone.binding.bundle_index,
                        ":proposal_id": action.proposal_id,
                    },
                    |row| row.get::<_, u32>(0),
                )
                .map_err(|error| VotingError::Internal {
                    message: format!("load pristine retired helper rows failed: {error}"),
                })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| VotingError::Internal {
                    message: format!("read pristine retired helper rows failed: {error}"),
                })?
        };
        stored_helper_share_indices.push(action_helper_share_indices);
    }
    let vote_count = stored_votes.iter().filter(|vote| vote.is_some()).count();
    let helper_row_count = stored_helper_share_indices
        .iter()
        .map(Vec::len)
        .sum::<usize>();
    if protections.is_empty() {
        if vote_count == 0 && helper_row_count == 0 {
            return Ok(());
        }

        if let Some(live_replacement) = resolve_live_replacement_record(ledger, tombstone)? {
            let mut matches_live_replacement = true;
            for ((action, stored), helper_share_indices) in evidence
                .actions
                .iter()
                .zip(&stored_votes)
                .zip(&stored_helper_share_indices)
            {
                let live_action = live_replacement
                    .actions
                    .iter()
                    .find(|candidate| candidate.proposal_id == action.proposal_id);
                if let Some((_, _, created_at, _, _, recovery_json)) = stored.as_ref() {
                    let Some(live_action) = live_action else {
                        matches_live_replacement = false;
                        break;
                    };
                    let Some(recovery_json) = recovery_json.as_ref() else {
                        matches_live_replacement = false;
                        break;
                    };
                    if *created_at != live_action.created_at
                        || recovery_generation_digest(recovery_json)?
                            != recovery_generation_digest(&live_action.recovery_json)?
                    {
                        matches_live_replacement = false;
                        break;
                    }
                }
                if helper_share_indices.iter().any(|share_index| {
                    !live_replacement.helper_shares.iter().any(|helper_share| {
                        helper_share.proposal_id == action.proposal_id
                            && helper_share.share_index == *share_index
                            && helper_share.delivery.is_some()
                    })
                }) {
                    matches_live_replacement = false;
                    break;
                }
                if live_action.is_none() && (stored.is_some() || !helper_share_indices.is_empty()) {
                    matches_live_replacement = false;
                    break;
                }
            }
            if matches_live_replacement {
                // A replay can encounter the already imported terminal live
                // generation without its skipped intermediate protection. The
                // live record pass below validates every retained row and
                // completes any missing actions before the transaction commits.
                return Ok(());
            }
        }
        return Err(VotingError::InvalidInput {
            message: "pending vote replacement found partial or unprotected retired local state"
                .to_string(),
        });
    }
    if protections.len() != evidence.actions.len() || vote_count != evidence.actions.len() {
        return Err(VotingError::InvalidInput {
            message: "pending vote replacement found partial or unprotected retired local state"
                .to_string(),
        });
    }
    for action in &evidence.actions {
        let exact_protection =
            protections
                .iter()
                .any(|(round_id, bundle_index, proposal_id, retired)| {
                    round_id == &tombstone.binding.round_id
                        && *bundle_index == tombstone.binding.bundle_index
                        && *proposal_id == action.proposal_id
                        && !retired
                });
        if !exact_protection {
            return Err(VotingError::InvalidInput {
                message: "pending vote replacement does not match live protection for the exact retired record"
                    .to_string(),
            });
        }
    }

    let mut pristine_actions = Vec::with_capacity(evidence.actions.len());
    for (action, stored) in evidence.actions.iter().zip(stored_votes) {
        let Some((choice, commitment, created_at, tx_hash, position, recovery_json)) = stored
        else {
            return Err(VotingError::InvalidInput {
                message:
                    "pending vote replacement is missing an action from the exact retired record"
                        .to_string(),
            });
        };
        if tx_hash.is_some() || position.is_some() {
            return Err(VotingError::InvalidInput {
                message:
                    "pending vote replacement cannot remove submitted or confirmed local evidence"
                        .to_string(),
            });
        }
        let recovery_json = recovery_json.ok_or_else(|| VotingError::InvalidInput {
            message: "pending vote replacement cannot identify local recovery generation"
                .to_string(),
        })?;
        let recovery = parse_recovery(&recovery_json)?;
        if recovery.vote_round_id != tombstone.binding.round_id
            || recovery.bundle_index != tombstone.binding.bundle_index
            || recovery.proposal_id != action.proposal_id
            || recovery.vc_tree_position != 0
            || recovery.vote_decision != choice
            || commitment
                .as_ref()
                .is_some_and(|stored| stored.as_slice() != recovery.vote_commitment.as_slice())
            || created_at != action.created_at
            || recovery_generation_digest(&recovery_json)? != action.recovery_generation_digest
        {
            return Err(VotingError::InvalidInput {
                message:
                    "pending vote replacement does not match the exact retired vote generation"
                        .to_string(),
            });
        }
        validate_pristine_replacement_helper_rows(
            conn,
            wallet_id,
            &tombstone.binding,
            action.proposal_id,
            &recovery_json,
        )?;
        pristine_actions.push(StoredPristineReplacementAction {
            proposal_id: action.proposal_id,
            vote_decision: recovery.vote_decision,
        });
    }

    let live_replacement = resolve_live_replacement_record(ledger, tombstone)?;
    for action in pristine_actions {
        let replacement_decision = live_replacement
            .and_then(|replacement| {
                replacement
                    .actions
                    .iter()
                    .find(|candidate| candidate.proposal_id == action.proposal_id)
            })
            .map(|replacement| parse_recovery(&replacement.recovery_json))
            .transpose()?
            .map(|replacement| replacement.vote_decision);
        if replacement_decision.is_some_and(|decision| decision != action.vote_decision) {
            conn.execute(
                "DELETE FROM ballot_intent
                 WHERE round_id = :round_id AND wallet_id = :wallet_id
                   AND proposal_id = :proposal_id AND skipped = 0 AND choice = :choice",
                named_params! {
                    ":round_id": &tombstone.binding.round_id,
                    ":wallet_id": wallet_id,
                    ":proposal_id": action.proposal_id,
                    ":choice": action.vote_decision,
                },
            )
            .map_err(|error| VotingError::Internal {
                message: format!("remove stale retired ballot intent failed: {error}"),
            })?;
        }
        conn.execute(
            "DELETE FROM share_delegations
             WHERE round_id = :round_id AND wallet_id = :wallet_id
               AND bundle_index = :bundle_index AND proposal_id = :proposal_id",
            named_params! {
                ":round_id": &tombstone.binding.round_id,
                ":wallet_id": wallet_id,
                ":bundle_index": tombstone.binding.bundle_index,
                ":proposal_id": action.proposal_id,
            },
        )
        .map_err(|error| VotingError::Internal {
            message: format!("remove pristine retired helper rows failed: {error}"),
        })?;
        let removed = conn
            .execute(
                "DELETE FROM votes
                 WHERE round_id = :round_id AND wallet_id = :wallet_id
                   AND bundle_index = :bundle_index AND proposal_id = :proposal_id",
                named_params! {
                    ":round_id": &tombstone.binding.round_id,
                    ":wallet_id": wallet_id,
                    ":bundle_index": tombstone.binding.bundle_index,
                    ":proposal_id": action.proposal_id,
                },
            )
            .map_err(|error| VotingError::Internal {
                message: format!("remove pristine retired vote failed: {error}"),
            })?;
        if removed != 1 {
            return Err(VotingError::Internal {
                message: "exact pristine retired vote disappeared during replacement".to_string(),
            });
        }
    }
    Ok(())
}

fn validate_pristine_replacement_helper_rows(
    conn: &rusqlite::Connection,
    wallet_id: &str,
    binding: &PendingVoteBackupBindingV1,
    proposal_id: u32,
    recovery_json: &str,
) -> Result<(), VotingError> {
    type StoredShare = (u32, String, String, String, Vec<u8>, bool);
    let shares = {
        let mut statement = conn
            .prepare(
                "SELECT share_index, sent_to_urls, ambiguous_urls, attempting_urls,
                        nullifier, confirmed
                 FROM share_delegations
                 WHERE round_id = :round_id AND wallet_id = :wallet_id
                   AND bundle_index = :bundle_index AND proposal_id = :proposal_id",
            )
            .map_err(|error| VotingError::Internal {
                message: format!("prepare retired helper evidence lookup failed: {error}"),
            })?;
        let rows = statement
            .query_map(
                named_params! {
                    ":round_id": &binding.round_id,
                    ":wallet_id": wallet_id,
                    ":bundle_index": binding.bundle_index,
                    ":proposal_id": proposal_id,
                },
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .map_err(|error| VotingError::Internal {
                message: format!("load retired helper evidence failed: {error}"),
            })?;
        rows.collect::<Result<Vec<StoredShare>, _>>()
            .map_err(|error| VotingError::Internal {
                message: format!("read retired helper evidence failed: {error}"),
            })?
    };
    for (share_index, accepted, ambiguous, attempting, nullifier, confirmed) in shares {
        if confirmed
            || !parse_backup_url_json(&accepted, "sent_to_urls")?.is_empty()
            || !parse_backup_url_json(&ambiguous, "ambiguous_urls")?.is_empty()
            || !parse_backup_url_json(&attempting, "attempting_urls")?.is_empty()
        {
            return Err(VotingError::InvalidInput {
                message: "pending vote replacement cannot remove durable helper outcome evidence"
                    .to_string(),
            });
        }
        let expected =
            crate::share::nullifier_from_recovery_json(recovery_json, proposal_id, share_index)?;
        if nullifier != expected {
            return Err(VotingError::InvalidInput {
                message:
                    "pending vote replacement helper row does not match the retired generation"
                        .to_string(),
            });
        }
    }
    Ok(())
}

fn resolve_live_replacement_record<'a>(
    ledger: &'a PendingVoteBackupLedgerV1,
    tombstone: &PendingVoteBackupTombstoneV1,
) -> Result<Option<&'a PendingVoteBackupRecordV1>, VotingError> {
    let PendingVoteBackupRetirementV1::ReplacedBeforeSubmission {
        replacement_record_id,
    } = &tombstone.retirement
    else {
        return Ok(None);
    };
    let mut next_id = *replacement_record_id;
    let mut visited = BTreeSet::from([tombstone.binding.record_id]);
    loop {
        if !visited.insert(next_id) {
            return Err(VotingError::InvalidInput {
                message: "pending vote replacement tombstones contain a cycle".to_string(),
            });
        }
        let entry = ledger
            .entries()
            .iter()
            .find(|entry| entry.binding().record_id == next_id)
            .ok_or_else(|| VotingError::InvalidInput {
                message: "pending vote replacement target is not retained in the ledger"
                    .to_string(),
            })?;
        match entry {
            PendingVoteBackupEntryV1::Live(record) => return Ok(Some(record)),
            PendingVoteBackupEntryV1::Retired(PendingVoteBackupTombstoneV1 {
                retirement:
                    PendingVoteBackupRetirementV1::ReplacedBeforeSubmission {
                        replacement_record_id,
                    },
                ..
            }) => next_id = *replacement_record_id,
            PendingVoteBackupEntryV1::Retired(_) => return Ok(None),
        }
    }
}

fn import_live_record(
    conn: &rusqlite::Connection,
    wallet_id: &str,
    record: &PendingVoteBackupRecordV1,
) -> Result<(), VotingError> {
    validate_pending_record(record)?;
    for action in &record.actions {
        import_vote_action(conn, wallet_id, record, action)?;
    }
    for helper_share in &record.helper_shares {
        if let Some(delivery) = &helper_share.delivery {
            import_helper_delivery(conn, wallet_id, record, helper_share, delivery)?;
        }
    }
    Ok(())
}

fn import_vote_action(
    conn: &rusqlite::Connection,
    wallet_id: &str,
    record: &PendingVoteBackupRecordV1,
    action: &PendingVoteActionBackupV1,
) -> Result<(), VotingError> {
    let imported_recovery = parse_recovery(&action.recovery_json)?;
    let intent: Option<(bool, Option<u32>)> = conn
        .query_row(
            "SELECT skipped, choice FROM ballot_intent
             WHERE round_id = :round_id AND wallet_id = :wallet_id
               AND proposal_id = :proposal_id",
            named_params! {
                ":round_id": &record.binding.round_id,
                ":wallet_id": wallet_id,
                ":proposal_id": action.proposal_id,
            },
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| VotingError::Internal {
            message: format!("load ballot intent during pending vote import failed: {error}"),
        })?;
    if intent
        .is_some_and(|(skipped, choice)| skipped || choice != Some(imported_recovery.vote_decision))
    {
        return Err(VotingError::InvalidInput {
            message: format!(
                "pending vote import conflicts with ballot intent for proposal {}",
                action.proposal_id
            ),
        });
    }

    type StoredVote = (
        u32,
        Option<Vec<u8>>,
        u64,
        Option<String>,
        Option<u64>,
        Option<String>,
    );
    let stored: Option<StoredVote> = conn
        .query_row(
            "SELECT choice, commitment, created_at, tx_hash, vc_tree_position,
                    commitment_bundle_json
             FROM votes
             WHERE round_id = :round_id AND wallet_id = :wallet_id
               AND bundle_index = :bundle_index AND proposal_id = :proposal_id",
            named_params! {
                ":round_id": &record.binding.round_id,
                ":wallet_id": wallet_id,
                ":bundle_index": record.binding.bundle_index,
                ":proposal_id": action.proposal_id,
            },
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()
        .map_err(|error| VotingError::Internal {
            message: format!("load vote during pending backup import failed: {error}"),
        })?;
    let commitment = imported_recovery.vote_commitment.to_vec();
    if let Some((choice, stored_commitment, created_at, tx_hash, position, recovery_json)) = stored
    {
        if choice != imported_recovery.vote_decision
            || stored_commitment
                .as_ref()
                .is_some_and(|stored| stored != &commitment)
            || created_at != action.created_at
        {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "pending vote import conflicts with stored vote identity for proposal {}",
                    action.proposal_id
                ),
            });
        }
        if let Some(stored_recovery) = &recovery_json {
            ensure_same_recovery_generation(stored_recovery, &action.recovery_json)?;
        }
        let merged_tx_hash = merge_monotonic_option(
            tx_hash,
            action.tx_hash.clone(),
            "pending vote transaction hash conflict",
        )?;
        let merged_position = merge_monotonic_option(
            position,
            action.confirmed_vc_tree_position,
            "pending vote VC position conflict",
        )?;
        let mut merged_recovery = recovery_json
            .as_deref()
            .map(parse_recovery)
            .transpose()?
            .unwrap_or(imported_recovery);
        if let Some(position) = merged_position {
            merged_recovery.vc_tree_position = position;
        }
        let merged_recovery_json = serialize_recovery(&merged_recovery)?;
        conn.execute(
            "UPDATE votes
             SET commitment = COALESCE(commitment, :commitment),
                 tx_hash = :tx_hash,
                 vc_tree_position = :position,
                 commitment_bundle_json = :recovery_json
             WHERE round_id = :round_id AND wallet_id = :wallet_id
               AND bundle_index = :bundle_index AND proposal_id = :proposal_id",
            named_params! {
                ":commitment": commitment,
                ":tx_hash": merged_tx_hash,
                ":position": merged_position,
                ":recovery_json": merged_recovery_json,
                ":round_id": &record.binding.round_id,
                ":wallet_id": wallet_id,
                ":bundle_index": record.binding.bundle_index,
                ":proposal_id": action.proposal_id,
            },
        )
        .map_err(|error| VotingError::Internal {
            message: format!("merge pending vote backup action failed: {error}"),
        })?;
    } else {
        conn.execute(
            "INSERT INTO votes
             (round_id, wallet_id, bundle_index, proposal_id, choice, commitment,
              created_at, tx_hash, vc_tree_position, commitment_bundle_json)
             VALUES (:round_id, :wallet_id, :bundle_index, :proposal_id, :choice,
                     :commitment, :created_at, :tx_hash, :position, :recovery_json)",
            named_params! {
                ":round_id": &record.binding.round_id,
                ":wallet_id": wallet_id,
                ":bundle_index": record.binding.bundle_index,
                ":proposal_id": action.proposal_id,
                ":choice": imported_recovery.vote_decision,
                ":commitment": commitment,
                ":created_at": action.created_at,
                ":tx_hash": &action.tx_hash,
                ":position": action.confirmed_vc_tree_position,
                ":recovery_json": &action.recovery_json,
            },
        )
        .map_err(|error| VotingError::Internal {
            message: format!("restore pending vote backup action failed: {error}"),
        })?;
    }
    Ok(())
}

fn import_helper_delivery(
    conn: &rusqlite::Connection,
    wallet_id: &str,
    record: &PendingVoteBackupRecordV1,
    helper_share: &PendingHelperShareBackupV1,
    imported: &PendingHelperDeliveryBackupV1,
) -> Result<(), VotingError> {
    type StoredDelivery = (String, String, String, u32, Vec<u8>, bool, u64, u64);
    let stored: Option<StoredDelivery> = conn
        .query_row(
            "SELECT sent_to_urls, ambiguous_urls, attempting_urls, target_count,
                    nullifier, confirmed, submit_at, created_at
             FROM share_delegations
             WHERE round_id = :round_id AND wallet_id = :wallet_id
               AND bundle_index = :bundle_index AND proposal_id = :proposal_id
               AND share_index = :share_index",
            named_params! {
                ":round_id": &record.binding.round_id,
                ":wallet_id": wallet_id,
                ":bundle_index": record.binding.bundle_index,
                ":proposal_id": helper_share.proposal_id,
                ":share_index": helper_share.share_index,
            },
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .optional()
        .map_err(|error| VotingError::Internal {
            message: format!("load helper row during pending backup import failed: {error}"),
        })?;

    let (accepted_urls, ambiguous_urls, attempting_urls, target_count, confirmed, submit_at) =
        if let Some((
            accepted_json,
            ambiguous_json,
            attempting_json,
            stored_target,
            stored_nullifier,
            stored_confirmed,
            stored_submit_at,
            stored_created_at,
        )) = stored
        {
            if stored_nullifier != imported.nullifier || stored_created_at != imported.created_at {
                return Err(VotingError::InvalidInput {
                    message: "pending helper import conflicts with the stored share generation"
                        .to_string(),
                });
            }
            let stored_accepted = parse_backup_url_json(&accepted_json, "sent_to_urls")?;
            let stored_ambiguous = parse_backup_url_json(&ambiguous_json, "ambiguous_urls")?;
            let stored_attempting = parse_backup_url_json(&attempting_json, "attempting_urls")?;
            let (accepted, ambiguous, attempting) = merge_url_evidence(
                &stored_accepted,
                &stored_ambiguous,
                &stored_attempting,
                &imported.accepted_urls,
                &imported.ambiguous_urls,
                &imported.attempting_urls,
            );
            (
                accepted,
                ambiguous,
                attempting,
                stored_target.max(imported.target_count),
                stored_confirmed || imported.confirmed,
                merge_submit_at(stored_submit_at, imported.submit_at)?,
            )
        } else {
            (
                imported.accepted_urls.clone(),
                imported.ambiguous_urls.clone(),
                imported.attempting_urls.clone(),
                imported.target_count,
                imported.confirmed,
                imported.submit_at,
            )
        };
    let accepted_json = encode_backup_url_json(&accepted_urls, "sent_to_urls")?;
    let ambiguous_json = encode_backup_url_json(&ambiguous_urls, "ambiguous_urls")?;
    let attempting_json = encode_backup_url_json(&attempting_urls, "attempting_urls")?;
    conn.execute(
        "INSERT INTO share_delegations
         (round_id, wallet_id, bundle_index, proposal_id, share_index, sent_to_urls,
          ambiguous_urls, attempting_urls, target_count, nullifier, confirmed,
          submit_at, created_at)
         VALUES (:round_id, :wallet_id, :bundle_index, :proposal_id, :share_index,
                 :accepted, :ambiguous, :attempting, :target_count, :nullifier,
                 :confirmed, :submit_at, :created_at)
         ON CONFLICT(round_id, wallet_id, bundle_index, proposal_id, share_index)
         DO UPDATE SET sent_to_urls = excluded.sent_to_urls,
                       ambiguous_urls = excluded.ambiguous_urls,
                       attempting_urls = excluded.attempting_urls,
                       target_count = excluded.target_count,
                       confirmed = excluded.confirmed,
                       submit_at = excluded.submit_at",
        named_params! {
            ":round_id": &record.binding.round_id,
            ":wallet_id": wallet_id,
            ":bundle_index": record.binding.bundle_index,
            ":proposal_id": helper_share.proposal_id,
            ":share_index": helper_share.share_index,
            ":accepted": accepted_json,
            ":ambiguous": ambiguous_json,
            ":attempting": attempting_json,
            ":target_count": target_count,
            ":nullifier": &imported.nullifier,
            ":confirmed": confirmed,
            ":submit_at": submit_at,
            ":created_at": imported.created_at,
        },
    )
    .map_err(|error| VotingError::Internal {
        message: format!("merge pending helper backup row failed: {error}"),
    })?;
    Ok(())
}

fn merge_url_evidence(
    stored_accepted: &[String],
    stored_ambiguous: &[String],
    stored_attempting: &[String],
    imported_accepted: &[String],
    imported_ambiguous: &[String],
    imported_attempting: &[String],
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut accepted = Vec::new();
    for url in stored_accepted.iter().chain(imported_accepted) {
        if !accepted.contains(url) {
            accepted.push(url.clone());
        }
    }
    let mut ambiguous = Vec::new();
    for url in stored_ambiguous.iter().chain(imported_ambiguous) {
        if !accepted.contains(url) && !ambiguous.contains(url) {
            ambiguous.push(url.clone());
        }
    }
    let mut attempting = Vec::new();
    for url in stored_attempting.iter().chain(imported_attempting) {
        if !accepted.contains(url) && !ambiguous.contains(url) && !attempting.contains(url) {
            attempting.push(url.clone());
        }
    }
    (accepted, ambiguous, attempting)
}

fn merge_submit_at(stored: u64, imported: u64) -> Result<u64, VotingError> {
    if stored == imported {
        Ok(stored)
    } else if stored == 0 || imported == 0 {
        Ok(0)
    } else {
        Err(VotingError::InvalidInput {
            message: "pending helper import has conflicting write-once schedules".to_string(),
        })
    }
}

fn merge_monotonic_option<T: Eq>(
    stored: Option<T>,
    imported: Option<T>,
    conflict: &str,
) -> Result<Option<T>, VotingError> {
    match (stored, imported) {
        (Some(stored), Some(imported)) if stored != imported => Err(VotingError::InvalidInput {
            message: conflict.to_string(),
        }),
        (Some(stored), _) => Ok(Some(stored)),
        (_, Some(imported)) => Ok(Some(imported)),
        (None, None) => Ok(None),
    }
}

fn parse_backup_url_json(json: &str, field: &str) -> Result<Vec<String>, VotingError> {
    serde_json::from_str(json).map_err(|error| VotingError::Internal {
        message: format!("failed to parse stored {field} for pending backup: {error}"),
    })
}

fn encode_backup_url_json(urls: &[String], field: &str) -> Result<String, VotingError> {
    serde_json::to_string(urls).map_err(|error| VotingError::Internal {
        message: format!("failed to encode restored {field}: {error}"),
    })
}

fn ensure_same_recovery_generation(current: &str, next: &str) -> Result<(), VotingError> {
    let mut current = parse_recovery(current)?;
    let mut next = parse_recovery(next)?;
    current.vc_tree_position = 0;
    next.vc_tree_position = 0;
    if serialize_recovery(&current)? != serialize_recovery(&next)? {
        return Err(VotingError::InvalidInput {
            message: "pending vote recovery material changed for an existing record".to_string(),
        });
    }
    Ok(())
}

fn validate_record_successor(
    current: &PendingVoteBackupRecordV1,
    next: &PendingVoteBackupRecordV1,
) -> Result<(), VotingError> {
    if current.vote_kind != next.vote_kind
        || current.original_helper_fleet != next.original_helper_fleet
        || current
            .helper_fleet_history
            .iter()
            .any(|url| !next.helper_fleet_history.contains(url))
        || current.helper_shares.len() != next.helper_shares.len()
        || current.actions.len() != next.actions.len()
    {
        return Err(VotingError::InvalidInput {
            message: "pending vote backup successor changed immutable plans or actions".to_string(),
        });
    }
    for (old, new) in current.actions.iter().zip(&next.actions) {
        if old.proposal_id != new.proposal_id || old.created_at != new.created_at {
            return Err(VotingError::InvalidInput {
                message: "pending vote backup successor changed an action identity".to_string(),
            });
        }
        ensure_same_recovery_generation(&old.recovery_json, &new.recovery_json)?;
        require_monotonic_option(&old.tx_hash, &new.tx_hash, "transaction hash")?;
        require_monotonic_option(
            &old.confirmed_vc_tree_position,
            &new.confirmed_vc_tree_position,
            "VC position",
        )?;
    }
    for (old, new) in current.helper_shares.iter().zip(&next.helper_shares) {
        if old.proposal_id != new.proposal_id
            || old.share_index != new.share_index
            || old.original_plan != new.original_plan
        {
            return Err(VotingError::InvalidInput {
                message: "pending vote backup successor changed a helper plan".to_string(),
            });
        }
        match (&old.delivery, &new.delivery) {
            (None, _) => {}
            (Some(_), None) => {
                return Err(VotingError::InvalidInput {
                    message: "pending vote backup successor removed a durable helper row"
                        .to_string(),
                });
            }
            (Some(old), Some(new)) => validate_delivery_successor(old, new)?,
        }
    }
    Ok(())
}

fn require_monotonic_option<T: Eq>(
    current: &Option<T>,
    next: &Option<T>,
    field: &str,
) -> Result<(), VotingError> {
    if current
        .as_ref()
        .is_some_and(|current| next.as_ref() != Some(current))
    {
        return Err(VotingError::InvalidInput {
            message: format!("pending vote backup successor downgraded or changed {field}"),
        });
    }
    Ok(())
}

fn validate_delivery_successor(
    current: &PendingHelperDeliveryBackupV1,
    next: &PendingHelperDeliveryBackupV1,
) -> Result<(), VotingError> {
    if current.nullifier != next.nullifier
        || current.created_at != next.created_at
        || current.target_count > next.target_count
        || (current.confirmed && !next.confirmed)
        || (current.submit_at != next.submit_at && !(current.submit_at != 0 && next.submit_at == 0))
        || current
            .accepted_urls
            .iter()
            .any(|url| !next.accepted_urls.contains(url))
        || current
            .ambiguous_urls
            .iter()
            .any(|url| !next.accepted_urls.contains(url) && !next.ambiguous_urls.contains(url))
    {
        return Err(VotingError::InvalidInput {
            message: "pending helper backup successor downgraded durable evidence".to_string(),
        });
    }
    Ok(())
}

fn validate_pending_record(record: &PendingVoteBackupRecordV1) -> Result<(), VotingError> {
    validate_binding(&record.binding)?;
    if record.binding.record_id
        != derive_record_id(&record.binding, &record.vote_kind, &record.actions)?
    {
        return Err(VotingError::InvalidInput {
            message: "pending vote backup record_id does not match its immutable identity"
                .to_string(),
        });
    }
    let canonical_fleet = canonical_helper_url_list(&record.original_helper_fleet)?;
    if canonical_fleet.is_empty()
        || canonical_fleet.len() != record.original_helper_fleet.len()
        || canonical_fleet != record.original_helper_fleet
    {
        return Err(VotingError::InvalidInput {
            message: "pending vote backup original helper fleet must be nonempty, canonical, and distinct"
                .to_string(),
        });
    }
    let canonical_history = canonical_helper_url_list(&record.helper_fleet_history)?;
    if canonical_history.len() != record.helper_fleet_history.len()
        || canonical_history != record.helper_fleet_history
        || record
            .original_helper_fleet
            .iter()
            .any(|url| !canonical_history.contains(url))
    {
        return Err(VotingError::InvalidInput {
            message: "pending vote backup helper fleet history must be canonical and retain the original fleet"
                .to_string(),
        });
    }
    if record.actions.is_empty() {
        return Err(VotingError::InvalidInput {
            message: "pending vote backup must contain at least one action".to_string(),
        });
    }

    let mut recoveries = Vec::with_capacity(record.actions.len());
    let mut proposal_ids = BTreeSet::new();
    for action in &record.actions {
        if !proposal_ids.insert(action.proposal_id) {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "pending vote backup contains duplicate proposal {}",
                    action.proposal_id
                ),
            });
        }
        if action
            .tx_hash
            .as_ref()
            .is_some_and(|hash| hash.trim().is_empty())
        {
            return Err(VotingError::InvalidInput {
                message: "pending vote backup transaction hash must not be empty".to_string(),
            });
        }
        let recovery = parse_recovery(&action.recovery_json)?;
        if recovery.vote_round_id != record.binding.round_id
            || recovery.bundle_index != record.binding.bundle_index
            || recovery.proposal_id != action.proposal_id
        {
            return Err(VotingError::InvalidInput {
                message: "pending vote action does not match its authority/round/bundle binding"
                    .to_string(),
            });
        }
        let position_matches = action
            .confirmed_vc_tree_position
            .map_or(recovery.vc_tree_position == 0, |position| {
                recovery.vc_tree_position == position
            });
        if !position_matches {
            return Err(VotingError::InvalidInput {
                message: "confirmed VC position does not match the canonical recovery bundle"
                    .to_string(),
            });
        }
        if action.confirmed_vc_tree_position.is_some() && action.tx_hash.is_none() {
            return Err(VotingError::InvalidInput {
                message: "a confirmed VC position requires the recorded transaction hash"
                    .to_string(),
            });
        }
        if serialize_recovery(&recovery)? != action.recovery_json {
            return Err(VotingError::InvalidInput {
                message: "pending vote recovery JSON is not canonical".to_string(),
            });
        }
        recoveries.push(recovery);
    }
    validate_vote_kind(&record.vote_kind, &record.binding, &recoveries)?;
    validate_atomic_chain_observations(&record.vote_kind, &record.actions)?;
    validate_helper_shares(record, &recoveries)?;
    Ok(())
}

fn validate_binding(binding: &PendingVoteBackupBindingV1) -> Result<(), VotingError> {
    validate_vote_round_id_hex(&binding.round_id)?;
    for (name, digest) in [
        ("record_id", binding.record_id),
        ("authority_context_digest", binding.authority_context_digest),
        ("authority_source_digest", binding.authority_source_digest),
        ("bundle_source_digest", binding.bundle_source_digest),
    ] {
        if digest.as_bytes() == &[0; 32] {
            return Err(VotingError::InvalidInput {
                message: format!("pending vote backup {name} must not be zero"),
            });
        }
    }
    Ok(())
}

fn ensure_same_immutable_binding(
    current: &PendingVoteBackupBindingV1,
    next: &PendingVoteBackupBindingV1,
) -> Result<(), VotingError> {
    if current != next {
        return Err(VotingError::InvalidInput {
            message: "pending vote backup immutable binding changed for an existing record_id"
                .to_string(),
        });
    }
    Ok(())
}

fn validate_vote_kind(
    vote_kind: &PendingVoteBackupKindV1,
    binding: &PendingVoteBackupBindingV1,
    recoveries: &[VoteRecoveryBundle],
) -> Result<(), VotingError> {
    match vote_kind {
        PendingVoteBackupKindV1::Singleton { proposal_id } => {
            if recoveries.len() != 1
                || recoveries[0].proposal_id != *proposal_id
                || recoveries[0].batch.is_some()
            {
                return Err(VotingError::InvalidInput {
                    message: "pending singleton backup must contain exactly one singleton recovery"
                        .to_string(),
                });
            }
        }
        PendingVoteBackupKindV1::AtomicBatch {
            batch_digest,
            ordered_proposal_ids,
        } => {
            if ordered_proposal_ids.len() < 2 || ordered_proposal_ids.len() != recoveries.len() {
                return Err(VotingError::InvalidInput {
                    message: "pending atomic backup is incomplete".to_string(),
                });
            }
            for (index, (proposal_id, recovery)) in
                ordered_proposal_ids.iter().zip(recoveries).enumerate()
            {
                let Some(batch) = recovery.batch.as_ref() else {
                    return Err(VotingError::InvalidInput {
                        message: "pending atomic backup contains singleton recovery material"
                            .to_string(),
                    });
                };
                if recovery.vote_round_id != binding.round_id
                    || recovery.bundle_index != binding.bundle_index
                    || recovery.proposal_id != *proposal_id
                    || batch.digest != *batch_digest.as_bytes()
                    || batch.index != index as u32
                    || batch.size as usize != recoveries.len()
                {
                    return Err(VotingError::InvalidInput {
                        message: "pending atomic backup is incomplete or out of order".to_string(),
                    });
                }
            }
            let actions = recoveries
                .iter()
                .map(
                    |recovery| crate::vote_commitment::CastVoteBatchSighashAction {
                        r_vpk: &recovery.r_vpk,
                        van_nullifier: &recovery.van_nullifier,
                        vote_authority_note_new: &recovery.vote_authority_note_new,
                        vote_commitment: &recovery.vote_commitment,
                        proposal_id: recovery.proposal_id,
                    },
                )
                .collect::<Vec<_>>();
            let recomputed = crate::vote_commitment::cast_vote_batch_sighash(
                &binding.round_id,
                recoveries[0].anchor_height as u64,
                &actions,
            )?;
            if recomputed != *batch_digest.as_bytes() {
                return Err(VotingError::InvalidInput {
                    message: "pending atomic backup digest does not match its ordered actions"
                        .to_string(),
                });
            }
        }
    }
    Ok(())
}

fn validate_atomic_chain_observations(
    vote_kind: &PendingVoteBackupKindV1,
    actions: &[PendingVoteActionBackupV1],
) -> Result<(), VotingError> {
    if !matches!(vote_kind, PendingVoteBackupKindV1::AtomicBatch { .. }) {
        return Ok(());
    }
    let first_tx_hash = &actions[0].tx_hash;
    if actions
        .iter()
        .any(|action| action.tx_hash != *first_tx_hash)
    {
        return Err(VotingError::InvalidInput {
            message: "pending atomic backup has inconsistent transaction hashes".to_string(),
        });
    }
    let positions_recorded = actions[0].confirmed_vc_tree_position.is_some();
    if actions
        .iter()
        .any(|action| action.confirmed_vc_tree_position.is_some() != positions_recorded)
    {
        return Err(VotingError::InvalidInput {
            message: "pending atomic backup has partial confirmed VC positions".to_string(),
        });
    }
    if positions_recorded
        && actions.windows(2).any(|pair| {
            pair[0]
                .confirmed_vc_tree_position
                .and_then(|position| position.checked_add(1))
                != pair[1].confirmed_vc_tree_position
        })
    {
        return Err(VotingError::InvalidInput {
            message: "pending atomic backup VC positions are not in exact action order".to_string(),
        });
    }
    Ok(())
}

fn validate_helper_shares(
    record: &PendingVoteBackupRecordV1,
    recoveries: &[VoteRecoveryBundle],
) -> Result<(), VotingError> {
    let recovery_by_proposal = recoveries
        .iter()
        .map(|recovery| (recovery.proposal_id, recovery))
        .collect::<BTreeMap<_, _>>();
    let mut shares_by_action: BTreeMap<u32, Vec<&PendingHelperShareBackupV1>> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for helper_share in &record.helper_shares {
        if !seen.insert((helper_share.proposal_id, helper_share.share_index)) {
            return Err(VotingError::InvalidInput {
                message: "pending vote backup contains a duplicate helper share".to_string(),
            });
        }
        let recovery = recovery_by_proposal
            .get(&helper_share.proposal_id)
            .ok_or_else(|| VotingError::InvalidInput {
                message: "pending helper share has no matching vote action".to_string(),
            })?;
        let expected_share_count = if recovery.single_share {
            1
        } else {
            VOTE_COMMITMENT_SHARE_COUNT
        };
        if helper_share.share_index >= expected_share_count as u32 {
            return Err(VotingError::InvalidInput {
                message: "pending helper share index is outside its committed share set"
                    .to_string(),
            });
        }
        validate_original_plan(&helper_share.original_plan, &record.original_helper_fleet)?;
        if let Some(delivery) = &helper_share.delivery {
            validate_delivery(
                delivery,
                helper_share,
                recovery,
                &record.helper_fleet_history,
            )?;
        }
        shares_by_action
            .entry(helper_share.proposal_id)
            .or_default()
            .push(helper_share);
    }

    for recovery in recoveries {
        let expected_share_count = if recovery.single_share {
            1
        } else {
            VOTE_COMMITMENT_SHARE_COUNT
        };
        let shares = shares_by_action
            .get(&recovery.proposal_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if shares.len() != expected_share_count {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "pending vote backup is missing helper plans for proposal {}",
                    recovery.proposal_id
                ),
            });
        }
        if !recovery.single_share && record.original_helper_fleet.len() > 1 {
            let mut assignments = BTreeMap::<&str, usize>::new();
            for share in shares {
                for server in &share.original_plan.target_servers {
                    *assignments.entry(server).or_default() += 1;
                }
            }
            if assignments
                .values()
                .any(|count| *count > SHARE_HELPER_MAX_INITIAL_SHARES_PER_SERVER as usize)
            {
                return Err(VotingError::InvalidInput {
                    message: "pending vote backup original plans exceed the complete-commitment helper quota"
                        .to_string(),
                });
            }
        }
    }
    Ok(())
}

fn validate_original_plan(
    plan: &ShareSubmissionPlan,
    original_helper_fleet: &[String],
) -> Result<(), VotingError> {
    let expected_target = share_submission_target_count(original_helper_fleet.len());
    let target = usize::try_from(plan.target_count).unwrap_or(usize::MAX);
    let canonical_targets = canonical_helper_url_list(&plan.target_servers)?;
    if target != expected_target
        || canonical_targets.len() != plan.target_servers.len()
        || canonical_targets != plan.target_servers
        || canonical_targets.len() != target
        || canonical_targets
            .iter()
            .any(|url| !original_helper_fleet.contains(url))
    {
        return Err(VotingError::InvalidInput {
            message: "pending vote backup contains an invalid original helper plan".to_string(),
        });
    }
    Ok(())
}

fn validate_delivery(
    delivery: &PendingHelperDeliveryBackupV1,
    helper_share: &PendingHelperShareBackupV1,
    recovery: &VoteRecoveryBundle,
    helper_fleet_history: &[String],
) -> Result<(), VotingError> {
    if delivery.nullifier.len() != 32 {
        return Err(VotingError::InvalidInput {
            message: "pending helper delivery nullifier must be 32 bytes".to_string(),
        });
    }
    let canonical_recovery = serialize_recovery(recovery)?;
    let expected_nullifier = crate::share::nullifier_from_recovery_json(
        &canonical_recovery,
        helper_share.proposal_id,
        helper_share.share_index,
    )?;
    if delivery.nullifier != expected_nullifier {
        return Err(VotingError::InvalidInput {
            message: "pending helper delivery nullifier does not match vote recovery material"
                .to_string(),
        });
    }
    if delivery.target_count < helper_share.original_plan.target_count {
        return Err(VotingError::InvalidInput {
            message: "pending helper delivery target cannot be below its original plan".to_string(),
        });
    }
    if usize::try_from(delivery.target_count).unwrap_or(usize::MAX) > SHARE_HELPER_TARGET_COUNT_CAP
    {
        return Err(VotingError::InvalidInput {
            message: "pending helper delivery target exceeds the protocol cap".to_string(),
        });
    }
    if delivery.submit_at != 0 && delivery.submit_at != helper_share.original_plan.submit_at {
        return Err(VotingError::InvalidInput {
            message: "pending helper delivery schedule differs from its original plan".to_string(),
        });
    }
    validate_delivery_urls(delivery)?;
    if delivery
        .accepted_urls
        .iter()
        .chain(&delivery.ambiguous_urls)
        .chain(&delivery.attempting_urls)
        .any(|url| !helper_fleet_history.contains(url))
    {
        return Err(VotingError::InvalidInput {
            message: "pending helper evidence contains a URL outside the recorded fleet history"
                .to_string(),
        });
    }
    Ok(())
}

fn validate_delivery_urls(delivery: &PendingHelperDeliveryBackupV1) -> Result<(), VotingError> {
    let accepted = canonicalize_backup_url_list(&delivery.accepted_urls)?;
    let ambiguous = canonicalize_backup_url_list(&delivery.ambiguous_urls)?;
    let attempting = canonicalize_backup_url_list(&delivery.attempting_urls)?;
    ShareDeliveryState::from_url_lists(&accepted, &ambiguous, &attempting).map(|_| ())
}

fn canonicalize_backup_url_list(urls: &[String]) -> Result<Vec<String>, VotingError> {
    let mut canonical = Vec::with_capacity(urls.len());
    for url in urls {
        let normalized = canonicalize_helper_base_url(url)?;
        if normalized != *url || canonical.contains(&normalized) {
            return Err(VotingError::InvalidInput {
                message: "pending helper evidence must contain distinct canonical URLs".to_string(),
            });
        }
        canonical.push(normalized);
    }
    Ok(canonical)
}

fn validate_retirement(
    record: &PendingVoteBackupRecordV1,
    retirement: &PendingVoteBackupRetirementV1,
    entries: &[PendingVoteBackupEntryV1],
) -> Result<(), VotingError> {
    match retirement {
        PendingVoteBackupRetirementV1::EveryExpectedShareConfirmed => {
            if record.helper_shares.iter().any(|share| {
                share
                    .delivery
                    .as_ref()
                    .is_none_or(|delivery| !delivery.confirmed)
            }) {
                return Err(VotingError::InvalidInput {
                    message:
                        "pending vote backup cannot retire before every expected share is confirmed"
                            .to_string(),
                });
            }
        }
        PendingVoteBackupRetirementV1::ReplacedBeforeSubmission {
            replacement_record_id,
        } => {
            let pristine = record.actions.iter().all(|action| {
                action.tx_hash.is_none() && action.confirmed_vc_tree_position.is_none()
            }) && record.helper_shares.iter().all(|share| {
                share.delivery.as_ref().is_none_or(|delivery| {
                    !delivery.confirmed
                        && delivery.accepted_urls.is_empty()
                        && delivery.ambiguous_urls.is_empty()
                        && delivery.attempting_urls.is_empty()
                })
            });
            if !pristine {
                return Err(VotingError::InvalidInput {
                    message: "only a pristine pre-submit pending vote may be replaced".to_string(),
                });
            }
            let replacement = entries.iter().find_map(|entry| match entry {
                PendingVoteBackupEntryV1::Live(replacement)
                    if replacement.binding.record_id == *replacement_record_id =>
                {
                    Some(replacement)
                }
                _ => None,
            });
            let Some(replacement) = replacement else {
                return Err(VotingError::InvalidInput {
                    message: "pending vote replacement record is not live in the same ledger"
                        .to_string(),
                });
            };
            if replacement.binding.authority_context_digest
                != record.binding.authority_context_digest
                || replacement.binding.authority_source_digest
                    != record.binding.authority_source_digest
                || replacement.binding.bundle_source_digest != record.binding.bundle_source_digest
                || replacement.binding.round_id != record.binding.round_id
                || replacement.binding.bundle_index != record.binding.bundle_index
            {
                return Err(VotingError::InvalidInput {
                    message: "pending vote replacement does not match the retired authority and bundle source"
                        .to_string(),
                });
            }
        }
    }
    Ok(())
}

fn replacement_evidence(
    record: &PendingVoteBackupRecordV1,
) -> Result<PendingVoteReplacementEvidenceV1, VotingError> {
    Ok(PendingVoteReplacementEvidenceV1 {
        vote_kind: record.vote_kind.clone(),
        actions: record
            .actions
            .iter()
            .map(action_generation_evidence)
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn action_generation_evidence(
    action: &PendingVoteActionBackupV1,
) -> Result<PendingVoteActionGenerationEvidenceV1, VotingError> {
    Ok(PendingVoteActionGenerationEvidenceV1 {
        proposal_id: action.proposal_id,
        recovery_generation_digest: recovery_generation_digest(&action.recovery_json)?,
        created_at: action.created_at,
    })
}

fn validate_tombstone(tombstone: &PendingVoteBackupTombstoneV1) -> Result<(), VotingError> {
    validate_binding(&tombstone.binding)?;
    if tombstone.retired_record_digest.as_bytes() == &[0; 32] {
        return Err(VotingError::InvalidInput {
            message: "pending vote backup tombstone digest must not be zero".to_string(),
        });
    }
    match (&tombstone.retirement, &tombstone.replacement_evidence) {
        (PendingVoteBackupRetirementV1::EveryExpectedShareConfirmed, None) => Ok(()),
        (PendingVoteBackupRetirementV1::ReplacedBeforeSubmission { .. }, Some(evidence)) => {
            validate_replacement_evidence(&tombstone.binding, evidence)
        }
        (PendingVoteBackupRetirementV1::EveryExpectedShareConfirmed, Some(_)) => {
            Err(VotingError::InvalidInput {
                message: "completed pending vote tombstone must not contain replacement evidence"
                    .to_string(),
            })
        }
        (PendingVoteBackupRetirementV1::ReplacedBeforeSubmission { .. }, None) => {
            Err(VotingError::InvalidInput {
                message: "replacement tombstone is missing retired vote generation evidence"
                    .to_string(),
            })
        }
    }
}

fn validate_replacement_evidence(
    binding: &PendingVoteBackupBindingV1,
    evidence: &PendingVoteReplacementEvidenceV1,
) -> Result<(), VotingError> {
    if evidence.actions.is_empty() {
        return Err(VotingError::InvalidInput {
            message: "replacement tombstone must identify at least one retired action".to_string(),
        });
    }
    let mut proposals = BTreeSet::new();
    for action in &evidence.actions {
        if !proposals.insert(action.proposal_id)
            || action.recovery_generation_digest.as_bytes() == &[0; 32]
        {
            return Err(VotingError::InvalidInput {
                message: "replacement tombstone contains invalid retired action evidence"
                    .to_string(),
            });
        }
    }
    let ordered_proposal_ids = evidence
        .actions
        .iter()
        .map(|action| action.proposal_id)
        .collect::<Vec<_>>();
    match &evidence.vote_kind {
        PendingVoteBackupKindV1::Singleton { proposal_id }
            if ordered_proposal_ids.as_slice() == [*proposal_id] => {}
        PendingVoteBackupKindV1::AtomicBatch {
            batch_digest,
            ordered_proposal_ids: expected,
        } if batch_digest.as_bytes() != &[0; 32] && expected == &ordered_proposal_ids => {}
        _ => {
            return Err(VotingError::InvalidInput {
                message:
                    "replacement tombstone action evidence does not match its retired vote kind"
                        .to_string(),
            });
        }
    }
    let derived = derive_record_id_from_generation_digests(
        binding,
        &evidence.vote_kind,
        evidence
            .actions
            .iter()
            .map(|action| action.recovery_generation_digest),
    )?;
    if derived != binding.record_id {
        return Err(VotingError::InvalidInput {
            message: "replacement tombstone evidence does not identify its retired record"
                .to_string(),
        });
    }
    Ok(())
}

fn validate_replacement_reference(
    tombstone: &PendingVoteBackupTombstoneV1,
    entries: &[PendingVoteBackupEntryV1],
) -> Result<(), VotingError> {
    let PendingVoteBackupRetirementV1::ReplacedBeforeSubmission {
        replacement_record_id,
    } = &tombstone.retirement
    else {
        return Ok(());
    };
    let mut next_id = *replacement_record_id;
    let mut visited = BTreeSet::from([tombstone.binding.record_id]);
    loop {
        if !visited.insert(next_id) {
            return Err(VotingError::InvalidInput {
                message: "pending vote replacement tombstones contain a cycle".to_string(),
            });
        }
        let entry = entries
            .iter()
            .find(|entry| entry.binding().record_id == next_id)
            .ok_or_else(|| VotingError::InvalidInput {
                message: "pending vote replacement target is not retained in the ledger"
                    .to_string(),
            })?;
        ensure_same_replacement_source(&tombstone.binding, entry.binding())?;
        match entry {
            PendingVoteBackupEntryV1::Live(_) => return Ok(()),
            PendingVoteBackupEntryV1::Retired(PendingVoteBackupTombstoneV1 {
                retirement:
                    PendingVoteBackupRetirementV1::ReplacedBeforeSubmission {
                        replacement_record_id,
                    },
                ..
            }) => next_id = *replacement_record_id,
            PendingVoteBackupEntryV1::Retired(_) => return Ok(()),
        }
    }
}

fn ensure_same_replacement_source(
    retired: &PendingVoteBackupBindingV1,
    replacement: &PendingVoteBackupBindingV1,
) -> Result<(), VotingError> {
    if replacement.authority_context_digest != retired.authority_context_digest
        || replacement.authority_source_digest != retired.authority_source_digest
        || replacement.bundle_source_digest != retired.bundle_source_digest
        || replacement.round_id != retired.round_id
        || replacement.bundle_index != retired.bundle_index
    {
        return Err(VotingError::InvalidInput {
            message:
                "pending vote replacement does not match the retired authority and bundle source"
                    .to_string(),
        });
    }
    Ok(())
}

fn digest_serializable<T: Serialize>(value: &T) -> Result<PendingVoteBackupDigest, VotingError> {
    let bytes = serde_json::to_vec(value).map_err(|error| VotingError::Internal {
        message: format!("failed to encode pending vote backup digest input: {error}"),
    })?;
    let hash = blake2b_simd::Params::new()
        .hash_length(32)
        .personal(PENDING_VOTE_BACKUP_PERSONALIZATION)
        .hash(&bytes);
    let digest: [u8; 32] = hash
        .as_bytes()
        .try_into()
        .expect("configured BLAKE2b digest is 32 bytes");
    Ok(PendingVoteBackupDigest::from_bytes(digest))
}

#[derive(Serialize)]
struct PendingVoteRecordIdInput<'a> {
    authority_context_digest: PendingVoteBackupDigest,
    authority_source_digest: PendingVoteBackupDigest,
    bundle_source_digest: PendingVoteBackupDigest,
    round_id: &'a str,
    bundle_index: u32,
    vote_kind: &'a PendingVoteBackupKindV1,
    recovery_generation_digests: Vec<PendingVoteBackupDigest>,
}

#[derive(Serialize)]
struct PendingVoteRecoveryGenerationDigestInput<'a> {
    format: &'static str,
    canonical_recovery: &'a str,
}

fn recovery_generation_digest(recovery_json: &str) -> Result<PendingVoteBackupDigest, VotingError> {
    let mut recovery = parse_recovery(recovery_json)?;
    recovery.vc_tree_position = 0;
    let canonical_recovery = serialize_recovery(&recovery)?;
    digest_serializable(&PendingVoteRecoveryGenerationDigestInput {
        format: "zcash_voting_pending_vote_recovery_generation_v1",
        canonical_recovery: &canonical_recovery,
    })
}

fn derive_record_id(
    binding: &PendingVoteBackupBindingV1,
    vote_kind: &PendingVoteBackupKindV1,
    actions: &[PendingVoteActionBackupV1],
) -> Result<PendingVoteBackupDigest, VotingError> {
    let recovery_generation_digests = actions
        .iter()
        .map(|action| recovery_generation_digest(&action.recovery_json))
        .collect::<Result<Vec<_>, VotingError>>()?;
    derive_record_id_from_generation_digests(binding, vote_kind, recovery_generation_digests)
}

fn derive_record_id_from_generation_digests(
    binding: &PendingVoteBackupBindingV1,
    vote_kind: &PendingVoteBackupKindV1,
    recovery_generation_digests: impl IntoIterator<Item = PendingVoteBackupDigest>,
) -> Result<PendingVoteBackupDigest, VotingError> {
    digest_serializable(&PendingVoteRecordIdInput {
        authority_context_digest: binding.authority_context_digest,
        authority_source_digest: binding.authority_source_digest,
        bundle_source_digest: binding.bundle_source_digest,
        round_id: &binding.round_id,
        bundle_index: binding.bundle_index,
        vote_kind,
        recovery_generation_digests: recovery_generation_digests.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::pasta_curves::{group::ff::PrimeField, pallas};
    use crate::{
        delegation_capability::{
            validate_recoverable_delegation_capability_v1, DelegationCapabilityBundleV1,
            DelegationCapabilityV1, ValidateRecoverableDelegationCapabilityV1Params,
        },
        storage::VotingDb,
        types::{EncryptedShare, Network, VotingRoundParams},
        vote::VoteBatchRecovery,
    };
    use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};

    const ROUND_ID: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    const WALLET_ID: &str = "pending-backup-wallet";
    const HELPER: &str = "https://helper.example";

    fn recovery(proposal_id: u32, position: u64) -> VoteRecoveryBundle {
        VoteRecoveryBundle {
            vote_round_id: ROUND_ID.to_string(),
            bundle_index: 0,
            proposal_id,
            vote_decision: 1,
            anchor_height: 123,
            vc_tree_position: position,
            single_share: true,
            num_options: 3,
            van_nullifier: [0x10; 32],
            vote_authority_note_new: [0x11; 32],
            vote_commitment: [proposal_id as u8; 32],
            proof: vec![0x13; 96],
            shares_hash: [0x14; 32],
            r_vpk: [0x15; 32],
            alpha_v: [0x16; 32],
            vote_auth_sig: [0x17; 64],
            encrypted_shares: vec![EncryptedShare {
                c1: vec![0x21; 32],
                c2: vec![0x22; 32],
                share_index: 0,
                plaintext_value: 1,
                randomness: vec![0x23; 32],
            }],
            share_blinds: vec![[0x31; 32]],
            share_comms: vec![[0x41; 32]],
            batch: None,
        }
    }

    fn record_from_recovery(
        recovery: VoteRecoveryBundle,
        confirmed_position: Option<u64>,
    ) -> PendingVoteBackupRecordV1 {
        let proposal_id = recovery.proposal_id;
        let plan = ShareSubmissionPlan {
            immediate: false,
            submit_at: 100,
            target_count: 1,
            target_servers: vec![HELPER.to_string()],
        };
        PendingVoteBackupRecordV1::new(
            &expected_binding(b"capability-one"),
            PendingVoteBackupKindV1::Singleton { proposal_id },
            vec![PendingVoteActionBackupV1 {
                proposal_id,
                recovery_json: serialize_recovery(&recovery).unwrap(),
                tx_hash: confirmed_position.map(|_| "tx-hash".to_string()),
                confirmed_vc_tree_position: confirmed_position,
                created_at: 50,
            }],
            vec![HELPER.to_string()],
            vec![PendingHelperShareBackupV1 {
                proposal_id,
                share_index: 0,
                original_plan: plan,
                delivery: None,
            }],
        )
        .unwrap()
    }

    fn record(proposal_id: u32, confirmed_position: Option<u64>) -> PendingVoteBackupRecordV1 {
        record_from_recovery(
            recovery(proposal_id, confirmed_position.unwrap_or(0)),
            confirmed_position,
        )
    }

    fn replacement_ledgers() -> (
        PendingVoteBackupLedgerV1,
        PendingVoteBackupLedgerV1,
        PendingVoteBackupDigest,
        PendingVoteBackupDigest,
    ) {
        let original = record(1, None);
        let original_id = original.record_id();
        let mut replacement_recovery = recovery(1, 0);
        replacement_recovery.vote_decision = 2;
        replacement_recovery.vote_authority_note_new = [0x91; 32];
        replacement_recovery.vote_commitment = [0x92; 32];
        replacement_recovery.proof[0] = 0x93;
        let replacement = record_from_recovery(replacement_recovery, None);
        let replacement_id = replacement.record_id();
        let original_ledger = PendingVoteBackupLedgerV1::new(original).unwrap();
        let replacement_ledger = original_ledger
            .successor_with_record(replacement)
            .unwrap()
            .successor_with_retirement(
                original_id,
                PendingVoteBackupRetirementV1::ReplacedBeforeSubmission {
                    replacement_record_id: replacement_id,
                },
            )
            .unwrap();
        (
            original_ledger,
            replacement_ledger,
            original_id,
            replacement_id,
        )
    }

    fn replacement_record_with_marker(
        vote_decision: u32,
        marker: u16,
    ) -> PendingVoteBackupRecordV1 {
        let mut replacement = recovery(1, 0);
        replacement.vote_decision = vote_decision;
        replacement.vote_authority_note_new = [0x91; 32];
        replacement.vote_commitment = [0x92; 32];
        let marker = marker.to_le_bytes();
        replacement.vote_authority_note_new[..marker.len()].copy_from_slice(&marker);
        replacement.vote_commitment[..marker.len()].copy_from_slice(&marker);
        replacement.proof[..marker.len()].copy_from_slice(&marker);
        record_from_recovery(replacement, None)
    }

    struct ReplacementChainFixture {
        initial_ledger: PendingVoteBackupLedgerV1,
        final_ledger: PendingVoteBackupLedgerV1,
        original_record_id: PendingVoteBackupDigest,
        intermediate_record: PendingVoteBackupRecordV1,
        final_record: PendingVoteBackupRecordV1,
    }

    fn unfavorable_replacement_chain() -> ReplacementChainFixture {
        let original = record(1, None);
        let original_record_id = original.record_id();
        let intermediate_record = (0..=u16::MAX)
            .map(|marker| replacement_record_with_marker(2, marker))
            .find(|candidate| candidate.record_id() < original_record_id)
            .expect("a replacement fixture must sort before the original record");
        let intermediate_record_id = intermediate_record.record_id();
        let final_record = replacement_record_with_marker(0, 0xcafe);
        assert_ne!(final_record.record_id(), original_record_id);
        assert_ne!(final_record.record_id(), intermediate_record_id);

        let initial_ledger = PendingVoteBackupLedgerV1::new(original).unwrap();
        let final_ledger = initial_ledger
            .successor_with_record(intermediate_record.clone())
            .unwrap()
            .successor_with_retirement(
                original_record_id,
                PendingVoteBackupRetirementV1::ReplacedBeforeSubmission {
                    replacement_record_id: intermediate_record_id,
                },
            )
            .unwrap()
            .successor_with_record(final_record.clone())
            .unwrap()
            .successor_with_retirement(
                intermediate_record_id,
                PendingVoteBackupRetirementV1::ReplacedBeforeSubmission {
                    replacement_record_id: final_record.record_id(),
                },
            )
            .unwrap();
        ReplacementChainFixture {
            initial_ledger,
            final_ledger,
            original_record_id,
            intermediate_record,
            final_record,
        }
    }

    fn atomic_record() -> PendingVoteBackupRecordV1 {
        let mut first = recovery(1, 0);
        let mut second = recovery(2, 0);
        let sighash_actions = [&first, &second]
            .into_iter()
            .map(
                |recovery| crate::vote_commitment::CastVoteBatchSighashAction {
                    r_vpk: &recovery.r_vpk,
                    van_nullifier: &recovery.van_nullifier,
                    vote_authority_note_new: &recovery.vote_authority_note_new,
                    vote_commitment: &recovery.vote_commitment,
                    proposal_id: recovery.proposal_id,
                },
            )
            .collect::<Vec<_>>();
        let batch_digest =
            crate::vote_commitment::cast_vote_batch_sighash(ROUND_ID, 123, &sighash_actions)
                .unwrap();
        first.batch = Some(VoteBatchRecovery {
            digest: batch_digest,
            index: 0,
            size: 2,
        });
        second.batch = Some(VoteBatchRecovery {
            digest: batch_digest,
            index: 1,
            size: 2,
        });
        let plan = ShareSubmissionPlan {
            immediate: false,
            submit_at: 100,
            target_count: 1,
            target_servers: vec![HELPER.to_string()],
        };
        PendingVoteBackupRecordV1::new(
            &expected_binding(b"capability-one"),
            PendingVoteBackupKindV1::AtomicBatch {
                batch_digest: PendingVoteBackupDigest::from_bytes(batch_digest),
                ordered_proposal_ids: vec![1, 2],
            },
            [&first, &second]
                .into_iter()
                .map(|recovery| PendingVoteActionBackupV1 {
                    proposal_id: recovery.proposal_id,
                    recovery_json: serialize_recovery(recovery).unwrap(),
                    tx_hash: None,
                    confirmed_vc_tree_position: None,
                    created_at: 50,
                })
                .collect(),
            vec![HELPER.to_string()],
            [1, 2]
                .into_iter()
                .map(|proposal_id| PendingHelperShareBackupV1 {
                    proposal_id,
                    share_index: 0,
                    original_plan: plan.clone(),
                    delivery: None,
                })
                .collect(),
        )
        .unwrap()
    }

    fn authority_fixture(
        bundle_source: super::super::BundleMaterialSourceV1,
    ) -> (
        super::super::VotingAuthorityRootV1,
        super::super::VotingAuthoritySelectionV1,
    ) {
        let context = super::super::VotingAuthorityContextV1::from_fingerprint(
            Network::Testnet,
            0,
            [0x55; 32],
            "vote-chain-test",
            [0x01; 32],
        )
        .unwrap();
        let request = super::super::SoftwareRegisteredKeyRequestV1::new(
            super::super::RegisteredKeyApplicationV1::new(7),
            context,
        );
        let root =
            super::super::VotingAuthorityRootV1::from_registered_key_output(&request, [0x66; 64]);
        let authenticated_round = super::super::test_verified_round_auth_v3(root.context());
        let selection = super::super::VotingAuthoritySelectionV1::bind(
            &root,
            bundle_source,
            &authenticated_round,
        )
        .unwrap();
        (root, selection)
    }

    fn expected_binding(capability_bytes: &[u8]) -> PendingVoteBackupExpectedBindingV1 {
        let (_, selection) =
            authority_fixture(super::super::BundleMaterialSourceV1::CustodyCapability);
        let capability = PendingVoteCapabilityBindingV1 {
            bundle_index: 0,
            digest: digest_serializable(&("test custody capability", capability_bytes)).unwrap(),
        };
        PendingVoteBackupExpectedBindingV1::derive(
            &selection,
            PendingVoteBundleBindingV1::CustodyCapability(&capability),
        )
        .unwrap()
    }

    fn validated_capability_fixture() -> (
        crate::delegation_capability::ValidatedDelegationCapabilityMaterialV1,
        super::super::VotingAuthoritySelectionV1,
    ) {
        let (root, selection) =
            authority_fixture(super::super::BundleMaterialSourceV1::CustodyCapability);
        let hotkey = root.voting_hotkey().unwrap();
        let round_params = VotingRoundParams {
            vote_round_id: ROUND_ID.to_string(),
            snapshot_height: 1_234_567,
            ea_pk: vec![0xEA; 32],
            nc_root: vec![0xAA; 32],
            nullifier_imt_root: vec![0xBB; 32],
        };
        let verified_round =
            super::super::test_verified_voting_round_v3(root.context(), round_params);
        let capability_json = DelegationCapabilityV1 {
            format_version: 1,
            vote_chain_id: "vote-chain-test".to_string(),
            network: "testnet".to_string(),
            vote_round_id: ROUND_ID.to_string(),
            address_index: 0,
            raw_orchard_address: BASE64_STANDARD.encode(hotkey.raw_orchard_address()),
            bundles: vec![DelegationCapabilityBundleV1 {
                bundle_index: 0,
                num_ballots: 1,
                van_comm_rand: BASE64_STANDARD.encode(pallas::Base::from(11).to_repr()),
                delegation_tx_hash: hex::encode([0x44; 32]),
            }],
        }
        .to_json()
        .unwrap();
        let material = validate_recoverable_delegation_capability_v1(
            &capability_json,
            ValidateRecoverableDelegationCapabilityV1Params {
                authority_root: &root,
                authority_selection: &selection,
                voting_hotkey: &hotkey,
                verified_round: &verified_round,
            },
        )
        .unwrap();
        (material, selection)
    }

    fn empty_round_db() -> VotingDb {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(WALLET_ID);
        let conn = db.conn();
        conn.execute(
            "INSERT INTO rounds
             (round_id, wallet_id, network, snapshot_height, ea_pk, nc_root,
              nullifier_imt_root, phase, created_at)
             VALUES (?1, ?2, 'testnet', 1, X'01', X'02', X'03', 0, 1)",
            [ROUND_ID, WALLET_ID],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO bundles (round_id, wallet_id, bundle_index)
             VALUES (?1, ?2, 0)",
            [ROUND_ID, WALLET_ID],
        )
        .unwrap();
        drop(conn);
        db
    }

    #[test]
    fn json_roundtrip_preserves_confirmed_position_zero() {
        let ledger = PendingVoteBackupLedgerV1::new(record(1, Some(0))).unwrap();
        let restored = PendingVoteBackupLedgerV1::from_json(&ledger.to_json().unwrap()).unwrap();
        let PendingVoteBackupEntryV1::Live(restored_record) = &restored.entries()[0] else {
            panic!("record must remain live");
        };
        assert_eq!(
            restored_record.actions[0].confirmed_vc_tree_position,
            Some(0)
        );
    }

    #[test]
    fn ledger_rejects_caller_selected_record_identity() {
        let mut record = record(1, None);
        record.binding.record_id = PendingVoteBackupDigest::from_bytes([9; 32]);
        let error = PendingVoteBackupLedgerV1::new(record)
            .err()
            .expect("caller-selected record identity must fail");
        assert!(error.to_string().contains("record_id"), "{error}");
    }

    #[test]
    fn same_proposal_replacement_gets_a_new_generation_id_and_tombstones_the_old_vote() {
        let original = record(1, None);
        let original_id = original.record_id();
        let mut replacement_recovery = recovery(1, 0);
        replacement_recovery.vote_decision = 2;
        replacement_recovery.vote_authority_note_new = [0x91; 32];
        replacement_recovery.vote_commitment = [0x92; 32];
        replacement_recovery.proof[0] = 0x93;
        let replacement = record_from_recovery(replacement_recovery, None);
        let replacement_id = replacement.record_id();
        assert_ne!(original_id, replacement_id);

        let ledger = PendingVoteBackupLedgerV1::new(original.clone()).unwrap();
        let ledger = ledger.successor_with_record(replacement).unwrap();
        let ledger = ledger
            .successor_with_retirement(
                original_id,
                PendingVoteBackupRetirementV1::ReplacedBeforeSubmission {
                    replacement_record_id: replacement_id,
                },
            )
            .unwrap();

        assert!(ledger.live_record(replacement_id).is_ok());
        assert!(ledger.live_record(original_id).is_err());
        assert!(ledger.successor_with_record(original).is_err());
        assert!(ledger.entries().iter().any(|entry| matches!(
            entry,
            PendingVoteBackupEntryV1::Retired(PendingVoteBackupTombstoneV1 {
                retirement:
                    PendingVoteBackupRetirementV1::ReplacedBeforeSubmission {
                        replacement_record_id
                    },
                ..
            }) if *replacement_record_id == replacement_id
        )));
    }

    #[test]
    fn import_atomically_replaces_the_exact_pristine_local_generation() {
        let db = empty_round_db();
        let (original, replacement, original_id, replacement_id) = replacement_ledgers();
        let expected = [expected_binding(b"capability-one")];
        import_pending_vote_backup_ledger_v1(
            &db,
            &original,
            &expected,
            original.revision(),
            original.digest(),
        )
        .unwrap();
        db.conn()
            .execute(
                "INSERT INTO ballot_intent
                 (round_id, wallet_id, proposal_id, skipped, choice, created_at, updated_at)
                 VALUES (?1, ?2, 1, 0, 1, 40, 40)",
                [ROUND_ID, WALLET_ID],
            )
            .unwrap();

        let replacement =
            PendingVoteBackupLedgerV1::from_json(&replacement.to_json().unwrap()).unwrap();
        import_pending_vote_backup_ledger_v1(
            &db,
            &replacement,
            &expected,
            replacement.revision(),
            replacement.digest(),
        )
        .unwrap();

        let (choice, recovery_json): (u32, String) = db
            .conn()
            .query_row(
                "SELECT choice, commitment_bundle_json FROM votes
                 WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = 0
                   AND proposal_id = 1",
                [ROUND_ID, WALLET_ID],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(choice, 2);
        assert_eq!(parse_recovery(&recovery_json).unwrap().vote_decision, 2);
        let intent_count: u64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM ballot_intent
                 WHERE round_id = ?1 AND wallet_id = ?2 AND proposal_id = 1",
                [ROUND_ID, WALLET_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(intent_count, 0);
        let original_protection: (bool, u64, Vec<u8>) = db
            .conn()
            .query_row(
                "SELECT retired, ledger_revision, ledger_digest
                 FROM pending_vote_backup_protection
                 WHERE wallet_id = ?1 AND record_id = ?2 AND proposal_id = 1",
                rusqlite::params![WALLET_ID, original_id.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!(original_protection.0);
        assert_eq!(original_protection.1, replacement.revision());
        assert_eq!(original_protection.2, replacement.digest().as_bytes());
        let replacement_protection: (bool, u64, Vec<u8>) = db
            .conn()
            .query_row(
                "SELECT retired, ledger_revision, ledger_digest
                 FROM pending_vote_backup_protection
                 WHERE wallet_id = ?1 AND record_id = ?2 AND proposal_id = 1",
                rusqlite::params![WALLET_ID, replacement_id.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!(!replacement_protection.0);
        assert_eq!(replacement_protection.1, replacement.revision());
        assert_eq!(replacement_protection.2, replacement.digest().as_bytes());
        let stored_head: (u64, Vec<u8>) = db
            .conn()
            .query_row(
                "SELECT revision, digest FROM pending_vote_backup_heads WHERE wallet_id = ?1",
                [WALLET_ID],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored_head.0, replacement.revision());
        assert_eq!(stored_head.1, replacement.digest().as_bytes());

        import_pending_vote_backup_ledger_v1(
            &db,
            &replacement,
            &expected,
            replacement.revision(),
            replacement.digest(),
        )
        .expect("exact replacement-ledger replay must be idempotent");

        let mut refreshed = replacement.live_record(replacement_id).unwrap().clone();
        refreshed
            .helper_fleet_history
            .push("https://helper-two.example".to_string());
        refreshed.helper_fleet_history.sort();
        let successor = replacement.successor_with_record(refreshed).unwrap();
        import_pending_vote_backup_ledger_v1(
            &db,
            &successor,
            &expected,
            successor.revision(),
            successor.digest(),
        )
        .expect("a later replacement-ledger successor must converge");
        let successor_head: (u64, Vec<u8>) = db
            .conn()
            .query_row(
                "SELECT revision, digest FROM pending_vote_backup_heads WHERE wallet_id = ?1",
                [WALLET_ID],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(successor_head.0, successor.revision());
        assert_eq!(successor_head.1, successor.digest().as_bytes());
    }

    #[test]
    fn skipped_replacement_chain_uses_dependency_order_and_replays_idempotently() {
        let db = empty_round_db();
        let fixture = unfavorable_replacement_chain();
        let intermediate_record_id = fixture.intermediate_record.record_id();
        let final_record_id = fixture.final_record.record_id();
        assert!(
            intermediate_record_id < fixture.original_record_id,
            "the canonical record-id order must be unfavorable"
        );
        let application_order = replacement_tombstones_in_application_order(&fixture.final_ledger)
            .unwrap()
            .into_iter()
            .map(|tombstone| tombstone.binding.record_id)
            .collect::<Vec<_>>();
        assert_eq!(
            application_order,
            vec![fixture.original_record_id, intermediate_record_id]
        );

        let expected = [expected_binding(b"capability-one")];
        import_pending_vote_backup_ledger_v1(
            &db,
            &fixture.initial_ledger,
            &expected,
            fixture.initial_ledger.revision(),
            fixture.initial_ledger.digest(),
        )
        .unwrap();
        import_pending_vote_backup_ledger_v1(
            &db,
            &fixture.final_ledger,
            &expected,
            fixture.final_ledger.revision(),
            fixture.final_ledger.digest(),
        )
        .expect("a skipped replacement chain must apply oldest generation first");

        let (stored_choice, stored_recovery): (u32, String) = db
            .conn()
            .query_row(
                "SELECT choice, commitment_bundle_json FROM votes
                 WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = 0
                   AND proposal_id = 1",
                [ROUND_ID, WALLET_ID],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored_choice, 0);
        assert_eq!(
            stored_recovery,
            fixture.final_record.actions[0].recovery_json
        );
        let original_retired: bool = db
            .conn()
            .query_row(
                "SELECT retired FROM pending_vote_backup_protection
                 WHERE wallet_id = ?1 AND record_id = ?2 AND proposal_id = 1",
                rusqlite::params![WALLET_ID, fixture.original_record_id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(original_retired);
        let intermediate_protection_count: u64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM pending_vote_backup_protection
                 WHERE wallet_id = ?1 AND record_id = ?2",
                rusqlite::params![WALLET_ID, intermediate_record_id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(intermediate_protection_count, 0);
        let final_retired: bool = db
            .conn()
            .query_row(
                "SELECT retired FROM pending_vote_backup_protection
                 WHERE wallet_id = ?1 AND record_id = ?2 AND proposal_id = 1",
                rusqlite::params![WALLET_ID, final_record_id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!final_retired);

        import_pending_vote_backup_ledger_v1(
            &db,
            &fixture.final_ledger,
            &expected,
            fixture.final_ledger.revision(),
            fixture.final_ledger.digest(),
        )
        .expect("an exact skipped-chain replay must remain idempotent");
    }

    #[test]
    fn skipped_replacement_chain_rejects_an_exact_unprotected_intermediate_generation() {
        let db = empty_round_db();
        let fixture = unfavorable_replacement_chain();
        let intermediate_record_id = fixture.intermediate_record.record_id();
        let intermediate_ledger =
            PendingVoteBackupLedgerV1::new(fixture.intermediate_record.clone()).unwrap();
        let expected = [expected_binding(b"capability-one")];
        import_pending_vote_backup_ledger_v1(
            &db,
            &intermediate_ledger,
            &expected,
            intermediate_ledger.revision(),
            intermediate_ledger.digest(),
        )
        .unwrap();
        db.conn()
            .execute(
                "DELETE FROM pending_vote_backup_protection
                 WHERE wallet_id = ?1 AND record_id = ?2",
                rusqlite::params![WALLET_ID, intermediate_record_id.as_bytes().as_slice()],
            )
            .unwrap();

        let error = import_pending_vote_backup_ledger_v1(
            &db,
            &fixture.final_ledger,
            &expected,
            fixture.final_ledger.revision(),
            fixture.final_ledger.digest(),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("partial or unprotected"),
            "{error}"
        );
        let stored_recovery: String = db
            .conn()
            .query_row(
                "SELECT commitment_bundle_json FROM votes
                 WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = 0
                   AND proposal_id = 1",
                [ROUND_ID, WALLET_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stored_recovery,
            fixture.intermediate_record.actions[0].recovery_json
        );
    }

    #[test]
    fn skipped_replacement_chain_rejects_unprotected_state_outside_terminal_proposals() {
        let db = empty_round_db();
        let original = record(2, None);
        let original_record_id = original.record_id();
        let intermediate = replacement_record_with_marker(2, 0xb002);
        let intermediate_record_id = intermediate.record_id();
        let mut final_recovery = recovery(2, 0);
        final_recovery.vote_decision = 2;
        final_recovery.vote_authority_note_new = [0xc3; 32];
        final_recovery.vote_commitment = [0xc4; 32];
        final_recovery.proof[0] = 0xc5;
        let final_record = record_from_recovery(final_recovery, None);
        let final_record_id = final_record.record_id();
        let final_ledger = PendingVoteBackupLedgerV1::new(original)
            .unwrap()
            .successor_with_record(intermediate)
            .unwrap()
            .successor_with_retirement(
                original_record_id,
                PendingVoteBackupRetirementV1::ReplacedBeforeSubmission {
                    replacement_record_id: intermediate_record_id,
                },
            )
            .unwrap()
            .successor_with_record(final_record)
            .unwrap()
            .successor_with_retirement(
                intermediate_record_id,
                PendingVoteBackupRetirementV1::ReplacedBeforeSubmission {
                    replacement_record_id: final_record_id,
                },
            )
            .unwrap();

        let foreign_record = replacement_record_with_marker(0, 0xd004);
        let foreign_record_id = foreign_record.record_id();
        let foreign_ledger = PendingVoteBackupLedgerV1::new(foreign_record.clone()).unwrap();
        let expected = [expected_binding(b"capability-one")];
        import_pending_vote_backup_ledger_v1(
            &db,
            &foreign_ledger,
            &expected,
            foreign_ledger.revision(),
            foreign_ledger.digest(),
        )
        .unwrap();
        db.conn()
            .execute(
                "DELETE FROM pending_vote_backup_protection
                 WHERE wallet_id = ?1 AND record_id = ?2",
                rusqlite::params![WALLET_ID, foreign_record_id.as_bytes().as_slice()],
            )
            .unwrap();

        let error = import_pending_vote_backup_ledger_v1(
            &db,
            &final_ledger,
            &expected,
            final_ledger.revision(),
            final_ledger.digest(),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("partial or unprotected"),
            "{error}"
        );
        let stored_recovery: String = db
            .conn()
            .query_row(
                "SELECT commitment_bundle_json FROM votes
                 WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = 0
                   AND proposal_id = 1",
                [ROUND_ID, WALLET_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_recovery, foreign_record.actions[0].recovery_json);
        let terminal_vote_count: u64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM votes
                 WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = 0
                   AND proposal_id = 2",
                [ROUND_ID, WALLET_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(terminal_vote_count, 0);
        let stored_head: (u64, Vec<u8>) = db
            .conn()
            .query_row(
                "SELECT revision, digest FROM pending_vote_backup_heads WHERE wallet_id = ?1",
                [WALLET_ID],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored_head.0, foreign_ledger.revision());
        assert_eq!(stored_head.1, foreign_ledger.digest().as_bytes());
    }

    #[test]
    fn replacement_import_rejects_submitted_or_confirmed_local_vote_evidence() {
        for update in [
            "UPDATE votes SET tx_hash = 'submitted' WHERE proposal_id = 1",
            "UPDATE votes SET vc_tree_position = 7 WHERE proposal_id = 1",
        ] {
            let db = empty_round_db();
            let (original, replacement, original_id, replacement_id) = replacement_ledgers();
            let expected = [expected_binding(b"capability-one")];
            import_pending_vote_backup_ledger_v1(
                &db,
                &original,
                &expected,
                original.revision(),
                original.digest(),
            )
            .unwrap();
            db.conn().execute(update, []).unwrap();

            let error = import_pending_vote_backup_ledger_v1(
                &db,
                &replacement,
                &expected,
                replacement.revision(),
                replacement.digest(),
            )
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("cannot remove submitted or confirmed"),
                "{error}"
            );
            assert_failed_replacement_preserved_original(
                &db,
                &original,
                original_id,
                replacement_id,
            );
        }
    }

    #[test]
    fn replacement_import_rejects_every_durable_helper_outcome_class() {
        for (accepted, ambiguous, attempting, confirmed) in [
            (vec![HELPER], Vec::new(), Vec::new(), false),
            (Vec::new(), vec![HELPER], Vec::new(), false),
            (Vec::new(), Vec::new(), vec![HELPER], false),
            (Vec::new(), Vec::new(), Vec::new(), true),
        ] {
            let db = empty_round_db();
            let (original, replacement, original_id, replacement_id) = replacement_ledgers();
            let expected = [expected_binding(b"capability-one")];
            import_pending_vote_backup_ledger_v1(
                &db,
                &original,
                &expected,
                original.revision(),
                original.digest(),
            )
            .unwrap();
            let original_record = original.live_record(original_id).unwrap();
            let recovery_json = &original_record.actions[0].recovery_json;
            let nullifier = crate::share::nullifier_from_recovery_json(recovery_json, 1, 0)
                .unwrap()
                .to_vec();
            db.conn()
                .execute(
                    "INSERT INTO share_delegations
                     (round_id, wallet_id, bundle_index, proposal_id, share_index,
                      sent_to_urls, ambiguous_urls, attempting_urls, target_count,
                      nullifier, confirmed, submit_at, created_at)
                     VALUES (:round_id, :wallet_id, 0, 1, 0, :accepted, :ambiguous,
                             :attempting, 1, :nullifier, :confirmed, 100, 75)",
                    named_params! {
                        ":round_id": ROUND_ID,
                        ":wallet_id": WALLET_ID,
                        ":accepted": serde_json::to_string(&accepted).unwrap(),
                        ":ambiguous": serde_json::to_string(&ambiguous).unwrap(),
                        ":attempting": serde_json::to_string(&attempting).unwrap(),
                        ":nullifier": nullifier,
                        ":confirmed": confirmed,
                    },
                )
                .unwrap();

            let error = import_pending_vote_backup_ledger_v1(
                &db,
                &replacement,
                &expected,
                replacement.revision(),
                replacement.digest(),
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("helper outcome evidence"),
                "{error}"
            );
            assert_failed_replacement_preserved_original(
                &db,
                &original,
                original_id,
                replacement_id,
            );
        }
    }

    fn assert_failed_replacement_preserved_original(
        db: &VotingDb,
        original: &PendingVoteBackupLedgerV1,
        original_id: PendingVoteBackupDigest,
        replacement_id: PendingVoteBackupDigest,
    ) {
        let stored_head: (u64, Vec<u8>) = db
            .conn()
            .query_row(
                "SELECT revision, digest FROM pending_vote_backup_heads WHERE wallet_id = ?1",
                [WALLET_ID],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored_head.0, original.revision());
        assert_eq!(stored_head.1, original.digest().as_bytes());
        let original_retired: bool = db
            .conn()
            .query_row(
                "SELECT retired FROM pending_vote_backup_protection
                 WHERE wallet_id = ?1 AND record_id = ?2 AND proposal_id = 1",
                rusqlite::params![WALLET_ID, original_id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!original_retired);
        let replacement_protection_count: u64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM pending_vote_backup_protection
                 WHERE wallet_id = ?1 AND record_id = ?2",
                rusqlite::params![WALLET_ID, replacement_id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(replacement_protection_count, 0);
        let choice: u32 = db
            .conn()
            .query_row(
                "SELECT choice FROM votes
                 WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = 0
                   AND proposal_id = 1",
                [ROUND_ID, WALLET_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(choice, 1);
    }

    #[test]
    fn custody_binding_accepts_only_a_bundle_from_validated_capability_material() {
        let (material, selection) = validated_capability_fixture();
        let binding =
            PendingVoteCapabilityBindingV1::from_validated_capability(&material, 0).unwrap();
        assert_eq!(binding.bundle_index, 0);
        assert_eq!(binding.digest.as_bytes(), material.digest().as_bytes());
        assert!(PendingVoteCapabilityBindingV1::from_validated_capability(&material, 1).is_err());

        let expected = PendingVoteBackupExpectedBindingV1::derive(
            &selection,
            PendingVoteBundleBindingV1::CustodyCapability(&binding),
        )
        .unwrap();
        assert_eq!(expected.bundle_index, 0);
        assert_eq!(expected.bundle_source_digest, binding.digest);
    }

    #[test]
    fn successor_retains_multiple_records_and_rejects_evidence_downgrade() {
        let first = record(1, None);
        let first_id = first.record_id();
        let ledger = PendingVoteBackupLedgerV1::new(first).unwrap();
        let ledger = ledger.successor_with_record(record(2, None)).unwrap();
        assert_eq!(ledger.entries().len(), 2);

        let mut strengthened = ledger.live_record(first_id).unwrap().clone();
        strengthened.helper_shares[0].delivery = Some(PendingHelperDeliveryBackupV1 {
            accepted_urls: vec![HELPER.to_string()],
            ambiguous_urls: vec![],
            attempting_urls: vec![],
            target_count: 1,
            nullifier: crate::share::nullifier_from_recovery_json(
                &strengthened.actions[0].recovery_json,
                1,
                0,
            )
            .unwrap()
            .to_vec(),
            confirmed: false,
            submit_at: 100,
            created_at: 75,
        });
        let ledger = ledger.successor_with_record(strengthened).unwrap();
        let mut downgraded = ledger.live_record(first_id).unwrap().clone();
        downgraded.helper_shares[0]
            .delivery
            .as_mut()
            .unwrap()
            .accepted_urls
            .clear();
        assert!(ledger.successor_with_record(downgraded).is_err());
    }

    #[test]
    fn import_preserves_some_zero_and_protects_unconfirmed_recovery_from_cleanup() {
        let db = empty_round_db();
        let confirmed = PendingVoteBackupLedgerV1::new(record(1, Some(0))).unwrap();
        import_pending_vote_backup_ledger_v1(
            &db,
            &confirmed,
            &[expected_binding(b"capability-one")],
            confirmed.revision(),
            confirmed.digest(),
        )
        .unwrap();
        let position: Option<u64> = db
            .conn()
            .query_row(
                "SELECT vc_tree_position FROM votes WHERE proposal_id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(position, Some(0));

        let unconfirmed_record = record(2, None);
        let unconfirmed_id = unconfirmed_record.record_id();
        let successor = confirmed.successor_with_record(unconfirmed_record).unwrap();
        import_pending_vote_backup_ledger_v1(
            &db,
            &successor,
            &[expected_binding(b"capability-one")],
            successor.revision(),
            successor.digest(),
        )
        .unwrap();
        db.clear_recovery_state(ROUND_ID).unwrap();
        let recovery_json: Option<String> = db
            .conn()
            .query_row(
                "SELECT commitment_bundle_json FROM votes WHERE proposal_id = 2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(recovery_json.is_some());
        assert!(successor.live_record(unconfirmed_id).is_ok());
    }

    #[test]
    fn import_accepts_skipped_checkpoint_revisions_and_rejects_stale_head() {
        let db = empty_round_db();
        let first = PendingVoteBackupLedgerV1::new(record(1, None)).unwrap();
        import_pending_vote_backup_ledger_v1(
            &db,
            &first,
            &[expected_binding(b"capability-one")],
            first.revision(),
            first.digest(),
        )
        .unwrap();
        let second = first.successor_with_record(record(2, None)).unwrap();
        let third = second.successor_with_record(record(3, None)).unwrap();
        import_pending_vote_backup_ledger_v1(
            &db,
            &third,
            &[expected_binding(b"capability-one")],
            third.revision(),
            third.digest(),
        )
        .unwrap();
        assert!(import_pending_vote_backup_ledger_v1(
            &db,
            &first,
            &[expected_binding(b"capability-one")],
            first.revision(),
            first.digest()
        )
        .is_err());
    }

    #[test]
    fn atomic_record_protection_rolls_back_if_any_action_cannot_be_protected() {
        let db = empty_round_db();
        let atomic = atomic_record();
        let record_id = atomic.record_id();
        let ledger = PendingVoteBackupLedgerV1::new(atomic).unwrap();
        db.conn()
            .execute_batch(
                "CREATE TRIGGER fail_second_pending_vote_protection
                 BEFORE INSERT ON pending_vote_backup_protection
                 WHEN NEW.proposal_id = 2
                 BEGIN
                     SELECT RAISE(ABORT, 'injected protection failure');
                 END;",
            )
            .unwrap();

        let error = protect_live_pending_record(&db, &ledger, record_id).unwrap_err();
        assert!(
            error.to_string().contains("protect pending vote"),
            "{error}"
        );
        let count: u64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM pending_vote_backup_protection",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);

        db.conn()
            .execute_batch("DROP TRIGGER fail_second_pending_vote_protection;")
            .unwrap();
        protect_live_pending_record(&db, &ledger, record_id).unwrap();
        let count: u64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM pending_vote_backup_protection",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn import_rejects_a_different_typed_authority_or_bundle_binding() {
        let db = empty_round_db();
        let ledger = PendingVoteBackupLedgerV1::new(record(1, None)).unwrap();
        let error = import_pending_vote_backup_ledger_v1(
            &db,
            &ledger,
            &[expected_binding(b"different-capability")],
            ledger.revision(),
            ledger.digest(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("expected authority"), "{error}");
    }

    #[test]
    fn compatible_fleet_addition_is_checkpointed_without_changing_original_plan() {
        let mut ledger = PendingVoteBackupLedgerV1::new(record(1, None)).unwrap();
        let record_id = ledger.entries()[0].binding().record_id;
        let plan = ledger.live_record(record_id).unwrap().helper_shares[0]
            .original_plan
            .clone();
        let mut persisted = 0;
        let mut persist = |_: &PendingVoteBackupLedgerV1| {
            persisted += 1;
            Ok(())
        };
        {
            let mut checkpoint =
                PendingVoteBackupCheckpointV1::new(&mut ledger, record_id, &mut persist).unwrap();
            checkpoint
                .validate_share_request(
                    ROUND_ID,
                    0,
                    1,
                    0,
                    &plan,
                    &[HELPER.to_string(), "https://helper-two.example".to_string()],
                )
                .unwrap();
        }
        assert_eq!(persisted, 1);
        let record = ledger.live_record(record_id).unwrap();
        assert_eq!(record.original_helper_fleet, vec![HELPER.to_string()]);
        assert!(record
            .helper_fleet_history
            .contains(&"https://helper-two.example".to_string()));
    }
}
