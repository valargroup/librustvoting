//! Ballot progress is exact for batches and run-relative.

use super::fixtures::*;

#[tokio::test]
async fn the_tally_counts_every_chosen_proposal_the_run_starts_owing() {
    let executor = executor();
    executor
        .set_ballot_intents(&[
            BallotIntent {
                proposal_id: 1,
                decision: Decision::Choice(0),
            },
            BallotIntent {
                proposal_id: 2,
                decision: Decision::Choice(1),
            },
        ])
        .unwrap();
    let control = ChainSubmissionControl::new(1);
    let (report, _) = drive(&executor, &control).await;

    // The run stops for signing material without casting anything, so both
    // chosen proposals are still owed. A host counting steps would see one
    // `Delegate` and read the ballot as one question.
    assert_eq!(report.tally.total_proposals, 2);
    assert_eq!(report.tally.completed_proposals, 0);
}

#[tokio::test]
async fn a_skipped_proposal_is_not_a_question_to_complete() {
    let executor = executor();
    executor
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
    let control = ChainSubmissionControl::new(1);
    let (report, _) = drive(&executor, &control).await;

    assert_eq!(report.tally.total_proposals, 1);
}

#[tokio::test]
async fn a_round_with_nothing_chosen_owes_no_questions() {
    let executor = executor();
    let control = ChainSubmissionControl::new(1);
    let (report, _) = drive(&executor, &control).await;

    assert_eq!(report.tally.total_proposals, 0);
    assert_eq!(report.tally.completed_proposals, 0);
    assert_eq!(report.tally.remaining_obligations, 0);
}

/// Obligations naming `proposals` as one atomic batch still owed.
fn batch_obligations(
    choices: &[u32],
    still_owed: &[u32],
) -> crate::round_planning::RoundObligations {
    let obligations = if still_owed.is_empty() {
        Vec::new()
    } else {
        vec![crate::round_planning::Obligation::ReconcileChain {
            unit: crate::round_planning::VoteUnitId::Batch {
                bundle_index: 0,
                ordered_batch_digest: [7; 32],
            },
            bundle_index: 0,
            ordered_proposal_ids: still_owed.to_vec(),
            undispatched: false,
            tx_hash: None,
            prerequisite: None,
        }]
    };
    crate::round_planning::RoundObligations {
        obligations,
        choice_proposals: choices.to_vec(),
        open_proposals: Vec::new(),
        unrostered_intents: Vec::new(),
        stale_vote_keys: Default::default(),
        needs_bundle_setup: false,
    }
}

#[test]
fn a_batch_counts_every_ordered_member_not_just_its_anchor() {
    // The batch projects to one `AdvanceVoteBatch` carrying proposal 1, so a
    // host counting steps reads a three-proposal ballot as one question. The
    // tally reads the obligation's membership instead.
    let baseline = BallotBaseline::capture(&batch_obligations(&[1, 2, 3], &[1, 2, 3]));

    let owed = baseline.tally(&batch_obligations(&[1, 2, 3], &[1, 2, 3]));
    assert_eq!(owed.total_proposals, 3);
    assert_eq!(owed.completed_proposals, 0);

    let landed = baseline.tally(&batch_obligations(&[1, 2, 3], &[]));
    assert_eq!(
        landed.completed_proposals, 3,
        "a batch lands whole, so all three complete together"
    );
    assert_eq!(landed.total_proposals, 3);
    assert_eq!(landed.remaining_obligations, 0);
}

#[test]
fn progress_is_measured_against_what_the_run_started_owing() {
    // A resumed round owes one of three proposals. Reporting "1 of 3" would
    // describe the ballot rather than this run, and would never reach its
    // total.
    let baseline = BallotBaseline::capture(&batch_obligations(&[1, 2, 3], &[3]));
    assert_eq!(
        baseline
            .tally(&batch_obligations(&[1, 2, 3], &[3]))
            .total_proposals,
        1
    );

    let done = baseline.tally(&batch_obligations(&[1, 2, 3], &[]));
    assert_eq!((done.completed_proposals, done.total_proposals), (1, 1));
}

#[test]
fn a_retire_is_not_work_the_tally_reports_as_owed() {
    // A retire is carried out by the `Cast` that replaces its unit, so it is
    // never dispatched on its own. Counting it would double count that work,
    // and for a round whose retire has no surviving cast it would report work
    // owed beside a `NoWorkLeft` quiescence — a state no host can act on.
    let obligations = crate::round_planning::RoundObligations {
        obligations: vec![crate::round_planning::Obligation::Retire {
            unit: crate::round_planning::VoteUnitId::Singleton {
                bundle_index: 0,
                proposal_id: 1,
            },
            members: vec![1],
        }],
        choice_proposals: Vec::new(),
        open_proposals: Vec::new(),
        unrostered_intents: Vec::new(),
        stale_vote_keys: Default::default(),
        needs_bundle_setup: false,
    };

    let tally = BallotBaseline::capture(&obligations).tally(&obligations);
    assert_eq!(
        tally.remaining_obligations, 0,
        "the driver has no entry point that executes a retire alone"
    );
    assert_eq!(tally.total_proposals, 0, "a retire owes no vote of its own");
}
