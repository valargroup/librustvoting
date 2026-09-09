//! What a ballot has to be before a round is spent on it.
//!
//! Every case here is something the chain or the SDK would otherwise reject
//! after provisioning, when the cost is a wasted round and a run that reads
//! like a defect.

use stage_bench::ballot::{Ballot, MAX_OPTIONS, MAX_PROPOSAL_ID, MIN_OPTIONS};
use zcash_voting::session::Decision;

const EXPORT: &[u8] = include_bytes!("fixtures/round-export.json");

#[test]
fn a_synthetic_ballot_cycles_its_option_widths() {
    let ballot = Ballot::synthetic(7, &[2, 3, 4]).expect("a seven-proposal ballot");

    let widths: Vec<usize> = ballot
        .proposals()
        .iter()
        .map(|proposal| proposal.options.len())
        .collect();
    assert_eq!(widths, vec![2, 3, 4, 2, 3, 4, 2]);

    // One-based and contiguous, as the chain requires.
    assert_eq!(ballot.proposal_ids(), (1..=7).collect::<Vec<u32>>());
}

/// The regression this benchmark exists to avoid repeating.
///
/// The conformance suite derives each proposal's choice from its *position*,
/// which stays in range only because its ballot is three proposals of widths
/// two, three and four. At thirty-seven proposals that choice exceeds every
/// proposal's `num_options`, and the failure lands after the round has been
/// provisioned and the delegation spent.
#[test]
fn every_choice_is_inside_its_own_proposal() {
    let ballot = Ballot::synthetic(37, &[2, 3, 4]).expect("a thirty-seven-proposal ballot");

    let intents = ballot.intents();
    assert_eq!(intents.len(), 37);
    for (intent, proposal) in intents.iter().zip(ballot.proposals()) {
        assert_eq!(intent.proposal_id, proposal.id);
        let Decision::Choice(choice) = intent.decision else {
            panic!("a benchmark ballot decides every proposal");
        };
        assert!(
            (choice as usize) < proposal.options.len(),
            "proposal {} has {} options but was voted choice {choice}",
            proposal.id,
            proposal.options.len()
        );
    }
}

#[test]
fn the_roster_matches_the_ballot_it_came_from() {
    let ballot = Ballot::synthetic(5, &[2, 8]).expect("a five-proposal ballot");

    let roster = ballot.roster();
    assert_eq!(roster.len(), 5);
    for (entry, proposal) in roster.iter().zip(ballot.proposals()) {
        assert_eq!(entry.proposal_id, proposal.id);
        assert_eq!(entry.num_options as usize, proposal.options.len());
    }
}

#[test]
fn a_ballot_wider_than_the_sdk_allows_is_refused() {
    let too_many = MAX_PROPOSAL_ID as usize + 1;
    assert!(Ballot::synthetic(too_many, &[2]).is_err());
    assert!(Ballot::synthetic(MAX_PROPOSAL_ID as usize, &[2]).is_ok());
    assert!(Ballot::synthetic(0, &[2]).is_err());
}

#[test]
fn an_out_of_range_option_width_is_refused() {
    assert!(Ballot::synthetic(3, &[MIN_OPTIONS - 1]).is_err());
    assert!(Ballot::synthetic(3, &[MAX_OPTIONS + 1]).is_err());
    assert!(Ballot::synthetic(3, &[]).is_err());
}

#[test]
fn a_round_export_becomes_the_same_shape_as_a_synthetic_ballot() {
    let ballot = Ballot::from_export(EXPORT).expect("the fixture export");

    assert_eq!(ballot.len(), 3);
    assert_eq!(ballot.proposal_ids(), vec![1, 2, 3]);
    assert_eq!(
        ballot
            .proposals()
            .iter()
            .map(|proposal| proposal.options.len())
            .collect::<Vec<_>>(),
        vec![2, 3, 4]
    );
    assert_eq!(ballot.proposals()[0].title, "Is 12 an even number?");
    assert_eq!(ballot.proposals()[2].options[0].label, "Africa");

    // Empty strings in the export are absences, not values: serializing one
    // into the round description would put a blank description on the chain.
    assert_eq!(ballot.proposals()[2].description, None);
    assert_eq!(ballot.proposals()[0].options[0].description, None);

    for (intent, proposal) in ballot.intents().iter().zip(ballot.proposals()) {
        let Decision::Choice(choice) = intent.decision else {
            panic!("a benchmark ballot decides every proposal");
        };
        assert!((choice as usize) < proposal.options.len());
    }
}

/// The export's own identifiers are kept for the manifest and never sent on.
///
/// The SDK identifies a proposal by a small integer. Carrying the UUID through
/// would be untranslatable; dropping it entirely would leave a replayed run
/// unable to say which real ballot it replayed.
#[test]
fn an_imported_ballot_records_where_each_proposal_came_from() {
    let ballot = Ballot::from_export(EXPORT).expect("the fixture export");

    let sources = ballot.sources();
    assert_eq!(sources.len(), 3);
    assert_eq!(sources[0].proposal_id, 1);
    assert_eq!(
        sources[0].export_id.as_deref(),
        Some("de881f3c-a745-47db-84d6-a1439885c960")
    );
    assert_eq!(sources[2].num_options, 4);
}

#[test]
fn an_empty_export_is_refused() {
    let empty = br#"{"round":{"proposals":[]}}"#;
    assert!(Ballot::from_export(empty).is_err());
}

#[test]
fn an_export_proposal_outside_the_option_bounds_is_refused() {
    let single = br#"{"round":{"proposals":[{"title":"one","options":[{"label":"only"}]}]}}"#;
    assert!(Ballot::from_export(single).is_err());
}
