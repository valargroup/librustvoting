//! Recoverable voting authority construction.
//!
//! Version 1 keeps wallet seed handling outside this crate. Software wallets
//! receive a registered-key request and return only its 64-byte cryptovalue;
//! Keystone hosts instead retain a versioned random master generation. Both
//! sources produce the same typed per-round root and therefore share hotkey and
//! self-custody bundle derivation.

mod ballot;
mod bundle;
mod chain_recovery;
mod derivation;
mod pir;
mod round;

pub use ballot::{
    commit_recoverable_complete_ballot_v1, recoverable_complete_ballot_readiness_v1,
    RecoverableBallotReadinessV1, RecoverableCompleteBallotV1,
};
pub(crate) use bundle::canonical_recoverable_notes;
pub use bundle::{
    plan_recoverable_self_custody_bundles_v1, recoverable_bundle_policy_v1,
    RecoverableBundleIdentityV1, RecoverableBundleMaterialV1, RecoverableBundleNoteIdentityV1,
    RecoverableBundleUseV1, RecoverableSelfCustodyBundlePlanV1, RecoverableSelfCustodyBundleV1,
    RecoverableVanBlindingV1,
};
pub use chain_recovery::{
    reconcile_recoverable_ballot_v1, ConfirmedTransitionPositionV1,
    ConfirmedVotingAuthorityConsumerKindV1, ConfirmedVotingAuthorityConsumerV1,
    ReconciledRecoverableBallotV1, RecoverableBallotChainStatusV1, RecoveryCheckpointV1,
    UnsupportedRecoverableBallotSpendV1, VotingAuthorityRecoveryEvidenceV1,
    VotingAuthorityRecoveryEvidenceVerifierV1, VotingAuthorityVanWitnessV1,
};
pub use derivation::{
    orchard_fvk_fingerprint_v1, KeystoneMasterGenerationV1, RecoverableVotingHotkeyV1,
    RegisteredKeyApplicationV1, SoftwareRegisteredKeyRequestV1, VotingAuthorityContextV1,
    VotingAuthorityRootBindingV1, VotingAuthorityRootSourceV1, VotingAuthorityRootV1,
    VotingAuthorityScopeV1, KEYSTONE_MASTER_SECRET_LEN, VOTING_AUTHORITY_ROOT_LEN,
};
pub use pir::{
    connect_recoverable_pir_blocking_v1, select_recoverable_pir_snapshot_v1,
    RecoverablePirClientV1, RecoverablePirSnapshotMetadataV1, VerifiedRecoverablePirSnapshotV1,
};
#[cfg(test)]
pub(crate) use round::test_validated_recoverable_voting_round_v1;
pub use round::{
    validate_recoverable_voting_round_v1, ChainVotingRoundV1, ValidatedRecoverableVotingRoundV1,
};
