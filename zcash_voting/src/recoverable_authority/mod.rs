//! Deterministic voting authority construction for recoverable rounds.
//!
//! The caller supplies public wallet and vote-chain context. Seed access and
//! durable secret backup remain outside this crate.

mod derivation;

pub use derivation::{
    orchard_fvk_fingerprint_v1, KeystoneMasterGenerationV1, RecoverableVotingHotkeyV1,
    RegisteredKeyApplicationV1, SoftwareRegisteredKeyRequestV1, VotingAuthorityContextV1,
    VotingAuthorityRootBindingV1, VotingAuthorityRootSourceV1, VotingAuthorityRootV1,
    VotingAuthorityScopeV1, KEYSTONE_MASTER_SECRET_LEN, VOTING_AUTHORITY_ROOT_LEN,
};
