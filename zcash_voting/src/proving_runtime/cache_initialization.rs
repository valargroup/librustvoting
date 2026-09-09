//! Cache initialization is single-flighted outside proof admission.

use crate::{ObservationScope, VotingError};
use std::sync::OnceLock;

#[derive(Clone, Copy)]
pub(crate) enum CacheKind {
    Delegation,
    Vote,
}
static DELEGATION: OnceLock<Result<(), String>> = OnceLock::new();
static VOTE: OnceLock<Result<(), String>> = OnceLock::new();

/// Waiters never occupy pool workers or heavy-job permits while keys are cold.
pub(crate) fn ensure_cache(
    kind: CacheKind,
    observations: &ObservationScope,
) -> Result<(), VotingError> {
    let runtime = super::runtime().map_err(super::internal)?;
    if runtime.pool.current_thread_index().is_some() {
        return Err(super::internal(
            "cache initialization requested on a CPU worker",
        ));
    }
    let cache = match kind {
        CacheKind::Delegation => &DELEGATION,
        CacheKind::Vote => &VOTE,
    };
    observations.measure_result("proving::cache_ready", || {
        cache
            .get_or_init(|| {
                // Shared cache work outlives any one requesting operation.
                let cache_operation = super::Operation::controlled(
                    format!(
                        "cache:{}",
                        match kind {
                            CacheKind::Delegation => "delegation",
                            CacheKind::Vote => "vote",
                        }
                    ),
                    crate::ChainSubmissionControl::new(0),
                    0,
                );
                cache_operation
                    .enter(|| {
                        super::execute(observations, || match kind {
                            CacheKind::Delegation => {
                                voting_circuits::delegation::warm_delegation_keys()
                                    .map_err(|error| super::internal(error.to_string()))
                            }
                            CacheKind::Vote => voting_circuits::vote_proof::warm_vote_proof_keys()
                                .map_err(|error| super::internal(error.to_string())),
                        })
                    })
                    .map_err(|error| error.to_string())
            })
            .as_ref()
            .map(|_| ())
            .map_err(|message| VotingError::ProofFailed {
                message: message.clone(),
            })
    })
}
