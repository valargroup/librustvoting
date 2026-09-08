//! Vote-submission progress is exact for batches and counted against the
//! baseline selected by the host.

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
    // `Delegate` and read two selected votes as one.
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

/// Obligations naming `still_owed` as one atomic batch the round still owes,
/// for a ballot whose only durable choices are `choices`.
fn batch_obligations(
    choices: &[u32],
    still_owed: &[u32],
) -> crate::round_planning::RoundObligations {
    BatchObligations {
        choices,
        still_owed,
        ..BatchObligations::default()
    }
    .build()
}

/// The same round, described in full: the two sets that hold a choice no
/// obligation of this plan names.
#[derive(Default)]
struct BatchObligations<'a> {
    /// Rostered proposals the durable ballot recorded a choice for.
    choices: &'a [u32],
    /// The members of the batch obligation the round still owes.
    still_owed: &'a [u32],
    /// Choices whose cast the plan could not draw up: an open ballot, a held
    /// bundle, or a batch waiting on an undecided member withholds them.
    withheld: &'a [u32],
    /// Choices the roster no longer lists whose vote the chain lifecycle owns,
    /// so the host cannot clear them and their work outlives the roster change.
    lifecycle_owned: &'a [u32],
}

impl BatchObligations<'_> {
    fn build(self) -> crate::round_planning::RoundObligations {
        let obligations = if self.still_owed.is_empty() {
            Vec::new()
        } else {
            vec![crate::round_planning::Obligation::ReconcileChain {
                unit: crate::round_planning::VoteUnitId::Batch {
                    bundle_index: 0,
                    ordered_batch_digest: [7; 32],
                },
                bundle_index: 0,
                ordered_proposal_ids: self.still_owed.to_vec(),
                undispatched: false,
                tx_hash: None,
                prerequisite: None,
            }]
        };
        crate::round_planning::RoundObligations {
            obligations,
            choice_proposals: self.choices.to_vec(),
            open_proposals: Vec::new(),
            unrostered_intents: Vec::new(),
            stale_vote_keys: Default::default(),
            needs_bundle_setup: false,
            withheld_casts: self.withheld.iter().copied().collect(),
            lifecycle_owned_choices: self.lifecycle_owned.iter().copied().collect(),
        }
    }
}

#[test]
fn a_batch_counts_every_ordered_member_not_just_its_anchor() {
    // The batch projects to one `AdvanceVoteBatch` carrying proposal 1, so a
    // host counting steps reads three selected votes as one. The
    // tally reads the obligation's membership instead.
    let baseline = VoteProgressBaseline::for_run(&batch_obligations(&[1, 2, 3], &[1, 2, 3]));

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
    // describe all selected votes rather than this run, and would never reach
    // its total.
    let baseline = VoteProgressBaseline::for_run(&batch_obligations(&[1, 2, 3], &[3]));
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
        withheld_casts: Default::default(),
        lifecycle_owned_choices: Default::default(),
    };

    let tally = VoteProgressBaseline::for_run(&obligations).tally(&obligations);
    assert_eq!(
        tally.remaining_obligations, 0,
        "the driver has no entry point that executes a retire alone"
    );
    assert_eq!(tally.total_proposals, 0, "a retire owes no vote of its own");
}

#[test]
fn the_selected_choices_baseline_keeps_its_total_across_a_resume() {
    // The same resumed round as above: one of three proposals still owed. The
    // run baseline reports "1 of 1" for this run; the selected-choices
    // baseline keeps all three selected votes in view across the restart.
    let resumed = batch_obligations(&[1, 2, 3], &[3]);
    let baseline = VoteProgressBaseline::for_selected_choices(&resumed);

    let owed = baseline.tally(&resumed);
    assert_eq!((owed.completed_proposals, owed.total_proposals), (2, 3));

    let done = baseline.tally(&batch_obligations(&[1, 2, 3], &[]));
    assert_eq!((done.completed_proposals, done.total_proposals), (3, 3));
}

#[test]
fn the_selected_choices_baseline_counts_every_member_of_an_atomic_batch() {
    // Same exactness the run baseline has: membership comes from the
    // obligation, not from the single anchor id an `AdvanceVoteBatch` carries.
    let baseline =
        VoteProgressBaseline::for_selected_choices(&batch_obligations(&[1, 2, 3], &[1, 2, 3]));

    let owed = baseline.tally(&batch_obligations(&[1, 2, 3], &[1, 2, 3]));
    assert_eq!((owed.completed_proposals, owed.total_proposals), (0, 3));

    let landed = baseline.tally(&batch_obligations(&[1, 2, 3], &[]));
    assert_eq!((landed.completed_proposals, landed.total_proposals), (3, 3));
}

#[test]
fn a_withheld_cast_is_not_a_completed_selected_choice() {
    // Proposal 3 is chosen but its cast is withheld while the ballot is open,
    // so no obligation names it. Reading completion as "no obligation covers
    // it" would report completed submission for a choice that has not been
    // cast; proposal 1 has genuinely landed and still counts.
    let partly_cast = BatchObligations {
        choices: &[1, 2, 3],
        still_owed: &[2],
        withheld: &[3],
        ..BatchObligations::default()
    }
    .build();
    let baseline = VoteProgressBaseline::for_selected_choices(&partly_cast);

    let owed = baseline.tally(&partly_cast);
    assert_eq!((owed.completed_proposals, owed.total_proposals), (1, 3));

    // The voter resolves the ballot: the withheld cast becomes owed work, and
    // the count does not go backwards past what already landed.
    let unblocked = batch_obligations(&[1, 2, 3], &[2, 3]);
    let resolved = baseline.tally(&unblocked);
    assert_eq!(
        (resolved.completed_proposals, resolved.total_proposals),
        (1, 3)
    );

    let done = baseline.tally(&batch_obligations(&[1, 2, 3], &[]));
    assert_eq!((done.completed_proposals, done.total_proposals), (3, 3));
}

