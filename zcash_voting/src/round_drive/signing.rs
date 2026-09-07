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
/// Each admitted step is read against **its own** bundle's context, not one
/// context taken for the wave. [`RoundHostSource`](super::RoundHostSource) is
/// sampled once per dispatch and nothing requires two samples to agree, so a
/// single mode applied to every bundle would either broadcast one under a
/// signer the host had stopped offering, or demand a durable row for a bundle
/// whose own context signs during its step — a handoff the host can never
/// satisfy, because there is nothing for it to store.
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
    let admitted: Vec<(u32, &RoundHostContext)> = dispatches
        .iter()
        .filter(|(step, _)| selection::needs_delegation_signer(step))
        .map(|(step, context)| (selection::bundle_index(step), context))
        .collect();
    if admitted.is_empty() {
        return Ok(Vec::new());
    }

    // A bundle whose own context produces its signature during the step is
    // owed nothing, whatever the rest of the wave uses.
    let mut signs_itself = Vec::new();
    let mut cannot_sign = false;
    let mut reads_stored_material = false;
    for (bundle_index, context) in &admitted {
        match context.delegation.as_ref().map(|inputs| &inputs.signer) {
            // No delegation inputs at all: this step cannot sign, and the
            // round-wide rule below then applies to what it owes.
            None => cannot_sign = true,
            Some(DelegationSigner::Keystone(KeystoneSignatureSource::Stored)) => {
                reads_stored_material = true;
            }
            Some(_) => signs_itself.push(*bundle_index),
        }
    }
    let owed: Vec<u32> = required
        .into_iter()
        .filter(|bundle_index| !signs_itself.contains(bundle_index))
        .collect();
    if owed.is_empty() {
        return Ok(Vec::new());
    }
    if cannot_sign {
        return Ok(owed);
    }
    if !reads_stored_material {
        return Ok(Vec::new());
    }

    // The round-wide part: bundles this wave has not reached are owed a
    // durable row too, so the voter signs once rather than a wave at a time.
    let stored = executor.database().get_keystone_signatures(round_id)?;
    Ok(owed
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
