//! Stable imports for wallet SDK integrations.
//!
//! Wallets should prefer this module over importing from internal modules. The
//! prelude intentionally contains the setup, precompute, and delegation types
//! needed by mobile SDK boundaries without exposing proof-circuit internals.

pub use crate::delegate::{
    cache_prepared_setup, clear_prepared_setups, display_memo, load_account_keys, pczt_sighash,
    prepared_epoch, record_submission, record_van_position, redact_for_signer,
    setup as setup_delegation, spend_auth_signature, submission as delegation_submission,
    take_prepared_setup, DelegationAccountKeys, DelegationKeys, DelegationPhase, DelegationProof,
    DelegationSetup, DelegationSigner, DelegationSubmission, KeystoneSigningRequest,
    PreparedDelegationReport, SignedDelegationBundle,
};
pub use crate::error::VotingError;
pub use crate::governance::BALLOT_DIVISOR;
pub use crate::hotkey::generate_hotkey;
pub use crate::pir::{select_pir_endpoint, PirEndpoint};
pub use crate::precompute::{
    note_witnesses, stored_note_witnesses, verify_witness, PirPrecomputeReport,
};
pub use crate::round::{
    note_bundles, quantized_bundle_set_weight, quantized_bundle_weight, raw_bundle_weight,
    BundleLayout, RoundInfo, RoundParams, VotingDb,
};
pub use crate::types::{
    Network, NoopProgressReporter, NoteInfo, ProgressReporter, VotingHotkey, WitnessData,
};
pub use crate::warm_proving_caches;

#[cfg(feature = "pir")]
pub use crate::precompute::delegation_pir;
