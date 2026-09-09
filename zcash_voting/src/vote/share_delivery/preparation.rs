//! Validates a whole commitment before any of its shares can enter delivery.

use crate::{
    round::VotingDb,
    share::ShareOperationScope,
    share_tracking::{ShareDeliveryPlan, ShareDeliverySubmissionParams},
    vote::CommittedVote,
    VotingError,
};

/// Immutable commitment-wide validation retained by all jobs for this proposal.
pub(super) struct PreparedVoteDelivery<'a> {
    pub(super) vote: &'a CommittedVote,
    pub(super) plan: ShareDeliveryPlan,
    pub(super) generation: String,
}

/// Loads the exact persisted plan and validates every payload before admission.
/// No share rows or network effects are created by this read-only boundary.
pub(super) fn prepare<'a>(
    vote: &'a CommittedVote,
    db: &VotingDb,
    scope: &ShareOperationScope,
    params: &ShareDeliverySubmissionParams<'_>,
) -> Result<PreparedVoteDelivery<'a>, VotingError> {
    let (plan, plan_generation) = crate::share_tracking::load_share_delivery_plan(
        db,
        scope,
        &vote.round_id,
        vote.bundle_index,
        vote.commit.proposal_id,
        &vote.commitment_bundle_json,
        params.configured_server_urls,
        &vote.commit.share_payloads,
    )?;

    let recovery = crate::recovery::helper_recovery_material_for_wallet(
        db,
        scope.wallet_id(),
        &vote.round_id,
        vote.bundle_index,
        vote.commit.proposal_id,
    )?;
    let vc_tree_position = match recovery {
        crate::recovery::HelperRecoveryMaterial::Ready(bundle)
            if bundle.commitment_bundle_json == plan_generation =>
        {
            bundle.vc_tree_position
        }
        crate::recovery::HelperRecoveryMaterial::Ready(_) => {
            return Err(VotingError::InvalidInput {
                message: "committed vote changed after loading its helper-share delivery plan"
                    .to_string(),
            })
        }
        crate::recovery::HelperRecoveryMaterial::AwaitingVcPosition => {
            return Err(VotingError::InvalidInput {
                message: "committed vote must be confirmed before submitting helper shares"
                    .to_string(),
            })
        }
        crate::recovery::HelperRecoveryMaterial::Missing => {
            return Err(VotingError::Internal {
                message: "committed vote is missing durable helper recovery material".to_string(),
            })
        }
    };
    for (payload, share_plan) in vote.commit.share_payloads.iter().zip(&plan.share_plans) {
        payload.to_wire_json(Some(vc_tree_position), share_plan.submit_at)?;
    }

    Ok(PreparedVoteDelivery {
        vote,
        plan,
        generation: plan_generation,
    })
}
