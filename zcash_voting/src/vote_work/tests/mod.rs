use crate::MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES;

use super::step_scope::{bounded_message, parse_round_id};

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
mod obligation_dispatch;
mod share_steps;
mod step_ledger;
mod wallet_scope;
