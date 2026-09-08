//! Full executor coverage uses real ZKP2 proofs, so it runs in `make proofs`.

mod fixtures;

use super::fixtures::{advance_plan_head, ROUND_ID};
use crate::*;
use fixtures::*;
use std::sync::atomic::Ordering;

async fn prepare_early(
    executor: &RoundExecutor<std::sync::Arc<Peers>>,
    host: &RoundHostContext,
    control: &ChainSubmissionControl,
) {
    executor
        .database()
        .conn()
        .execute("DELETE FROM ballot_intent WHERE proposal_id=2", [])
        .unwrap();
    assert!(!executor.plan().unwrap().open_proposals.is_empty());
    let outcome = advance_plan_head(executor, host, control, &NoopRoundStepProgressReporter {})
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome.step,
        Some(session::NextStep::Delegate { bundle_index: 0 })
    );
    assert!(outcome.delegation.is_none());
    assert_eq!(
        executor.database().delegation_phase(ROUND_ID, 0).unwrap(),
        phases::DelegationPhase::Proved
    );
    assert!(!executor
        .plan()
        .unwrap()
        .next_steps
        .iter()
        .any(|step| matches!(step, session::NextStep::CastVote { .. })));
    executor
        .set_ballot_intents(&[BallotIntent {
            proposal_id: 2,
            decision: session::Decision::Choice(1),
        }])
        .unwrap();
}

