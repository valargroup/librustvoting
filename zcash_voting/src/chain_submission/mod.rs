//! Durable vocabulary for SDK-owned chain submission.
//!
//! A semantic generation describes one immutable chain effect. The lifecycle
//! may make more than one bounded network attempt for that generation, but it
//! must never reinterpret an attempt as a different vote or delegation.
//!
//! This module exposes the durable data model, host-owned HTTP transport seam,
//! and bounded delegation and singleton-vote lifecycle client. Persistence and
//! coordination remain internal SDK mechanisms.

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
#[allow(dead_code, reason = "internal submission protocol")]
mod protocol;
mod recovery;
mod result;
#[allow(dead_code, reason = "internal lifecycle transition validation")]
mod state;
#[allow(dead_code, reason = "internal persistence contract for the lifecycle")]
mod store;
mod transport;

pub use client::{
    AdvanceDelegation, AdvanceVote, AdvanceVoteBatch, ChainRecoveryMode, ChainSubmissionClient,
    ChainSubmissionClientConfig, ChainSubmissionControl,
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
