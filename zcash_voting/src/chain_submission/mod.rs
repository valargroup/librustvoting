//! Durable vocabulary for SDK-owned chain submission.
//!
//! A semantic generation describes one immutable chain effect. The lifecycle
//! may make more than one bounded network attempt for that generation, but it
//! must never reinterpret an attempt as a different vote or delegation.
//!
//! This module exposes the durable data model, host-owned HTTP transport seam,
//! and bounded delegation, singleton-vote, and atomic vote-batch lifecycle
//! client. Persistence and coordination remain internal SDK mechanisms.
//!
//! # Removed legacy mutation APIs
//!
//! [`ChainSubmissionClient`] is the only route to submission, polling,
//! recovery, and confirmation. The version-17 entry points that let a host
//! record a transaction hash, record a VAN or vote-commitment position, or
//! apply its own parsed chain events were removed, and the raw storage writers
//! behind them are no longer public. Event parsing, tree matching, and the
//! domain and helper-share writes are private lifecycle mechanisms.
//! Delegation callers likewise provide only a SpendAuth signature; the SDK
//! loads the locked sighash and randomized verification key itself.
//!
//! The doctests below are the compile-time surface check required by
//! `docs/chain_submission_invariants.md`. Each must fail to compile; if one
//! starts compiling, a bypass has been reintroduced.
//!
//! Caller-controlled delegation sighash assembly is gone:
//!
//! ```compile_fail
//! let _ = zcash_voting::delegate::DelegationSigner::signature;
//! ```
//!
//! Caller-controlled confirmation is gone:
//!
//! ```compile_fail
//! use zcash_voting::prelude::*;
//! let _ = confirm_delegation_submission;
//! ```
//!
//! ```compile_fail
//! use zcash_voting::prelude::*;
//! let _ = confirm_vote_submission;
//! ```
//!
//! ```compile_fail
//! use zcash_voting::prelude::*;
//! let _ = confirm_vote_batch_submission;
//! ```
//!
//! Caller-controlled transaction-hash recording is gone:
//!
//! ```compile_fail
//! let _ = zcash_voting::delegate::record_submission;
//! ```
//!
//! Caller-controlled position recording is gone:
//!
//! ```compile_fail
//! let _ = zcash_voting::delegate::record_van_position;
//! ```
//!
//! The vote-side recorders are crate-private test helpers and stay off the
//! surface even when the `test-fixtures` feature is enabled:
//!
//! ```compile_fail
//! let _ = zcash_voting::vote::record_submission;
//! ```
//!
//! ```compile_fail
//! let _ = zcash_voting::vote::record_batch_submission;
//! ```
//!
//! ```compile_fail
//! let _ = zcash_voting::vote::record_vc_position;
//! ```
//!
//! ```compile_fail
//! let _ = zcash_voting::round::VotingDb::mark_vote_submitted;
//! ```
//!
//! The host chain-event vocabulary is private:
//!
//! ```compile_fail
//! let _: Option<zcash_voting::confirmation::TxEvent> = None;
//! ```
//!
//! The legacy confirmation DTOs are not reachable through the public wire
//! module either:
//!
//! ```compile_fail
//! let _: Option<zcash_voting::wire::DelegationConfirmation> = None;
//! ```
//!
//! ```compile_fail
//! let _: Option<zcash_voting::wire::VoteConfirmation> = None;
//! ```
//!
//! ```compile_fail
//! let _: Option<zcash_voting::wire::VoteBatchConfirmation> = None;
//! ```
//!
//! So is the atomic domain projection behind confirmation:
//!
//! ```compile_fail
//! let _ = zcash_voting::confirmation::apply_delegation_confirmation_with_conn;
//! ```
//!
//! The storage facade does not re-export the raw writers:
//!
//! ```compile_fail
//! let _ = zcash_voting::storage::queries::store_delegation_tx_hash;
//! ```
//!
//! ```compile_fail
//! let _ = zcash_voting::storage::queries::store_van_position;
//! ```
//!
//! ```compile_fail
//! let _ = zcash_voting::storage::queries::record_vote_submission;
//! ```
//!
//! Chain-ready payload builders are gone, because the lifecycle owns dispatch:
//!
//! ```compile_fail
//! let _ = zcash_voting::vote::submission;
//! ```
//!
//! ```compile_fail
//! let _ = zcash_voting::delegate::submission;
//! ```

mod client;
#[allow(dead_code, reason = "internal confirmation projection")]
mod confirmation;
#[allow(dead_code, reason = "internal lifecycle coordination")]
pub(crate) mod coordination;
#[allow(dead_code, reason = "internal lifecycle engine")]
mod coordinator;
#[allow(dead_code, reason = "internal generation derivation")]
mod generation;
mod identity;
pub(crate) mod planning;
#[allow(dead_code, reason = "internal submission protocol")]
mod protocol;
mod recovery;
mod result;
#[allow(dead_code, reason = "internal lifecycle transition validation")]
mod state;
#[allow(dead_code, reason = "internal persistence contract for the lifecycle")]
mod store;
mod transport;

#[cfg(test)]
mod tests;

pub use client::{
    AdvanceDelegation, AdvanceImportedDelegation, AdvanceVote, AdvanceVoteBatch, ChainRecoveryMode,
    ChainSubmissionClient, ChainSubmissionClientConfig, ChainSubmissionControl,
    DEFAULT_CHAIN_MAXIMUM_POST_ATTEMPTS, DEFAULT_CHAIN_RETRY_BACKOFFS,
    DEFAULT_CHAIN_TRACKING_WINDOW,
};
#[cfg(test)]
pub(crate) use generation::generation_for_vote;
pub(crate) use generation::generation_for_vote_batch;
#[cfg(test)]
pub(crate) use identity::submission_identity_key;
pub use identity::{
    CandidateTransactionHash, CandidateTransactionHashError, ChainSubmissionGeneration,
    ChainSubmissionGenerationDigest, ChainSubmissionIdentity, ChainSubmissionIdentityError,
    ChainSubmissionTarget,
};
pub use result::{
    ChainSubmissionConfirmation, ChainSubmissionConfirmationError,
    ChainSubmissionConfirmationSource, ChainSubmissionDiagnostic, ChainSubmissionDiagnosticKind,
    ChainSubmissionFailure, ChainSubmissionFailureKind, ChainSubmissionFailureState,
    ChainSubmissionPending, ChainSubmissionResult, ChainSubmissionState,
    ChainSubmissionStateEvidence, MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES,
};
pub use transport::{
    ChainHttpRequest, ChainHttpResponse, ChainPostDispatch, ChainTransport, ChainTransportError,
    ChainTransportFailureKind, ChainTransportFuture, MAX_CHAIN_HTTP_RESPONSE_BYTES,
};