fn assert_completed(executor: &RoundExecutor<std::sync::Arc<Peers>>, peers: &Peers) {
    assert_eq!(peers.posts.lock().unwrap().len(), 1);
    assert_eq!(peers.deliveries.load(Ordering::SeqCst), 2);
    let db = executor.database();
    assert_eq!(
        db.delegation_phase(ROUND_ID, 0).unwrap(),
        phases::DelegationPhase::Confirmed
    );
    for proposal in [1, 2] {
        assert_eq!(
            db.vote_phase(ROUND_ID, 0, proposal).unwrap(),
            phases::VotePhase::Confirmed
        );
        let recovery = vote::recovery_bundle(&db, ROUND_ID, 0, proposal)
            .unwrap()
            .unwrap();
        assert_eq!(recovery.vc_tree_position, 7 + u64::from(proposal));
    }
    let rows: u32 = db
        .conn()
        .query_row("SELECT count(*) FROM chain_submissions", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(rows, 1, "delegation and votes share one lifecycle");
}

#[tokio::test]
#[ignore = "generates real ZKP2 proofs; run make proofs"]
async fn combined_executor_zkp2_prepares_submits_confirms_and_delivers() {
    let (db, driver) = database();
    let peers = Peers::new(db.clone());
    let executor = executor(db, peers.clone(), true);
    let host = context(driver.clone());
    let control = ChainSubmissionControl::new(1);
    prepare_early(&executor, &host, &control).await;
    assert_eq!(driver.prepared.load(Ordering::SeqCst), 1);
    assert_eq!(driver.signed.load(Ordering::SeqCst), 0);
    assert!(peers.posts.lock().unwrap().is_empty());
    let outcome = advance_plan_head(
        &executor,
        &host,
        &control,
        &NoopRoundStepProgressReporter {},
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(outcome.disposition, RoundStepDisposition::Advanced);
    assert_eq!(driver.signed.load(Ordering::SeqCst), 1);
    assert_completed(&executor, &peers);
}

struct CancelAfterPersistence(ChainSubmissionControl);
impl RoundStepProgressReporter for CancelAfterPersistence {
    fn report(&self, progress: RoundStepProgress) {
        if matches!(
            progress,
            RoundStepProgress::DelegateAndVoteBatchPersisted { .. }
        ) {
            self.0.cancel();
        }
    }
}

#[tokio::test]
async fn combined_authorization_refuses_a_wallet_switch_with_duplicate_setup() {
    let (db, driver) = database();
    let peers = Peers::new(db.clone());
    let executor = executor(db.clone(), peers, true);
    prepare_early(
        &executor,
        &context(driver.clone()),
        &ChainSubmissionControl::new(1),
    )
    .await;
    let authorization = crate::delegate_and_vote_batch::DelegationAuthorization::capture(
        &db.conn(),
        "wallet",
        ROUND_ID,
        0,
        driver.signature,
    )
    .unwrap();
    {
        let conn = db.conn();
        // Import exactly the same setup under a second wallet identifier.
        for table in ["rounds", "bundles", "proofs"] {
            let mut columns = conn
                .prepare(&format!("PRAGMA table_info({table})"))
                .unwrap();
            let projection = columns
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .map(|column| {
                    let column = column.unwrap();
                    if column == "wallet_id" {
                        "'other-wallet'".to_owned()
                    } else {
                        column
                    }
                })
                .collect::<Vec<_>>()
                .join(",");
            conn.execute(
                &format!(
                    "INSERT INTO {table} SELECT {projection} FROM {table} WHERE wallet_id='wallet'"
                ),
                [],
            )
            .unwrap();
        }
    }
    db.set_wallet_id("other-wallet");
    let other = crate::delegate_and_vote_batch::DelegationAuthorization::capture(
        &db.conn(),
        &db.wallet_id(),
        ROUND_ID,
        0,
        driver.signature,
    )
    .unwrap();
    assert_eq!(authorization.submission, other.submission);
    assert_eq!(authorization.van, other.van);
    authorization.validate_fresh(&db.conn()).unwrap();
    assert!(authorization
        .validate_scope(&db.wallet_id(), ROUND_ID, 0)
        .is_err());
    other.validate_scope(&db.wallet_id(), ROUND_ID, 0).unwrap();
}

#[tokio::test]
async fn combined_executor_prepares_early_without_signing_or_dispatch() {
    let (db, driver) = database();
    let peers = Peers::new(db.clone());
    let executor = executor(db, peers.clone(), true);
    let host = context(driver.clone());
    prepare_early(&executor, &host, &ChainSubmissionControl::new(1)).await;
    assert_eq!(driver.prepared.load(Ordering::SeqCst), 1);
    assert_eq!(driver.signed.load(Ordering::SeqCst), 0);
    assert!(peers.posts.lock().unwrap().is_empty());
    // Check the fixture's complete delegation recovery before expensive proofs.
    let db = executor.database();
    crate::delegate_and_vote_batch::DelegationAuthorization::capture(
        &db.conn(),
        "wallet",
        ROUND_ID,
        0,
        driver.signature,
    )
    .unwrap();
}

#[tokio::test]
#[ignore = "generates real ZKP2 proofs; run make proofs"]
async fn combined_executor_zkp2_cancellation_reopens_and_resumes_without_signing() {
    let (db, driver) = database();
    let peers = Peers::new(db.clone());
    let first = executor(db.clone(), peers.clone(), true);
    let host = context(driver.clone());
    let control = ChainSubmissionControl::new(1);
    prepare_early(&first, &host, &control).await;
    let outcome = advance_plan_head(
        &first,
        &host,
        &control,
        &CancelAfterPersistence(control.clone()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(outcome.disposition, RoundStepDisposition::Cancelled);
    assert!(peers.posts.lock().unwrap().is_empty());
    assert_eq!(peers.deliveries.load(Ordering::SeqCst), 0);
    let before =
        delegate_and_vote_batch::recover_delegate_and_vote_batch(&db, ROUND_ID, 0, 1).unwrap();
    let path = std::env::temp_dir().join(format!(
        "combined-executor-{}.sqlite",
        rand::random::<u64>()
    ));
    db.conn()
        .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
        .unwrap();
    drop((first, host, driver, peers, db));
    let db = std::sync::Arc::new(round::VotingDb::open(path.to_str().unwrap()).unwrap());
    db.set_wallet_id("wallet");
    let peers = Peers::new(db.clone());
    let resumed = executor(db, peers.clone(), false);
    let after = delegate_and_vote_batch::recover_delegate_and_vote_batch(
        &resumed.database(),
        ROUND_ID,
        0,
        2,
    )
    .unwrap();
    assert_eq!(before.batch_json, after.batch_json);
    let plan = resumed.plan().unwrap();
    assert!(
        matches!(
            plan.next_steps.first(),
            Some(session::NextStep::AdvanceVoteBatch { .. })
        ),
        "{:?}",
        plan.next_steps
    );
    let control = ChainSubmissionControl::new(2);
    // No delegation driver and no hotkey: recovery must consume durable bytes.
    let host = RoundHostContext {
        now_seconds: 99_999,
        ..super::fixtures::host()
    };
    let outcome = advance_plan_head(&resumed, &host, &control, &NoopRoundStepProgressReporter {})
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.disposition, RoundStepDisposition::Advanced);
    assert_eq!(peers.posts.lock().unwrap()[0], before.batch_json.as_bytes());
    assert_completed(&resumed, &peers);
    drop((resumed, peers));
    std::fs::remove_file(path).unwrap();
}
