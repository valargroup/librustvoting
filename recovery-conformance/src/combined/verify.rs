use super::CombinedBundle;
use crate::{assertions::DurableSnapshot, CrashStage};
use anyhow::{ensure, Context, Result};
use zcash_voting::session::{NextStep, RoundPlan};

impl CombinedBundle {
    /// Checks the complete ordered recovery roster against the requested ballot.
    pub fn assert_complete(&self, proposals: &[u32]) -> Result<()> {
        ensure!(
            self.authorizations.len() == 1,
            "combined bundle {} has no unique authorization",
            self.bundle_index
        );
        ensure!(
            self.members.len() == proposals.len(),
            "combined bundle {} lost batch members",
            self.bundle_index
        );
        for (index, (member, proposal)) in self.members.iter().zip(proposals).enumerate() {
            ensure!(
                member.proposal_id == *proposal
                    && member.batch_index == Some(index as u32)
                    && member.batch_size == Some(proposals.len() as u32)
                    && member.combined
                    && member.batch_digest.as_ref() == Some(&self.authorizations[0].0)
                    && member.anchor_height == 0,
                "combined bundle {} has inconsistent recovery metadata for proposal {}",
                self.bundle_index,
                proposal
            );
        }
        Ok(())
    }

    /// Confirms one indivisible unit, including the final VAN and every vote.
    pub fn assert_confirmed(&self, proposals: &[u32]) -> Result<()> {
        self.assert_complete(proposals)?;
        let van = self
            .van_position
            .context("combined delegation is not confirmed")?;
        let first = self
            .members
            .first()
            .and_then(|m| m.position)
            .context("first vote is not confirmed")?;
        ensure!(
            Some(first) == van.checked_add(1),
            "combined vote positions do not follow the final VAN"
        );
        for (index, member) in self.members.iter().enumerate() {
            ensure!(
                member.position == first.checked_add(index as u64),
                "combined vote positions are partial or noncontiguous"
            );
            ensure!(
                member.tx_hash == self.delegation_hash,
                "combined delegation and votes disagree on transaction hash"
            );
            ensure!(
                member.has_plan,
                "confirmed combined member has no helper plan"
            );
        }
        Ok(())
    }
}

/// Finds precisely the armed wallet/round/bundle; another bundle is never evidence.
pub fn assert_combined_stage(
    stage: CrashStage,
    plan: &RoundPlan,
    snapshot: &DurableSnapshot,
    wallet: &str,
    round: &str,
    bundle_index: u32,
    proposals: &[u32],
) -> Result<()> {
    let bundle = snapshot
        .combined
        .iter()
        .find(|b| b.wallet_id == wallet && b.round_id == round && b.bundle_index == bundle_index)
        .context("target combined bundle missing from snapshot")?;
    let submissions: Vec<_> = snapshot
        .submissions
        .iter()
        .filter(|s| s.bundle_index == i64::from(bundle_index))
        .collect();
    let persisted = matches!(
        stage,
        CrashStage::AfterVoteCommit
            | CrashStage::AfterHelperPlans
            | CrashStage::BeforeBroadcast
            | CrashStage::BeforeVoteBroadcast
            | CrashStage::AfterBroadcastUnread
            | CrashStage::AfterVoteBroadcast
            | CrashStage::AfterBroadcastRead
            | CrashStage::AfterTracking
            | CrashStage::AfterVoteConfirmed
            | CrashStage::BeforeSharePost
            | CrashStage::AfterSharePost
            | CrashStage::AfterShareAccepted
    );
    if persisted {
        bundle.assert_complete(proposals)?;
        if stage != CrashStage::AfterVoteCommit {
            ensure!(
                bundle.members.iter().all(|m| m.has_plan),
                "target batch has incomplete helper plans"
            );
        } else {
            ensure!(
                bundle.members.iter().all(|m| !m.has_plan),
                "commit crash occurred after helper planning"
            );
            ensure!(
                submissions.is_empty(),
                "commit crash already reserved a POST"
            );
        }
        let confirmed = matches!(
            stage,
            CrashStage::AfterVoteConfirmed
                | CrashStage::BeforeSharePost
                | CrashStage::AfterSharePost
                | CrashStage::AfterShareAccepted
        );
        if confirmed {
            bundle.assert_confirmed(proposals)?;
            ensure!(
                submissions.len() == 1 && submissions[0].state == "confirmed",
                "combined confirmation missing"
            );
        } else {
            ensure!(
                bundle.van_position.is_none()
                    && bundle.members.iter().all(|m| m.position.is_none()),
                "unconfirmed combined unit partially confirmed"
            );
            let target_steps: Vec<_> = plan
                .next_steps
                .iter()
                .filter(|step| match step {
                    NextStep::AdvanceVoteBatch {
                        bundle_index: b, ..
                    }
                    | NextStep::AdvanceVote {
                        bundle_index: b, ..
                    }
                    | NextStep::AdvanceDelegation { bundle_index: b }
                    | NextStep::Delegate { bundle_index: b }
                    | NextStep::CastVote {
                        bundle_index: b, ..
                    } => *b == bundle_index,
                    _ => false,
                })
                .collect();
            ensure!(
                target_steps.len() == 1
                    && matches!(target_steps[0], NextStep::AdvanceVoteBatch { .. }),
                "combined unit must plan exactly one batch reconciliation: {target_steps:?}"
            );
        }
        if !matches!(
            stage,
            CrashStage::AfterVoteCommit | CrashStage::AfterHelperPlans
        ) {
            ensure!(
                submissions.len() == 1 && submissions[0].kind == "delegate_and_cast_vote_batch",
                "combined unit has split or missing lifecycle ownership"
            );
            let submission = submissions[0];
            if matches!(
                stage,
                CrashStage::BeforeBroadcast
                    | CrashStage::BeforeVoteBroadcast
                    | CrashStage::AfterBroadcastUnread
                    | CrashStage::AfterVoteBroadcast
                    | CrashStage::AfterBroadcastRead
            ) {
                ensure!(
                    submission.state == "submitting" && !submission.has_candidate_hash,
                    "broadcast boundary did not leave an unclassified reservation"
                );
            }
            if stage == CrashStage::AfterTracking {
                ensure!(
                    submission.state == "tracking" && submission.has_candidate_hash,
                    "tracking crash did not leave a tracked candidate"
                );
            }
        }
    } else {
        ensure!(
            bundle.authorizations.is_empty() && bundle.members.is_empty() && submissions.is_empty(),
            "pre-persistence crash already has durable cast work"
        );
        ensure!(
            bundle.van_position.is_none(),
            "preparation crash already confirmed the delegation"
        );
        if matches!(
            stage,
            CrashStage::BeforeDelegation | CrashStage::AfterNoteSelection
        ) {
            ensure!(
                bundle.pczt_fingerprint.is_none() && bundle.proof_fingerprint.is_none(),
                "pre-PCZT crash already persisted preparation"
            );
        }
        if stage == CrashStage::AfterPczt {
            ensure!(
                bundle.proof_fingerprint.is_none(),
                "PCZT crash already persisted the proof"
            );
        }
        if !matches!(
            stage,
            CrashStage::BeforeDelegation | CrashStage::AfterNoteSelection
        ) {
            ensure!(bundle.pczt_fingerprint.is_some(), "target PCZT missing");
        }
        if matches!(
            stage,
            CrashStage::AfterProof
                | CrashStage::BeforeCast
                | CrashStage::AfterSigning
                | CrashStage::AfterVoteProof
        ) {
            ensure!(
                bundle.proof_fingerprint.is_some(),
                "target delegation proof missing"
            );
        }
    }
    // Delivery is forbidden until the complete unit is confirmed, even if a
    // different bundle has already reached that point.
    for delivery in snapshot
        .deliveries
        .iter()
        .filter(|s| s.bundle_index == i64::from(bundle_index))
    {
        if !delivery.touched().is_empty() || delivery.confirmed {
            bundle.assert_confirmed(proposals)?;
        }
    }
    Ok(())
}

