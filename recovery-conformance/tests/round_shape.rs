//! The ballot shape and the round identity derived from it.

use recovery_conformance::provisioning::{
    suite_ballot, RoundDescription, SnapshotAnchor, EXPECTED_BUNDLE_COUNT, EXPECTED_VOTER_NOTES,
};

fn anchor() -> SnapshotAnchor {
    SnapshotAnchor {
        height: 3_428_150,
        blockhash_hex: "aa".repeat(32),
        nullifier_imt_root_hex: "bb".repeat(32),
        nc_root_hex: "cc".repeat(32),
    }
}

#[test]
fn the_ballot_satisfies_the_chains_own_bounds() {
    let ballot = suite_ballot();
    assert!(
        (1..=50).contains(&ballot.len()),
        "the chain accepts 1-50 proposals"
    );
    for proposal in &ballot {
        assert!(proposal.id >= 1, "proposal ids are one-based on chain");
        assert!(
            (2..=8).contains(&proposal.options.len()),
            "the chain accepts 2-8 options per proposal"
        );
        for (position, option) in proposal.options.iter().enumerate() {
            assert_eq!(
                option.index, position as u32,
                "option indexes are zero-based and dense"
            );
            assert!(option.label.is_ascii(), "the chain requires ASCII labels");
        }
    }
}

#[test]
fn the_ballot_varies_its_option_counts() {
    // `num_options` rides on each vote and bounds the recorded choice. A ballot
    // of uniform width could not catch a resume that rebuilt a vote against
    // another proposal's bounds, because every bound would be the same.
    let widths: Vec<usize> = suite_ballot()
        .iter()
        .map(|proposal| proposal.options.len())
        .collect();
    assert_eq!(widths, vec![2, 3, 4]);
}

#[test]
fn more_than_one_proposal_so_ordering_is_observable() {
    // "Vote work is proposal-primary" is not a testable claim with one
    // proposal: there is no order to get wrong.
    assert!(suite_ballot().len() > 1);
}

#[test]
fn the_same_ballot_hashes_the_same_way_twice() {
    // The hash feeds the Poseidon round-id derivation, so an unstable encoding
    // would give two runs asking for the same round two identities.
    let first = RoundDescription::new(&anchor(), 1_893_456_000, suite_ballot());
    let second = RoundDescription::new(&anchor(), 1_893_456_000, suite_ballot());
    assert_eq!(first.proposals_hash, second.proposals_hash);
    assert_eq!(first.proposals_hash.len(), 64, "SHA-256 as lowercase hex");
    assert!(first.proposals_hash.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn a_changed_ballot_cannot_reuse_a_round_identity() {
    let mut altered = suite_ballot();
    altered[0].options[1].label = "Nope".to_string();

    let original = RoundDescription::new(&anchor(), 1_893_456_000, suite_ballot());
    let changed = RoundDescription::new(&anchor(), 1_893_456_000, altered);
    assert_ne!(original.proposals_hash, changed.proposals_hash);
}

#[test]
fn the_description_serializes_to_the_fields_svoted_requires() {
    let json = RoundDescription::new(&anchor(), 1_893_456_000, suite_ballot())
        .to_json()
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

    for field in [
        "snapshot_height",
        "snapshot_blockhash",
        "proposals_hash",
        "vote_end_time",
        "nullifier_imt_root",
        "nc_root",
        "proposals",
    ] {
        assert!(
            parsed.get(field).is_some(),
            "missing required field {field}"
        );
    }

    // An absent optional must be omitted, not serialized as null: the CLI's
    // decoder treats the field as optional, not as nullable.
    let first_option = &parsed["proposals"][0]["options"][0];
    assert!(first_option.get("description").is_none());
}

#[test]
fn the_expected_wallet_layout_matches_the_bundling_rule() {
    // 11 notes at five slots per bundle fill 5/5/1. Recording both numbers
    // means a wallet rebalance that changes the layout fails here rather than
    // quietly weakening the multi-bundle invariants.
    // The "spare bundle" requirement itself is a compile-time assertion in
    // `provisioning`, so it cannot be weakened without failing the build.
    let slots = 5;
    assert_eq!(EXPECTED_VOTER_NOTES.div_ceil(slots), EXPECTED_BUNDLE_COUNT);
}
