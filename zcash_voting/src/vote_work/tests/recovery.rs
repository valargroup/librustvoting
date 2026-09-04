//! The persisted-vote recovery driver shares the executor's guards.

use super::fixtures::*;

#[tokio::test]
async fn recovery_on_a_round_stored_for_another_network_is_refused_before_helper_io() {
    let database = host_database();
    let helper_client = HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
    let executor = RoundExecutor::new(
        database,
        ChainSubmissionClientConfig::for_network(
            Network::Mainnet,
            vec!["https://chain.invalid".to_string()],
        ),
        helper_client,
    )
    .unwrap();
    let control = ChainSubmissionControl::new(1);
    let proposal_ids = [1u32, 2];
    let helper_urls = vec!["http://helper.invalid".to_string()];

    let failure = executor
        .advance(
            crate::VoteRecoveryRequest {
                round_id: ROUND_ID,
                proposal_ids: &proposal_ids,
                configured_helper_urls: &helper_urls,
                now_seconds: 10,
                vote_end_time_seconds: 100_000,
                last_moment_buffer_seconds: None,
            },
            &control,
            &crate::NoopVoteRecoveryProgressReporter {},
        )
        .await
        .expect_err("the stored round is Testnet");

    assert_eq!(failure.kind, crate::VoteRecoveryFailureKind::InvalidInput);
    assert!(
        failure.message.contains("stored for network Testnet"),
        "{}",
        failure.message
    );
}

#[tokio::test]
async fn a_malformed_recovery_round_id_is_refused_rather_than_reported_idle() {
    let database = host_database();
    let helper_client = HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
    let executor = RoundExecutor::new(
        database,
        ChainSubmissionClientConfig::for_network(
            Network::Testnet,
            vec!["https://chain.invalid".to_string()],
        ),
        helper_client,
    )
    .unwrap();
    let control = ChainSubmissionControl::new(1);
    let proposal_ids = [1u32, 2];
    let helper_urls = vec!["http://helper.invalid".to_string()];

    let failure = executor
        .advance(
            crate::VoteRecoveryRequest {
                round_id: "not-a-round-id",
                proposal_ids: &proposal_ids,
                configured_helper_urls: &helper_urls,
                now_seconds: 10,
                vote_end_time_seconds: 100_000,
                last_moment_buffer_seconds: None,
            },
            &control,
            &crate::NoopVoteRecoveryProgressReporter {},
        )
        .await
        .expect_err("a typo must not be reported as an idle round");

    assert_eq!(failure.kind, crate::VoteRecoveryFailureKind::InvalidInput);
}
