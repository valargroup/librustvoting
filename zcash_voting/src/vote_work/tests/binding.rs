//! `with_binding` validates the round, network, roster, and hotkey up front.

use super::fixtures::*;

#[test]
fn unbound_executor_rejects_the_step_api() {
    let database = Arc::new(crate::round::VotingDb::open_in_memory().unwrap());
    database.set_wallet_id("wallet");
    let helper_client = HelperClient::new(Arc::new(HyperTransport::new()), HelperHealth::default());
    let executor = RoundExecutor::new(
        database,
        ChainSubmissionClientConfig::for_network(
            Network::Testnet,
            vec!["http://chain.invalid".to_string()],
        ),
        helper_client,
    )
    .unwrap();
    let error = executor.plan().unwrap_err();
    assert_eq!(error.kind(), crate::VotingErrorKind::InvalidInput);
}

#[test]
fn host_context_derives_timing_from_the_shared_policy() {
    let mut host = host();
    assert_eq!(
        host.last_moment_buffer_seconds(),
        crate::share::policy::last_moment_buffer_seconds(0, 100_000)
    );
    assert!(!host.is_last_moment());
    host.now_seconds = 99_999;
    assert!(host.is_last_moment());
    host.vote_end_time_seconds = None;
    assert_eq!(host.last_moment_buffer_seconds(), None);
    assert!(!host.is_last_moment());
    assert_eq!(host.planning_vote_end_seconds(), 99_999);
}

#[test]
fn a_binding_requires_a_nonempty_distinct_roster() {
    let binding = |proposals: Vec<ProposalRosterEntry>| RoundBinding {
        round_id: ROUND_ID.to_string(),
        network: Network::Testnet,
        proposals,
        hotkey_secret: None,
    };
    let unbound = || {
        let (executor, _) = bound_executor_unbound(host_database());
        executor
    };

    let error = unbound()
        .with_binding(binding(Vec::new()))
        .err()
        .expect("an empty roster must be rejected");
    assert!(matches!(error, VotingError::InvalidInput { .. }), "{error}");
    assert!(error.to_string().contains("nonempty"), "{error}");

    let error = unbound()
        .with_binding(binding(vec![
            ProposalRosterEntry {
                proposal_id: 4,
                num_options: 2,
            },
            ProposalRosterEntry {
                proposal_id: 4,
                num_options: 3,
            },
        ]))
        .err()
        .expect("a repeated proposal must be rejected");
    assert!(matches!(error, VotingError::InvalidInput { .. }), "{error}");
    assert!(error.to_string().contains("proposal 4"), "{error}");

    assert!(unbound()
        .with_binding(binding(vec![ProposalRosterEntry {
            proposal_id: 4,
            num_options: 2,
        }]))
        .is_ok());
}

#[test]
fn a_binding_for_another_network_is_refused() {
    let (executor, _) = bound_executor_unbound(host_database());
    let error = executor
        .with_binding(RoundBinding {
            round_id: ROUND_ID.to_string(),
            network: Network::Mainnet,
            proposals: vec![ProposalRosterEntry {
                proposal_id: 1,
                num_options: 2,
            }],
            hotkey_secret: None,
        })
        .err()
        .expect("the chain client is configured for Testnet");
    assert!(matches!(error, VotingError::InvalidInput { .. }), "{error}");
    assert!(error.to_string().contains("Mainnet"), "{error}");
}

#[test]
fn a_binding_for_a_network_other_than_the_stored_round_is_refused() {
    // The fixture round is stored for Testnet; the host binds Mainnet with
    // a Mainnet chain client, which the chain-network check alone accepts.
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

    let error = executor
        .with_binding(RoundBinding {
            round_id: ROUND_ID.to_string(),
            network: Network::Mainnet,
            proposals: vec![ProposalRosterEntry {
                proposal_id: 1,
                num_options: 2,
            }],
            hotkey_secret: None,
        })
        .err()
        .expect("the stored round is Testnet");
    assert!(matches!(error, VotingError::InvalidInput { .. }), "{error}");
    assert!(
        error.to_string().contains("stored for network Testnet"),
        "{error}"
    );
}

#[test]
fn a_binding_with_a_malformed_hotkey_secret_is_refused() {
    let (executor, _) = bound_executor_unbound(host_database());
    let error = executor
        .with_binding(RoundBinding {
            round_id: ROUND_ID.to_string(),
            network: Network::Testnet,
            proposals: vec![ProposalRosterEntry {
                proposal_id: 1,
                num_options: 2,
            }],
            hotkey_secret: Some(zeroize::Zeroizing::new(vec![0x21; 63])),
        })
        .err()
        .expect("a 63-byte hotkey secret cannot reconstruct a hotkey");
    assert!(matches!(error, VotingError::InvalidInput { .. }), "{error}");
}

#[test]
fn a_binding_with_an_unsupported_option_count_is_refused() {
    let (executor, _) = bound_executor_unbound(host_database());
    let error = executor
        .with_binding(RoundBinding {
            round_id: ROUND_ID.to_string(),
            network: Network::Testnet,
            proposals: vec![
                ProposalRosterEntry {
                    proposal_id: 1,
                    num_options: 2,
                },
                ProposalRosterEntry {
                    proposal_id: 2,
                    num_options: 1,
                },
            ],
            hotkey_secret: None,
        })
        .err()
        .expect("a one-option proposal cannot be voted on");
    assert!(matches!(error, VotingError::InvalidInput { .. }), "{error}");
}
