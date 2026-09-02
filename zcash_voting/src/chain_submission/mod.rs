//! Durable vocabulary for SDK-owned chain submission.
//!
//! A semantic generation describes one immutable chain effect. The lifecycle
//! may make more than one bounded network attempt for that generation, but it
//! must never reinterpret an attempt as a different vote or delegation.
//!
//! This module exposes the durable data model and host-owned HTTP transport
//! seam. Persistence and lifecycle entry points remain private so callers use
//! the SDK-owned coordination boundary.

#[allow(dead_code, reason = "internal confirmation projection")]
mod confirmation;
#[allow(dead_code, reason = "inactive lifecycle coordination foundation")]
pub(crate) mod coordination;
#[allow(dead_code, reason = "inactive lifecycle coordinator")]
mod coordinator;
#[allow(dead_code, reason = "internal generation derivation and migration")]
mod generation;
mod identity;
#[allow(dead_code, reason = "internal submission protocol")]
mod protocol;
mod result;
#[allow(dead_code, reason = "internal lifecycle transition validation")]
mod state;
#[allow(dead_code, reason = "private persistence contract for the lifecycle")]
mod store;
mod transport;

pub(crate) use generation::{
    complete_generation_for_delegation, generation_for_vote, generation_for_vote_batch,
    ExpectedTreeLayout,
};
pub(crate) use identity::{network_name, submission_identity_key};
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
