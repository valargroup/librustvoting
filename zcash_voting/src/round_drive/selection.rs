//! Which obligation the driver dispatches next, and under which lock.

use crate::session::NextStep;

/// The lock scope the executor takes for a step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StepLockScope {
    Bundle,
    Round,
}

/// Mirrors the executor's exhaustive lock-scope decision.
pub(super) fn lock_scope(step: &NextStep) -> StepLockScope {
    match step {
        NextStep::Delegate { .. } | NextStep::AdvanceDelegation { .. } => StepLockScope::Bundle,
        NextStep::AdvanceImportedDelegation { .. }
        | NextStep::CastVote { .. }
        | NextStep::AdvanceVote { .. }
        | NextStep::AdvanceVoteBatch { .. }
        | NextStep::SubmitShares { .. }
        | NextStep::ConfirmShare { .. } => StepLockScope::Round,
    }
}

/// The bundle a step belongs to. Every step names one.
pub(super) fn bundle_index(step: &NextStep) -> u32 {
    match step {
        NextStep::Delegate { bundle_index }
        | NextStep::AdvanceDelegation { bundle_index }
        | NextStep::AdvanceImportedDelegation { bundle_index }
        | NextStep::CastVote { bundle_index, .. }
        | NextStep::AdvanceVote { bundle_index, .. }
        | NextStep::AdvanceVoteBatch { bundle_index, .. }
        | NextStep::SubmitShares { bundle_index, .. }
        | NextStep::ConfirmShare { bundle_index, .. } => *bundle_index,
        // A step variant added without a bundle would be a planning change
        // this driver cannot schedule; the match is exhaustive so that shows
        // up as a compile error rather than a misrouted lock.
    }
}

/// Whether running this step asks the host for delegation signing material.
///
/// An imported capability advances without a signer, so it is not included.
pub(super) fn needs_delegation_signer(step: &NextStep) -> bool {
    matches!(
        step,
        NextStep::Delegate { .. } | NextStep::AdvanceDelegation { .. }
    )
}

/// Selects one round-locked step or a bounded ordered wave of bundle work.
///
/// Preferred re-polls lead the same ordered candidate stream as the plan.
/// Bundle waves stop at the first round-locked candidate so concurrency never
/// jumps over work that plan order says must run first.
pub(super) fn next_dispatches(
    steps: &[NextStep],
    skipped_bundles: &[u32],
    preferred: &[NextStep],
    max_bundle_concurrency: usize,
    max_dispatches: usize,
    allow_parallel_bundles: bool,
) -> Vec<NextStep> {
    if max_dispatches == 0 {
        return Vec::new();
    }

    let mut ordered = Vec::new();
    for step in preferred.iter().chain(steps) {
        if steps.contains(step)
            && !skipped_bundles.contains(&bundle_index(step))
            && !ordered.contains(step)
        {
            ordered.push(step.clone());
        }
    }
    let Some(first) = ordered.first().cloned() else {
        return Vec::new();
    };
    if lock_scope(&first) == StepLockScope::Round || !allow_parallel_bundles {
        return vec![first];
    }

    let limit = max_bundle_concurrency.min(max_dispatches);
    let mut bundles = Vec::new();
    let mut selected = Vec::new();
    for step in ordered {
        if lock_scope(&step) == StepLockScope::Round {
            break;
        }
        let bundle_index = bundle_index(&step);
        if bundles.contains(&bundle_index) {
            continue;
        }
        bundles.push(bundle_index);
        selected.push(step);
        if selected.len() == limit {
            break;
        }
    }
    selected
}
