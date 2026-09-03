use crate::MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES;
use std::{sync::Arc, time::Duration};

use crate::{
    ChainSubmissionClientConfig, ChainSubmissionControl, HelperClient, HelperHealth,
    HyperTransport, Network, VoteRecoveryDisposition, VoteRecoveryExecutor, VoteRecoveryRequest,
};

use super::execution::{bounded_message, parse_round_id};

#[test]
fn failure_messages_are_bounded_and_escape_control_characters() {
    let message = format!(
        "secret\n{}",
        "x".repeat(MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES * 2)
    );
    let bounded = bounded_message(&message);
    assert!(!bounded.contains('\n'));
    assert!(bounded.len() <= MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES);
}

#[test]
fn round_id_parser_rejects_noncanonical_input() {
    let error = parse_round_id(&"A".repeat(64)).unwrap_err();
    assert!(error.to_string().contains("lowercase hex"));
}

#[tokio::test]
async fn no_persisted_vote_work_returns_without_network_io() {
    let database = Arc::new(crate::round::VotingDb::open_in_memory().unwrap());
    database.set_wallet_id("wallet");
    let helper_client = HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
    let executor = VoteRecoveryExecutor::new(
        database,
        ChainSubmissionClientConfig {
            network: Network::Testnet,
            vote_chain_id: "svote-1".to_string(),
            endpoints: vec!["http://chain.invalid".to_string()],
            tracking_window: Duration::from_secs(90),
            maximum_post_attempts: 1,
            retry_backoffs: Vec::new(),
        },
        helper_client,
    )
    .unwrap();
    let proposal_ids = [1];
    let helper_urls = ["http://helper.invalid".to_string()];
    let outcome = executor
        .advance(
            VoteRecoveryRequest {
                round_id: &"01".repeat(32),
                proposal_ids: &proposal_ids,
                configured_helper_urls: &helper_urls,
                now_seconds: 10,
                vote_end_time_seconds: 100,
                last_moment_buffer_seconds: Some(20),
            },
            &ChainSubmissionControl::new(1),
            &crate::NoopVoteRecoveryProgressReporter {},
        )
        .await
        .unwrap();

    assert_eq!(outcome.disposition, VoteRecoveryDisposition::NoWork);
    assert!(outcome.attempted_work.is_none());
    assert!(outcome.round_plan.open_proposals.contains(&1));
}
