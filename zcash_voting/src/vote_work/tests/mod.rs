use crate::MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES;
use std::{sync::Arc, time::Duration};

use crate::{
    ChainSubmissionClientConfig, ChainSubmissionControl, HelperClient, HelperHealth,
    HyperTransport, Network, RoundExecutor, VoteRecoveryDisposition, VoteRecoveryRequest,
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
    let executor = RoundExecutor::new(
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

#[test]
fn step_control_treats_an_epoch_change_like_cancellation() {
    use super::step_control::StepControl;
    use crate::ChainSubmissionControl;

    let control = ChainSubmissionControl::new(3);
    let captured = StepControl::capture(&control);
    assert!(!captured.interrupted());

    control.set_operation_epoch(4);
    assert!(captured.interrupted(), "a later epoch invalidates the pass");
    assert!(!control.is_cancelled());

    // A pass captured under the new epoch is live until cancelled.
    let recaptured = StepControl::capture(&control);
    assert!(!recaptured.interrupted());
    control.cancel();
    assert!(recaptured.interrupted());
    assert!(std::ptr::eq(recaptured.chain(), &control));
}

#[test]
fn vote_tree_node_urls_are_canonical_base_urls_and_https_on_mainnet() {
    use super::cast_vote::canonical_vote_tree_node_urls;
    use crate::{Network, VotingError};

    let urls = |list: &[&str]| list.iter().map(|url| url.to_string()).collect::<Vec<_>>();

    assert_eq!(
        canonical_vote_tree_node_urls(
            &urls(&["http://node.test:8080/", "https://node.test/mount///"]),
            Network::Testnet,
        )
        .unwrap(),
        urls(&["http://node.test:8080", "https://node.test/mount"]),
        "trailing slashes are removed so the API path is appended once"
    );
    assert_eq!(
        canonical_vote_tree_node_urls(&urls(&["https://node.example"]), Network::Mainnet).unwrap(),
        urls(&["https://node.example"])
    );

    for (list, network, needle) in [
        (urls(&[]), Network::Testnet, "at least one"),
        (
            urls(&["https://ok.example", "http://node.example"]),
            Network::Mainnet,
            "must use HTTPS",
        ),
        (
            urls(&["ftp://node.test"]),
            Network::Testnet,
            "http or https",
        ),
        (urls(&["not a url"]), Network::Testnet, "is invalid"),
        (urls(&["/relative/path"]), Network::Testnet, "with a host"),
        (
            urls(&["https://node.example?api_key=x"]),
            Network::Testnet,
            "without a query or fragment",
        ),
        (
            urls(&["https://node.example/#tree"]),
            Network::Testnet,
            "without a query or fragment",
        ),
    ] {
        let error = canonical_vote_tree_node_urls(&list, network).unwrap_err();
        assert!(
            matches!(error, VotingError::InvalidInput { .. }),
            "{list:?}: {error}"
        );
        assert!(error.to_string().contains(needle), "{list:?}: {error}");
    }
}

mod ballot_intents;
mod binding;
mod cancellation;
mod casting;
mod delegation_driver;
mod fixtures;
mod locking;
mod recovery;
mod share_steps;
mod wallet_scope;
