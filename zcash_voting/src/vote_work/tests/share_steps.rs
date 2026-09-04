//! Helper-share steps: a share no helper holds is delivered, not polled.

use super::fixtures::*;

#[tokio::test]
async fn a_blocking_confirm_share_step_delivers_before_polling() {
    // Share 0 is durable but no helper accepted it: every initial POST
    // failed definitely, so the planner lists it as blocking share work.
    let database = crate::share_tracking::tests::db_with_share(&[]);
    // The vote itself is confirmed on chain; only its share is undelivered.
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
    assert!(plan.blocking_share_work);

    let progress = RecordingProgress::default();
    let control = ChainSubmissionControl::new(1);
    let _ = executor
        .advance_step(step, &host(), &control, &progress)
        .await;

    let events = progress.events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, crate::RoundStepProgress::HelperPlansPrepared(_))),
        "the share must be driven through delivery from its durable plan: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, crate::RoundStepProgress::ShareConfirmed { .. })),
        "a share no helper holds must not be polled for confirmation: {events:?}"
    );
}

#[tokio::test]
async fn a_dispatched_vote_is_reconciled_before_its_ballot_is_terminal() {
    // The vote for proposal 1 is on the wire (tx hash, no tree position yet)
    // while proposal 2 is still undecided, so no helper plan can be derived.
    let database = crate::share_tracking::tests::db_with_share(&[]);
    database
        .conn()
        .execute(
            "UPDATE votes SET tx_hash = 'aa', vc_tree_position = NULL
             WHERE round_id = :round_id AND wallet_id = :wallet_id
               AND bundle_index = 0 AND proposal_id = 1",
            rusqlite::named_params! {
                ":round_id": ROUND_ID,
                ":wallet_id": database.wallet_id(),
            },
        )
        .unwrap();
    // Recovery checks the stored commitment against the recovery bundle.
    let recovery = crate::vote::recovery_bundle(&database, ROUND_ID, 0, 1)
        .unwrap()
        .unwrap();
    let commitment = crate::vote::stored_vote_commitment_bytes(&recovery).unwrap();
    database
        .conn()
        .execute(
            "UPDATE votes SET commitment = :commitment WHERE round_id = :round_id
               AND wallet_id = :wallet_id AND bundle_index = 0 AND proposal_id = 1",
            rusqlite::named_params! {
                ":commitment": commitment,
                ":round_id": ROUND_ID,
                ":wallet_id": database.wallet_id(),
            },
        )
        .unwrap();
    let control = ChainSubmissionControl::new(1);
    let chain = UnreachableChainTransport::cancelling(&control);
    let helper_client = HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
    let executor = RoundExecutor::with_transport(
        Arc::new(database),
        Arc::clone(&chain),
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
        proposals: vec![
            ProposalRosterEntry {
                proposal_id: 1,
                num_options: 3,
            },
            ProposalRosterEntry {
                proposal_id: 2,
                num_options: 3,
            },
        ],
        hotkey_secret: None,
    })
    .unwrap();
    let step = NextStep::AdvanceVote {
        bundle_index: 0,
        proposal_id: 1,
    };
    let plan = executor.plan().unwrap();
    assert!(plan.next_steps.contains(&step), "{:?}", plan.next_steps);
    assert_eq!(plan.open_proposals, vec![2]);

    let progress = RecordingProgress::default();
    let result = executor
        .advance_step(step, &host(), &control, &progress)
        .await;

    assert!(
        chain.requests.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "the chain must be consulted even though the ballot is not terminal: {result:?}"
    );
    if let Err(failure) = &result {
        assert!(
            !failure.message.contains("terminal decisions"),
            "helper-plan derivation must not gate chain reconciliation: {}",
            failure.message
        );
    }
    let events = progress.events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, crate::RoundStepProgress::HelperPlansPrepared(_))),
        "no plan is prepared before the chain is reconciled: {events:?}"
    );
}
