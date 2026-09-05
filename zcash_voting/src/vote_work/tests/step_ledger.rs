//! What a step accomplished rides on every outcome and failure it reports.

use super::fixtures::*;
use crate::{
    session::NextStep,
    share_tracking::{
        ShareBatchDeliveryReport, ShareDeliveryOutcome, SharePlacementGuarantee,
        ShareSubmissionReport,
    },
    vote_work::{step_control::StepControl, step_ledger::StepLedger, step_scope::StepScope},
    ChainSubmissionControl, ChainSubmissionResult, RoundStepDisposition, RoundStepFailureKind,
    VoteRecoveryKey, VoteShareDeliveryReport,
};

fn accepted_delivery() -> VoteShareDeliveryReport {
    VoteShareDeliveryReport {
        vote: VoteRecoveryKey {
            bundle_index: 0,
            proposal_id: 1,
        },
        delivery: ShareBatchDeliveryReport {
            deliveries: vec![ShareDeliveryOutcome {
                share_index: 0,
                submission: ShareSubmissionReport {
                    accepted_urls: vec!["https://helper-a.example".to_string()],
                    ambiguous_urls: Vec::new(),
                    target_count: 1,
                },
            }],
            pending_share_indices: Vec::new(),
            cancelled: false,
            placement_guarantee: SharePlacementGuarantee::Strict,
        },
    }
}

#[tokio::test]
async fn a_failure_and_a_cancellation_carry_the_chain_outcome_and_deliveries_so_far() {
    let (executor, _database) = bound_executor(host_database(), None);
    let host = host();
    let control = ChainSubmissionControl::new(1);
    let scope = StepScope::capture(
        &executor,
        NextStep::AdvanceVote {
            bundle_index: 0,
            proposal_id: 1,
        },
        &host,
        StepControl::capture(&control),
    )
    .unwrap();
    let mut ledger = StepLedger::default();
    ledger.record_chain_outcome(ChainSubmissionResult::Cancelled);
    ledger.record_delivery(accepted_delivery());

    let failure = executor.step_failure(
        RoundStepFailureKind::Transport,
        Some(&scope.step),
        None,
        &ledger,
        "a later helper became unreachable",
    );
    assert_eq!(failure.kind, RoundStepFailureKind::Transport);
    assert!(
        matches!(
            failure.chain_outcome,
            Some(ChainSubmissionResult::Cancelled)
        ),
        "the chain outcome recorded before the failure must survive it: {failure:?}"
    );
    assert_eq!(failure.share_deliveries, vec![accepted_delivery()]);

    let cancelled = executor.step_cancelled(&scope, ledger).unwrap();
    assert_eq!(cancelled.disposition, RoundStepDisposition::Cancelled);
    assert!(matches!(
        cancelled.chain_outcome,
        Some(ChainSubmissionResult::Cancelled)
    ));
    assert_eq!(cancelled.share_deliveries, vec![accepted_delivery()]);
}
