//! The signing material a wave needs before anything is dispatched.
//!
//! Delegation is the one obligation the SDK cannot complete on its own: the
//! voter's signature comes from outside. This child answers one question for
//! the run loop — which bundles the host still owes a signature for — and the
//! loop turns a non-empty answer into a handoff.

use crate::{
    round_planning::Obligation, session::NextStep, ChainTransport, DelegationSigner,
    KeystoneSignatureSource, RoundExecutor, RoundHostContext, VotingError,
};

use super::selection;

/// Delegation bundles the host must supply a signature for before this run
/// can dispatch anything.
///
/// The answer covers **every** bundle the round still owes a delegation for,
/// not just the ones this wave would run. A wave is bounded by the concurrency
/// limit, so checking only its members would prove and broadcast the signed
/// bundles first and report the unsigned ones one wave later — the host would
/// collect signatures in several rounds, and work would already have happened
/// before the first of them.
///
/// Every admitted step that asks for a signer is examined, not just the first.
/// [`RoundHostSource`](super::RoundHostSource) is sampled once per dispatch and
/// nothing requires two samples to agree, so treating the first as
/// representative could broadcast one bundle under a signer the host had
/// already stopped offering, or demand a stored signature for a bundle whose
/// own context could sign it.
pub(super) fn missing_signer_bundles<T: ChainTransport>(
    executor: &RoundExecutor<T>,
    dispatches: &[(NextStep, RoundHostContext)],
    round_id: &str,
    obligations: &[Obligation],
    skipped: &[u32],
) -> Result<Vec<u32>, VotingError> {
    let required = signer_bundles(obligations, skipped);
    if required.is_empty() {
        return Ok(Vec::new());
    }
    // A wave with no delegation work cannot be blocked by a signature: plan
    // order puts a bundle's delegation ahead of everything that depends on it,
    // so its vote and share work is not selected yet.
    let signer_contexts: Vec<&RoundHostContext> = dispatches
        .iter()
        .filter(|(step, _)| selection::needs_delegation_signer(step))
        .map(|(_, context)| context)
        .collect();
    if signer_contexts.is_empty() {
        return Ok(Vec::new());
    }
    // One admitted step with no delegation inputs at all cannot sign, and the
    // round-wide rule then applies to every bundle it owes.
    if signer_contexts
        .iter()
        .any(|context| context.delegation.is_none())
    {
        return Ok(required);
    }
    // Every other signer produces its signature during the step, so the
    // stored-material gate applies exactly when some admitted step depends on
    // material that must already exist.
    let reads_stored_material = signer_contexts.iter().any(|context| {
        matches!(
            context.delegation.as_ref().map(|inputs| &inputs.signer),
            Some(DelegationSigner::Keystone(KeystoneSignatureSource::Stored))
        )
    });
    if !reads_stored_material {
        return Ok(Vec::new());
    }

    let stored = executor.database().get_keystone_signatures(round_id)?;
    Ok(required
        .into_iter()
        .filter(|bundle_index| {
            !stored
                .iter()
                .any(|record| record.bundle_index == *bundle_index)
        })
        .collect())
}

/// The bundles whose delegation obligations still need signing material.
fn signer_bundles(obligations: &[Obligation], skipped: &[u32]) -> Vec<u32> {
    let mut bundles: Vec<u32> = obligations
        .iter()
        .filter_map(|obligation| match obligation {
            Obligation::Delegate { bundle_index } => Some(*bundle_index),
            Obligation::AdvanceDelegation {
                bundle_index,
                imported: false,
                ..
            } => Some(*bundle_index),
            _ => None,
        })
        .filter(|bundle_index| !skipped.contains(bundle_index))
        .collect();
    bundles.sort_unstable();
    bundles.dedup();
    bundles
}
