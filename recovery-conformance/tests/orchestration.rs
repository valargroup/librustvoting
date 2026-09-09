//! Hermetic tests for the orchestration logic the live matrix depends on.
//!
//! These do not mock the conformance run itself — a mocked crash would prove
//! nothing about recovery. They cover the parts that decide *how* the matrix
//! judges a run: how a configuration survives the trip to a child process, and
//! how an outcome is classified as conformance, environment, or neither. A
//! mistake in either turns a real finding into a silent pass.

use std::path::PathBuf;

use recovery_conformance::assertions::{
    assert_matches_control, assert_no_second_generation, assert_reservations_monotonic,
    assert_terminal_rows_unchanged, assert_untouched_bundles_did_not_reserve, DurableSnapshot,
    Submission,
};
use recovery_conformance::run_config::{
    Endpoints, FailureRecord, RoundRunConfig, RunMode, RunOutcome, Target,
};
use recovery_conformance::CrashStage;

fn config(mode: RunMode) -> RoundRunConfig {
    RoundRunConfig {
        sidecar: PathBuf::from("/tmp/sidecar.db"),
        wallet_db: PathBuf::from("/tmp/wallet.db"),
        warm_pir_from: Some(PathBuf::from("/tmp/template.db")),
        round_id: "abc123".to_string(),
        account_uuid: "8b29d4e6-7940-4570-b2c2-3c7a25ba6922".to_string(),
        endpoints: Endpoints {
            chain_rpc: "https://rpc.example".to_string(),
            vote_servers: vec![
                "https://a.example".to_string(),
                "https://b.example".to_string(),
            ],
            pir_urls: vec!["https://pir.example".to_string()],
            helper_urls: vec!["https://a.example".to_string()],
            lightwalletd: "https://lwd.example:443".to_string(),
        },
        target: Target {
            bundle_index: 0,
            proposal_id: 1,
        },
        mode,
        crash_log: PathBuf::from("/tmp/crash.jsonl"),
        outcome: PathBuf::from("/tmp/outcome.json"),
        max_dispatches: 512,
        stall: Default::default(),
        fleet: Default::default(),
        vote_end_time_seconds: 2_000_000_000,
    }
}

/// `(kind, bundle_index, generation_digest, state, reservations)`.
fn snapshot(submissions: Vec<(&str, i64, &str, &str, i64)>) -> DurableSnapshot {
    DurableSnapshot {
        submissions: submissions
            .into_iter()
            .map(
                |(kind, bundle_index, generation_digest, state, reservations)| Submission {
                    kind: kind.to_string(),
                    bundle_index,
                    proposal_id: None,
                    generation_digest: generation_digest.to_string(),
                    state: state.to_string(),
                    has_candidate_hash: state == "tracking",
                    has_confirmed_hash: state == "confirmed",
                    confirmation_source: (state == "confirmed").then(|| "hash".to_string()),
                    reservations,
                },
            )
            .collect(),
        bundles: 3,
        proofs: 1,
        votes: 0,
        helper_share_plans: 0,
        share_delegations: 0,
        attempting_urls: 0,
        accepted_urls: 0,
        confirmed_shares: 0,
        pczt_persisted: true,
        cached_tree: false,
        deliveries: Vec::new(),
    }
}

