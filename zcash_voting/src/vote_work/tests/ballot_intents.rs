//! Ballot intents are resolved against the bound roster and written atomically.

use super::fixtures::*;

#[test]
fn ballot_intents_use_the_bound_roster() {
    let executor = executor();
    let plan = executor.plan().unwrap();
    assert_eq!(plan.open_proposals, vec![1, 2]);

    let plan = executor
        .set_ballot_intents(&[BallotIntent {
            proposal_id: 2,
            decision: Decision::Skipped,
        }])
        .unwrap();
    assert_eq!(plan.open_proposals, vec![1]);

    let error = executor
        .set_ballot_intents(&[BallotIntent {
            proposal_id: 9,
            decision: Decision::Choice(0),
        }])
        .unwrap_err();
    assert_eq!(error.kind(), crate::VotingErrorKind::InvalidInput);
    assert!(error.to_string().contains("roster"));
}

#[test]
fn a_batch_naming_an_unknown_proposal_writes_nothing() {
    let executor = executor();
    let error = executor
        .set_ballot_intents(&[
            BallotIntent {
                proposal_id: 1,
                decision: Decision::Choice(0),
            },
            BallotIntent {
                proposal_id: 9,
                decision: Decision::Choice(0),
            },
        ])
        .unwrap_err();
    assert_eq!(error.kind(), crate::VotingErrorKind::InvalidInput);
    assert!(error.to_string().contains("roster"));

    // The valid leading intent must not have been applied.
    assert_eq!(executor.plan().unwrap().open_proposals, vec![1, 2]);
}

#[test]
fn a_batch_deciding_one_proposal_twice_is_rejected() {
    let executor = executor();
    let error = executor
        .set_ballot_intents(&[
            BallotIntent {
                proposal_id: 1,
                decision: Decision::Choice(0),
            },
            BallotIntent {
                proposal_id: 1,
                decision: Decision::Skipped,
            },
        ])
        .unwrap_err();
    assert_eq!(error.kind(), crate::VotingErrorKind::InvalidInput);
    assert!(error.to_string().contains("twice"));
    assert_eq!(executor.plan().unwrap().open_proposals, vec![1, 2]);
}

#[test]
fn a_batch_rejected_by_a_later_intent_rolls_back_the_earlier_one() {
    let executor = executor();
    let database = executor.database();
    database
        .set_ballot_intent(ROUND_ID, 2, Decision::Choice(1), 3)
        .unwrap();
    database
        .store_delegation_tx_hash(ROUND_ID, 0, "dtx")
        .unwrap();
    database.store_van_position(ROUND_ID, 0, 7).unwrap();
    crate::storage::queries::store_vote(&database.conn(), ROUND_ID, "wallet", 0, 2, 1, &[0xCC; 16])
        .unwrap();
    database
        .record_vote_submission(ROUND_ID, 0, 2, "vtx")
        .unwrap();

    // Proposal 1 is valid, proposal 2 now contradicts a submitted vote.
    let error = executor
        .set_ballot_intents(&[
            BallotIntent {
                proposal_id: 1,
                decision: Decision::Choice(0),
            },
            BallotIntent {
                proposal_id: 2,
                decision: Decision::Choice(2),
            },
        ])
        .unwrap_err();
    assert_eq!(error.kind(), crate::VotingErrorKind::InvalidInput);
    assert!(
        error.to_string().contains("ballot intent"),
        "unexpected error: {error}"
    );

    // Proposal 1 stays open and proposal 2 keeps its original choice.
    assert_eq!(
        database.ballot_intents(ROUND_ID).unwrap(),
        vec![(2, Decision::Choice(1))]
    );
}

#[test]
fn a_valid_batch_applies_every_intent() {
    let executor = executor();
    let plan = executor
        .set_ballot_intents(&[
            BallotIntent {
                proposal_id: 1,
                decision: Decision::Choice(0),
            },
            BallotIntent {
                proposal_id: 2,
                decision: Decision::Skipped,
            },
        ])
        .unwrap();
    assert!(plan.open_proposals.is_empty());
}

#[test]
fn intents_for_a_round_stored_under_another_network_are_refused_without_a_write() {
    // Bound before the round exists, so the binding check has nothing to
    // compare against.
    let database = host_database_for_wallet_without_round("wallet-late-round");
    let helper_client = HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
    let executor = RoundExecutor::new(
        Arc::clone(&database),
        ChainSubmissionClientConfig::for_network(
            Network::Mainnet,
            vec!["https://chain.invalid".to_string()],
        ),
        helper_client,
    )
    .unwrap()
    .with_binding(RoundBinding {
        round_id: ROUND_ID.to_string(),
        network: Network::Mainnet,
        proposals: vec![ProposalRosterEntry {
            proposal_id: 1,
            num_options: 2,
        }],
        hotkey_secret: None,
    })
    .unwrap();
    // The round then appears, stored for another network.
    crate::storage::queries::insert_round(
        &database.conn(),
        "wallet-late-round",
        Network::Testnet,
        &round_params(),
        None,
    )
    .unwrap();

    let error = executor
        .set_ballot_intents(&[BallotIntent {
            proposal_id: 1,
            decision: Decision::Choice(0),
        }])
        .expect_err("the stored round is Testnet");
    assert!(matches!(error, VotingError::InvalidInput { .. }), "{error}");
    assert!(
        error.to_string().contains("stored for network Testnet"),
        "{error}"
    );
    assert!(
        database.ballot_intents(ROUND_ID).unwrap().is_empty(),
        "a refused batch must not write into the other network's round"
    );
}
