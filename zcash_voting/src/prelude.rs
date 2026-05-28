//! Stable imports for wallet SDK integrations.
//!
//! Wallets should prefer this module over importing from internal modules. The
//! prelude intentionally contains the setup, precompute, and delegation types
//! needed by mobile SDK boundaries without exposing proof-circuit internals.

pub use crate::delegate::{
    pczt_sighash, record_submission, record_van_position, setup as setup_delegation,
    spend_auth_signature, submission as delegation_submission, DelegationKeys, DelegationPhase,
    DelegationProof, DelegationSetup, DelegationSigner, DelegationSubmission,
};
pub use crate::error::VotingError;
pub use crate::governance::BALLOT_DIVISOR;
pub use crate::hotkey::generate_hotkey;
pub use crate::pir::{select_pir_endpoint, PirEndpoint};
pub use crate::precompute::{
    note_witnesses, stored_note_witnesses, verify_witness, PirPrecomputeReport,
};
pub use crate::round::{note_bundles, BundleLayout, RoundInfo, RoundParams, VotingDb};
pub use crate::types::{
    Network, NoopProgressReporter, NoteInfo, ProgressReporter, VotingHotkey, WitnessData,
};
pub use crate::warm_proving_caches;

#[cfg(feature = "pir")]
pub use crate::precompute::delegation_pir;
