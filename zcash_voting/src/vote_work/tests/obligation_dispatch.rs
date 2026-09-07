//! A host-selected step runs the obligation the fresh plan resolves it to.

use super::fixtures::*;
use crate::{
    round_planning::{Obligation, VoteUnitId},
    session::NextStep,
};

#[tokio::test]
async fn a_confirm_share_step_for_an_accepted_share_polls_instead_of_delivering() {
    let database =
        crate::share_tracking::tests::db_with_share(&["http://helper.invalid".to_string()]);
    database
        .conn()
        .execute(
            "UPDATE votes SET tx_hash = 'aa' WHERE round_id = :round_id
               AND wallet_id = :wallet_id AND bundle_index = 0 AND proposal_id = 1",
            rusqlite::named_params! {
                ":round_id": ROUND_ID,
                ":wallet_id": database.wallet_id(),
            },
        )
        .unwrap();
    let database = Arc::new(database);
    let helper_client = HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
    let executor = RoundExecutor::new(
        database,
        ChainSubmissionClientConfig::for_network(
            Network::Testnet,
            vec!["https://chain.invalid".to_string()],
        ),
        helper_client,
    )
    .unwrap()
    .with_binding(RoundBinding {
        round_id: ROUND_ID.to_string(),
        network: Network::Testnet,
        proposals: vec![ProposalRosterEntry {
            proposal_id: 1,
            num_options: 3,
        }],
        hotkey_secret: None,
    })
    .unwrap();
    let step = NextStep::ConfirmShare {
        bundle_index: 0,
        proposal_id: 1,
        share_index: 0,
    };
    let plan = executor.plan().unwrap();
    assert!(plan.next_steps.contains(&step), "{:?}", plan.next_steps);
    assert!(!plan.blocking_share_work);

    let progress = RecordingProgress::default();
    let control = ChainSubmissionControl::new(1);
    let _ = executor
        .advance_step(step, &host(), &control, &progress)
        .await;

    let events = progress.events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, crate::RoundStepProgress::HelperPlansPrepared(_))),
        "a share a helper already holds is polled, not delivered again: {events:?}"
    );
}

#[tokio::test]
async fn a_share_with_only_ambiguous_evidence_is_polled_not_redelivered() {
    // No helper accepted the share, but one attempt ended without a usable
    // answer. Redelivery would exclude that helper and could never make
    // progress; the outcome-unknown attempt is tracking's to classify.
    let database = crate::share_tracking::tests::db_with_share(&[]);
    database
        .conn()
        .execute(
            "UPDATE share_delegations SET ambiguous_urls = '[\"http://helper.invalid\"]'
             WHERE round_id = :round_id AND wallet_id = :wallet_id
               AND bundle_index = 0 AND proposal_id = 1 AND share_index = 0",
            rusqlite::named_params! {
                ":round_id": ROUND_ID,
                ":wallet_id": database.wallet_id(),
            },
        )
        .unwrap();
    database
        .conn()
        .execute(
            "UPDATE votes SET tx_hash = 'aa' WHERE round_id = :round_id
               AND wallet_id = :wallet_id AND bundle_index = 0 AND proposal_id = 1",
            rusqlite::named_params! {
                ":round_id": ROUND_ID,
                ":wallet_id": database.wallet_id(),
            },
        )
        .unwrap();
    let database = Arc::new(database);
    let helper_client = HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
    let executor = RoundExecutor::new(
        database,
        ChainSubmissionClientConfig::for_network(
            Network::Testnet,
            vec!["https://chain.invalid".to_string()],
        ),
        helper_client,
    )
    .unwrap()
    .with_binding(RoundBinding {
        round_id: ROUND_ID.to_string(),
        network: Network::Testnet,
        proposals: vec![ProposalRosterEntry {
            proposal_id: 1,
            num_options: 3,
        }],
        hotkey_secret: None,
    })
    .unwrap();
    let step = NextStep::ConfirmShare {
        bundle_index: 0,
        proposal_id: 1,
        share_index: 0,
    };
    let plan = executor.plan().unwrap();
    assert!(plan.next_steps.contains(&step), "{:?}", plan.next_steps);
    assert!(
        !plan.blocking_share_work,
        "the outcome-unknown attempt belongs to background tracking"
    );

    let progress = RecordingProgress::default();
    let control = ChainSubmissionControl::new(1);
    let _ = executor
        .advance_step(step, &host(), &control, &progress)
        .await;

    let events = progress.events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, crate::RoundStepProgress::HelperPlansPrepared(_))),
        "an outcome-unknown share is polled, never delivered again: {events:?}"
    );
}

#[test]
fn a_reconcile_obligation_names_the_whole_batch_a_step_anchors() {
    // The projection anchors a batch step on its first member; dispatch
    // recovers every member from the obligation, never from the anchor.
    let obligations = vec![Obligation::ReconcileChain {
        unit: VoteUnitId::Batch {
            bundle_index: 0,
            ordered_batch_digest: [7; 32],
        },
        bundle_index: 0,
        ordered_proposal_ids: vec![1, 2],
        undispatched: false,
        tx_hash: None,
        prerequisite: None,
    }];
    let step = NextStep::AdvanceVoteBatch {
        bundle_index: 0,
        proposal_id: 1,
    };
    let Some(Obligation::ReconcileChain {
        ordered_proposal_ids,
        ..
    }) = crate::round_planning::resolve_step(&obligations, &step)
    else {
        panic!("a batch step resolves to its reconcile obligation");
    };
    assert_eq!(ordered_proposal_ids, &[1, 2]);
}
