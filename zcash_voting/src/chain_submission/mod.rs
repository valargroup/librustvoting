//! Durable vocabulary for SDK-owned chain submission.
//!
//! A semantic generation describes one immutable chain effect. The lifecycle
//! may make more than one bounded network attempt for that generation, but it
//! must never reinterpret an attempt as a different vote or delegation.
//!
//! This module exposes the additive data model and the host-owned HTTP
//! transport seam. Persistence and lifecycle entry points are not yet
//! included. The protocol client and transition reducer remain private so
//! callers cannot bypass the eventual coordinator.

mod identity;
#[allow(dead_code, reason = "used by the chain submission coordinator")]
mod protocol;
mod result;
// The reducer lands before the coordinator calls it. Keep the inactive
// foundation private without producing crate warnings.
#[allow(dead_code, reason = "used by the chain submission coordinator")]
mod state;
mod transport;

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
    ChainHttpRequest, ChainHttpResponse, ChainTransport, ChainTransportError,
    ChainTransportFailureKind, ChainTransportFuture, MAX_CHAIN_HTTP_RESPONSE_BYTES,
};