#[tokio::test]
async fn a_ballot_recorded_before_bundle_setup_completes_nothing() {
    // Choices persisted first is the supported ordering, and it plans no vote
    // work at all: the run stops with `NeedsBundleSetup`. The selected-choice
    // total is two, and neither submission is complete.
    let executor = executor_over(database_without_bundles());
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
    let events = RecordingReporter::default();
    let report = RoundDriver::new(&executor)
        .with_policy(RoundDrivePolicy {
            progress_baseline: ProgressBaseline::SelectedChoices,
            ..RoundDrivePolicy::default()
        })
        .run(&FixedHost, &control, &events)
        .await;

    assert!(matches!(
        report.quiescence,
        RoundQuiescence::NeedsBundleSetup
    ));
    assert_eq!(report.tally.total_proposals, 2);
    assert_eq!(
        report.tally.completed_proposals, 0,
        "no bundle exists to have cast either choice into"
    );
}

#[tokio::test]
async fn a_skipped_proposal_is_not_a_selected_choice() {
    // `choice_proposals` excludes skips, so only the selected vote contributes
    // to the total. A skip owes no vote submission.
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
    let events = RecordingReporter::default();
    let report = RoundDriver::new(&executor)
        .with_policy(RoundDrivePolicy {
            progress_baseline: ProgressBaseline::SelectedChoices,
            ..RoundDrivePolicy::default()
        })
        .run(&FixedHost, &control, &events)
        .await;

    assert_eq!(report.tally.total_proposals, 1);
}

#[tokio::test]
async fn selecting_a_baseline_does_not_disturb_a_round_both_agree_on() {
    // Both baselines are the same set while nothing has completed yet, so the
    // policy is observable only on a resume (see the two tests above). This
    // pins the wiring: selecting `SelectedChoices` runs the driver to the same
    // tally rather than, say, capturing an empty baseline.
    for baseline in [ProgressBaseline::Run, ProgressBaseline::SelectedChoices] {
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
        let events = RecordingReporter::default();
        let report = RoundDriver::new(&executor)
            .with_policy(RoundDrivePolicy {
                progress_baseline: baseline,
                ..RoundDrivePolicy::default()
            })
            .run(&FixedHost, &control, &events)
            .await;

        assert_eq!(report.tally.total_proposals, 2, "baseline {baseline:?}");
        assert_eq!(report.tally.completed_proposals, 0, "baseline {baseline:?}");
    }
}

#[test]
fn the_default_baseline_is_the_run_so_existing_hosts_are_unchanged() {
    assert_eq!(
        RoundDrivePolicy::default().progress_baseline,
        ProgressBaseline::Run
    );
}

#[test]
fn a_decided_member_of_a_held_batch_is_not_a_completed_selected_choice() {
    // The ballot decided proposal 1; its committed batch also holds proposal
    // 2, which the voter has not reached, so the batch cannot be dispatched
    // and owns no obligation. Reading completion as "no obligation covers it"
    // would report completed submission before anything was sent, then take
    // the count back when deciding proposal 2 produces the `ReconcileChain`.
    let held = BatchObligations {
        choices: &[1],
        withheld: &[1],
        ..BatchObligations::default()
    }
    .build();
    let baseline = VoteProgressBaseline::for_selected_choices(&held);

    let waiting = baseline.tally(&held);
    assert_eq!(
        (waiting.completed_proposals, waiting.total_proposals),
        (0, 1),
        "nothing has been dispatched for the decided member"
    );

    // The voter decides the rest of the batch: the same question is now owed
    // through an obligation, and the count has not moved backwards.
    let agreed = baseline.tally(&batch_obligations(&[1, 2], &[1, 2]));
    assert_eq!((agreed.completed_proposals, agreed.total_proposals), (0, 1));

    let landed = baseline.tally(&batch_obligations(&[1, 2], &[]));
    assert_eq!((landed.completed_proposals, landed.total_proposals), (1, 1));
}

#[test]
fn the_selected_choices_baseline_holds_a_choice_the_chain_lifecycle_owns() {
    // Proposal 1 left the roster after its vote reached the chain, so it is in
    // neither `choice_proposals` nor the clearable `unrostered_intents`. Its
    // work is still owed and its intent still stands, so the selected-choice
    // total must keep it: dropping it would move the denominator this baseline
    // exists to hold still and hide a vote on the wire.
    let unrostered = BatchObligations {
        choices: &[2, 3],
        still_owed: &[1],
        lifecycle_owned: &[1],
        ..BatchObligations::default()
    }
    .build();
    let baseline = VoteProgressBaseline::for_selected_choices(&unrostered);

    let owed = baseline.tally(&unrostered);
    assert_eq!(
        (owed.completed_proposals, owed.total_proposals),
        (2, 3),
        "the vote the lifecycle owns is still owed"
    );

    // The vote confirms. Its shares are tracked elsewhere, so no vote
    // obligation covers it any more and its vote submission reads complete.
    let confirmed = BatchObligations {
        choices: &[2, 3],
        lifecycle_owned: &[1],
        ..BatchObligations::default()
    }
    .build();
    let done = baseline.tally(&confirmed);
    assert_eq!((done.completed_proposals, done.total_proposals), (3, 3));

    // The same total whichever run captures it: the roster change does not
    // renumber the selected-vote submissions a host is labelling.
    assert_eq!(
        VoteProgressBaseline::for_selected_choices(&confirmed)
            .tally(&confirmed)
            .total_proposals,
        3
    );
}
