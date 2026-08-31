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

pub use ballot::{
    commit_recoverable_complete_ballot_v1, prepare_recoverable_complete_ballot_v1,
    record_recoverable_complete_ballot_submission_v1, recoverable_complete_ballot_readiness_v1,
    RecoverableBallotReadinessV1, RecoverableCompleteBallotV1,
};
pub(crate) use bundle::canonical_recoverable_notes;
pub use bundle::{
    plan_recoverable_self_custody_bundles_v1, recoverable_bundle_policy_v1,
    RecoverableBundleIdentityV1, RecoverableBundleMaterialV1, RecoverableBundleNoteIdentityV1,
    RecoverableBundleUseV1, RecoverableSelfCustodyBundlePlanV1, RecoverableSelfCustodyBundleV1,
    RecoverableVanBlindingV1,
};
pub use chain_recovery::*;
pub use derivation::{
    orchard_fvk_fingerprint_v1, BundleMaterialSourceV1, KeystoneMasterGenerationV1,
    RecoverableVotingHotkeyV1, RegisteredKeyApplicationV1, SoftwareRegisteredKeyRequestV1,
    VotingAuthorityContextV1, VotingAuthorityRootSourceV1, VotingAuthorityRootV1,
    VotingAuthorityScopeV1, VotingAuthoritySelectionV1, KEYSTONE_MASTER_SECRET_LEN,
    VOTING_AUTHORITY_ROOT_LEN,
};
pub use pir::{
    connect_recoverable_pir_blocking_v1, select_recoverable_pir_snapshot_v1,
    RecoverablePirClientV1, RecoverablePirSnapshotMetadataV1, VerifiedRecoverablePirSnapshotV1,
};

#[cfg(test)]
pub(crate) fn test_round_auth_payload_v3(
    context: &VotingAuthorityContextV1,
) -> crate::round_auth::RoundAuthPayloadV3 {
    crate::round_auth::RoundAuthPayloadV3::new(
        *context.vote_round_id(),
        [0xEA; 32],
        crate::wire::PirLayout {
            pir_depth: 19,
            tier0_layers: 12,
            tier1_layers: 7,
            poly_len: 4096,
        },
        crate::round_auth::RoundAuthContextV3::new(
            context.network(),
            context.vote_chain_id(),
            1_234_567,
            [0xAB; 32],
            3,
        )
        .expect("test authority context has a valid vote-chain identifier"),
    )
}

#[cfg(test)]
pub(crate) fn test_verified_round_auth_v3(
    context: &VotingAuthorityContextV1,
) -> crate::config::VerifiedRoundAuthV3 {
    crate::config::test_verified_round_auth_v3(test_round_auth_payload_v3(context))
}

#[cfg(test)]
pub(crate) fn test_verified_voting_round_v3(
    context: &VotingAuthorityContextV1,
    round_params: crate::wire::VotingRoundParams,
) -> crate::config::VerifiedVotingRoundV3 {
    let round_id: [u8; 32] = hex::decode(&round_params.vote_round_id)
        .expect("test round id is hex")
        .try_into()
        .expect("test round id is 32 bytes");
    assert_eq!(&round_id, context.vote_round_id());
    let ea_pk: [u8; 32] = round_params
        .ea_pk
        .as_slice()
        .try_into()
        .expect("test ea_pk is 32 bytes");
    let payload = crate::round_auth::RoundAuthPayloadV3::new(
        round_id,
        ea_pk,
        crate::wire::PirLayout {
            pir_depth: 19,
            tier0_layers: 12,
            tier1_layers: 7,
            poly_len: 4096,
        },
        crate::round_auth::RoundAuthContextV3::new(
            context.network(),
            context.vote_chain_id(),
            round_params.snapshot_height,
            [0xAB; 32],
            3,
        )
        .expect("test authority context has a valid vote-chain identifier"),
    );
    let round_auth = crate::config::test_verified_round_auth_v3(payload);
    crate::config::test_verified_voting_round_v3(&round_auth, round_params)
}
