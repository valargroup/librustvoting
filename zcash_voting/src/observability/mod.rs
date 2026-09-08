//! Optional invocation-local performance and failure observations.
//!
//! An entry point creates an enabled or disabled [`ObservationScope`]. Child
//! scopes share its recorder across threads without global state. Independent
//! limits bound retained records, summary groups, and active timers.
//! Reports describe execution, never replace durable chain or helper state.

mod classification;
mod collector;
mod format;
pub(crate) mod wire;
pub(crate) use classification::{
    chain_episode_outcome, chain_error_kind, chain_result_outcome, delegation_proof_outcome,
    delegation_setup_outcome, helper_error_kind, round_run_outcome, step_attribution,
    step_result_outcome, voting_error_kind,
};
mod scope;
mod types;

// Public only because the defaulted DelegationDriver extension hooks accept it.
// Construction, instrumentation, and finalization stay crate-private.
#[doc(hidden)]
pub use scope::ObservationScope;
pub use types::{
    ObservabilityOptions, ObservationAttribution, ObservationOutcome, ObservationRecord,
    ObservationSummary, OperationObservability, OperationReport,
};

#[cfg(test)]
mod tests;