#[test]
fn a_configuration_survives_the_trip_to_a_child_process() {
    // The child is a separate process, so every decision the parent made has to
    // cross a file boundary intact. A field lost here retargets the run
    // silently, which is what the old positional argument list did wrong.
    let original = config(RunMode::Armed {
        stage: CrashStage::AfterProof,
    });
    let path = std::env::temp_dir().join(format!("rc-config-{}.json", std::process::id()));
    original.write(&path).unwrap();
    let parsed = RoundRunConfig::read(&path).unwrap();

    assert_eq!(parsed.round_id, original.round_id);
    assert_eq!(parsed.armed_stage(), Some(CrashStage::AfterProof));
    assert_eq!(parsed.target.bundle_index, 0);
    // Endpoint order is behaviour: the submission lifecycle cycles endpoints by
    // reservation ordinal, so a reordering changes which one a retry lands on.
    assert_eq!(
        parsed.endpoints.vote_servers,
        original.endpoints.vote_servers
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn the_configuration_carries_no_secrets() {
    // The seed reaches the child through the environment it inherits. Anything
    // serialized here is written to disk and readable afterwards.
    let serialized = serde_json::to_string(&config(RunMode::Unarmed)).unwrap();
    for forbidden in ["mnemonic", "seed", "VOTE_SDK_VOTER_TEST", "VOTE_MANAGER"] {
        assert!(
            !serialized.contains(forbidden),
            "the run configuration mentions {forbidden}"
        );
    }
}

#[test]
fn an_unarmed_configuration_reports_no_stage() {
    assert_eq!(config(RunMode::Unarmed).armed_stage(), None);
}

#[test]
fn only_transport_failures_are_blamed_on_the_environment() {
    // Getting this wrong in either direction is costly: treating a conformance
    // failure as environmental retries past a real finding, and treating an
    // environmental one as conformance blames the SDK for a stalled endpoint.
    let transport = FailureRecord {
        step: None,
        bundle_index: Some(0),
        kind: "Transport".to_string(),
        message: "PIR unavailable".to_string(),
    };
    let invariant = FailureRecord {
        step: None,
        bundle_index: Some(0),
        kind: "InvariantViolation".to_string(),
        message: "step resolves to no obligation".to_string(),
    };
    assert!(transport.is_environmental());
    assert!(!invariant.is_environmental());
}

#[test]
fn a_run_mixing_a_real_failure_with_a_stall_is_not_environmental() {
    // One conformance failure is enough to make the whole run a finding, even
    // when a stalled endpoint failed alongside it.
    let outcome = RunOutcome {
        quiescence: "Failures".to_string(),
        quiescence_kind: "Failures".to_string(),
        failures: vec![
            FailureRecord {
                step: None,
                bundle_index: Some(0),
                kind: "Transport".to_string(),
                message: "stalled".to_string(),
            },
            FailureRecord {
                step: None,
                bundle_index: Some(1),
                kind: "InvariantViolation".to_string(),
                message: "listed step resolved to nothing".to_string(),
            },
        ],
        dispatches: 0,
        share_tracking: Vec::new(),
    };
    assert!(!outcome.is_environmental());
}

#[test]
fn only_a_finished_round_counts_as_terminal_success() {
    let ended = |kind: &str| RunOutcome {
        quiescence: kind.to_string(),
        quiescence_kind: kind.to_string(),
        failures: Vec::new(),
        dispatches: 0,
        share_tracking: Vec::new(),
    };
    assert!(ended("NoWorkLeft").is_terminal_success());
    // Background share work is the timer's to finish; the foreground round is
    // done.
    assert!(ended("BackgroundShareWorkOnly").is_terminal_success());
    for unfinished in [
        "NeedsBallot",
        "NeedsDelegationSignatures",
        "ChainTerminal",
        "ChainRecoveryStalled",
        "Failures",
        "PassBudgetExhausted",
        "Cancelled",
    ] {
        assert!(
            !ended(unfinished).is_terminal_success(),
            "{unfinished} must not read as a finished round"
        );
    }
}

#[test]
fn reservations_may_grow_but_never_shrink() {
    let before = snapshot(vec![("delegation", 0, "AA", "submitting", 1)]);
    let grown = snapshot(vec![("delegation", 0, "AA", "tracking", 2)]);
    assert!(assert_reservations_monotonic(&before, &grown).is_ok());

    let shrunk = snapshot(vec![("delegation", 0, "AA", "submitting", 0)]);
    assert!(
        assert_reservations_monotonic(&before, &shrunk).is_err(),
        "a decreasing reservation count hides a redispatch"
    );
}

#[test]
fn a_terminal_row_may_not_change_across_a_resume() {
    let before = snapshot(vec![("delegation", 0, "AA", "confirmed", 1)]);
    assert!(assert_terminal_rows_unchanged(&before, &before).is_ok());

    let altered = snapshot(vec![("delegation", 0, "AA", "recovering", 1)]);
    assert!(
        assert_terminal_rows_unchanged(&before, &altered).is_err(),
        "a confirmed row that moved back to recovering is an immutability breach"
    );
}

#[test]
fn a_non_terminal_row_is_free_to_advance() {
    // Only terminal rows are immutable; `submitting` becoming `confirmed` is
    // the ordinary path and must not be flagged.
    let before = snapshot(vec![("delegation", 0, "AA", "submitting", 1)]);
    let after = snapshot(vec![("delegation", 0, "AA", "confirmed", 1)]);
    assert!(assert_terminal_rows_unchanged(&before, &after).is_ok());
}

#[test]
fn total_reservations_sums_every_submission() {
    let snapshot = snapshot(vec![
        ("delegation", 0, "AA", "confirmed", 2),
        ("vote", 0, "BB", "tracking", 3),
    ]);
    assert_eq!(snapshot.total_reservations(), 5);
    assert_eq!(snapshot.states(), vec!["confirmed", "tracking"]);
}

/// A `RoundPlan` cannot be constructed outside the SDK, so idempotence is
/// covered here through the live matrix rather than a fixture. What is
/// hermetically checkable is the classification that feeds it.
#[test]
fn background_share_work_is_a_finished_round_not_a_stalled_one() {
    // The distinction the matrix got wrong: a round ending in
    // `BackgroundShareWorkOnly` has finished everything the foreground owns,
    // and the `ConfirmShare` steps it leaves are the host's timer's to close.
    let background = RunOutcome {
        quiescence: "BackgroundShareWorkOnly".to_string(),
        quiescence_kind: "BackgroundShareWorkOnly".to_string(),
        failures: Vec::new(),
        dispatches: 0,
        share_tracking: Vec::new(),
    };
    assert!(background.is_terminal_success());
    assert!(!background.is_environmental());
}

#[test]
fn a_stalled_chain_recovery_is_not_a_finished_round() {
    // It is also not a failure: the specification separates it from
    // `ChainTerminal` because running again later may resolve it. The matrix
    // waits and re-drives rather than reporting a verdict.
    let stalled = RunOutcome {
        quiescence: "ChainRecoveryStalled { .. }".to_string(),
        quiescence_kind: "ChainRecoveryStalled".to_string(),
        failures: Vec::new(),
        dispatches: 0,
        share_tracking: Vec::new(),
    };
    assert!(!stalled.is_terminal_success());
    // No failures recorded, so it must not be mistaken for an environment
    // problem either — it is a "come back later".
    assert!(!stalled.is_environmental());
}

#[test]
fn a_stale_vote_tree_cache_is_repaired_by_the_sdk_not_a_finding() {
    // The tree sync discards the cached tree when it disagrees with a confirmed
    // delegation, so the next pass re-syncs. Reporting the first occurrence as
    // a conformance failure would blame the SDK for its own repair.
    let stale = FailureRecord {
        step: Some("CastVote { bundle_index: 1, .. }".to_string()),
        bundle_index: Some(1),
        kind: "InvalidInput".to_string(),
        message: "Invalid input: confirmed delegation bundle 0 does not match its synced \
                  vote-tree leaf"
            .to_string(),
    };
    assert!(stale.is_self_healing());
    assert!(!stale.is_environmental());

    // The rule must stay narrow: another InvalidInput is a real finding.
    let real = FailureRecord {
        step: None,
        bundle_index: Some(0),
        kind: "InvalidInput".to_string(),
        message: "Invalid input: refusing to regress round phase".to_string(),
    };
    assert!(!real.is_self_healing());
}

#[test]
fn a_tree_confirmation_carries_no_hash_and_must_not_be_asked_for_one() {
    use recovery_conformance::assertions::assert_recovered_the_same_transaction;
    // The schema enforces this: `confirmation_source != 'tree' OR
    // confirmed_transaction_hash IS NULL`. An assertion that demanded a hash
    // here would be asserting something the database forbids.
    assert!(assert_recovered_the_same_transaction("abc123", None, Some("tree")).is_ok());

    // A hash confirmation must match, and a mismatch means a second
    // transaction was sent.
    assert!(assert_recovered_the_same_transaction("abc123", Some("abc123"), Some("hash")).is_ok());
    assert!(assert_recovered_the_same_transaction("abc123", Some("def456"), Some("hash")).is_err());

    // No confirmation at all leaves the dispatched transaction unresolved.
    assert!(assert_recovered_the_same_transaction("abc123", None, None).is_err());
}

/// A second generation for work that already had one is a second transaction.
///
/// This is the failure the reservation count cannot see. A duplicate POST
/// raises the count exactly as a legitimate retry does, so counting alone
/// cannot separate "tried again" from "built another transaction spending the
/// same notes". The digest can, and that distinction is the whole no-double-
/// spend claim.
#[test]
fn a_second_generation_for_the_same_target_is_refused() {
    let before = snapshot(vec![("delegation", 0, "AA", "recovering", 1)]);

    let retried = snapshot(vec![("delegation", 0, "AA", "tracking", 2)]);
    assert!(
        assert_no_second_generation(&before, &retried).is_ok(),
        "a same-generation retry is permitted; only a new generation is not"
    );

    let rebuilt = snapshot(vec![
        ("delegation", 0, "AA", "recovering", 1),
        ("delegation", 0, "CC", "tracking", 1),
    ]);
    let error = assert_no_second_generation(&before, &rebuilt)
        .expect_err("a second generation for one bundle must be refused");
    assert!(format!("{error}").contains("second transaction"), "{error}");
}

/// A bundle the crash never touched must not reserve another POST.
#[test]
fn an_untouched_bundle_that_reserves_again_is_refused() {
    let before = snapshot(vec![
        ("delegation", 0, "AA", "submitting", 1),
        ("delegation", 1, "BB", "confirmed", 1),
    ]);
    // Bundle 0 crashed and may reserve again; bundle 1 was idle and may not.
    let after = snapshot(vec![
        ("delegation", 0, "AA", "confirmed", 2),
        ("delegation", 1, "BB", "confirmed", 1),
    ]);
    assert!(assert_untouched_bundles_did_not_reserve(&before, &after, 0).is_ok());

    let meddled = snapshot(vec![
        ("delegation", 0, "AA", "confirmed", 2),
        ("delegation", 1, "BB", "confirmed", 2),
    ]);
    let error = assert_untouched_bundles_did_not_reserve(&before, &meddled, 0)
        .expect_err("an idle bundle reserving again is a POST nothing asked for");
    assert!(format!("{error}").contains("uncrashed bundle 1"), "{error}");
}

/// The control comparison must see more than submission state names.
///
/// Comparing states alone accepts a round that reached the right state names
/// while losing the votes, plans and share rows underneath them.
#[test]
fn the_control_comparison_notices_a_round_that_lost_its_votes() {
    let control = snapshot(vec![("vote", 0, "AA", "confirmed", 1)]);
    assert!(assert_matches_control(&control, &control).is_ok());

    let mut hollow = control.clone();
    hollow.votes = 0;
    let mut full = control.clone();
    full.votes = 9;
    let error = assert_matches_control(&hollow, &full)
        .expect_err("identical state names must not hide a round with no votes");
    assert!(format!("{error}").contains("A3 VIOLATED"), "{error}");
}