/// Every bundle of a completed live round must retain complete batch evidence.
pub fn assert_combined_terminal(snapshot: &DurableSnapshot, proposals: &[u32]) -> Result<()> {
    ensure!(
        snapshot.combined.len() == crate::provisioning::EXPECTED_BUNDLE_COUNT,
        "terminal round has an unexpected bundle count"
    );
    ensure!(
        snapshot.submissions.len() == snapshot.combined.len(),
        "terminal round does not have one submission per bundle"
    );
    for bundle in &snapshot.combined {
        bundle.assert_confirmed(proposals)?;
        ensure!(
            snapshot
                .submissions
                .iter()
                .any(|s| s.bundle_index == i64::from(bundle.bundle_index)
                    && s.kind == "delegate_and_cast_vote_batch"
                    && s.state == "confirmed"),
            "terminal batch lifecycle missing"
        );
    }
    Ok(())
}

/// Reopening preserves prepared proofs and every already-persisted authorization.
pub fn assert_preserved_combined(before: &DurableSnapshot, after: &DurableSnapshot) -> Result<()> {
    for bundle in &before.combined {
        let resumed = after
            .combined
            .iter()
            .find(|b| {
                b.wallet_id == bundle.wallet_id
                    && b.round_id == bundle.round_id
                    && b.bundle_index == bundle.bundle_index
            })
            .context("bundle vanished across resume")?;
        if bundle.pczt_fingerprint.is_some() {
            ensure!(
                bundle.pczt_fingerprint == resumed.pczt_fingerprint,
                "PCZT changed across resume"
            );
        }
        if bundle.proof_fingerprint.is_some() {
            ensure!(
                bundle.proof_fingerprint == resumed.proof_fingerprint,
                "delegation proof changed across resume"
            );
        }
        if !bundle.authorizations.is_empty() {
            ensure!(
                bundle.authorizations == resumed.authorizations,
                "combined authorization changed across resume"
            );
            let membership = |b: &CombinedBundle| {
                b.members
                    .iter()
                    .map(|m| {
                        (
                            m.proposal_id,
                            m.batch_digest.clone(),
                            m.batch_index,
                            m.batch_size,
                        )
                    })
                    .collect::<Vec<_>>()
            };
            ensure!(
                membership(bundle) == membership(resumed),
                "combined membership changed across resume"
            );
        }
    }
    Ok(())
}
