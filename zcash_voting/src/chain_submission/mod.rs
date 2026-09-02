//! Durable vocabulary for SDK-owned chain submission.
//!
//! A semantic generation describes one immutable chain effect. The lifecycle
//! may make more than one bounded network attempt for that generation, but it
//! must never reinterpret an attempt as a different vote or delegation.
//!
//! This module currently exposes only the additive data model. Networking,
//! persistence, and lifecycle entry points are not yet included. The
//! transition reducer is deliberately private so callers cannot bypass the
//! eventual coordinator.

mod identity;
mod result;
// The reducer lands before the coordinator calls it. Keep the inactive
// foundation private without producing crate warnings.
#[allow(dead_code, reason = "used by the chain submission coordinator")]
mod state;

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
