use super::{fixtures::*, *};
use tokio::sync::Semaphore;

#[tokio::test(start_paused = true)]
async fn queued_replaced_generation_is_not_posted_or_mutated() {
    let fixture = Fixture::new(3);
    let gate = Arc::new(Semaphore::new(0));
    let transport = ScriptedTransport::new({
        let gate = gate.clone();
        move |_| ReplyPlan {
            gate: Some(gate.clone()),
            ..Default::default()
        }
    });
    let replace = async {
        transport.wait_for(32).await;
        fixture.db.conn().execute(
            "UPDATE votes SET commitment_bundle_json = replace(commitment_bundle_json, '\"anchor_height\":123', '\"anchor_height\":124') WHERE proposal_id = 3", []
        ).unwrap();
        gate.add_permits(SHARE_COUNT * 3);
    };
    let (mut reports, ()) = tokio::join!(fixture.deliver(transport.clone(), &uncancelled), replace);
    assert_complete(reports.drain(..2).collect(), 2);
    let failed = reports.remove(0).unwrap_err().partial.unwrap();
    assert_eq!(failed.pending_share_indices.len(), SHARE_COUNT);
    assert_eq!(transport.count(), SHARE_COUNT * 2);
    assert!(share::list(&fixture.db, ROUND_ID)
        .unwrap()
        .iter()
        .all(|share| share.proposal_id <= 2));
}

#[tokio::test(start_paused = true)]
async fn queued_work_retains_its_wallet_even_after_active_wallet_switches() {
    let fixture = Fixture::new(3);
    let wallet = fixture.db.wallet_id();
    let gate = Arc::new(Semaphore::new(0));
    let transport = ScriptedTransport::new({
        let gate = gate.clone();
        move |_| ReplyPlan {
            gate: Some(gate.clone()),
            ..Default::default()
        }
    });
    let switch = async {
        transport.wait_for(32).await;
        fixture.db.set_wallet_id("other-wallet");
        seed_round_and_bundle(&fixture.db);
        gate.add_permits(SHARE_COUNT * 3);
    };
    let (reports, ()) = tokio::join!(fixture.deliver(transport.clone(), &uncancelled), switch);
    assert_complete(reports, 3);
    assert!(share::list(&fixture.db, ROUND_ID).unwrap().is_empty());
    fixture.db.set_wallet_id(&wallet);
    assert_eq!(
        share::list(&fixture.db, ROUND_ID).unwrap().len(),
        SHARE_COUNT * 3
    );
}

#[tokio::test(start_paused = true)]
async fn waiting_in_queue_does_not_spend_the_next_shares_fanout_budget() {
    let fixture = Fixture::new(5);
    let transport = ScriptedTransport::new(|wire| ReplyPlan {
        delay: if wire.proposal_id <= 4 {
            Duration::from_secs(31)
        } else {
            Duration::ZERO
        },
        ..Default::default()
    });
    let start = tokio::time::Instant::now();
    let reports = fixture.deliver(transport.clone(), &uncancelled).await;
    // Two waves of 32 shares consume the 30-second request timeout each.
    assert!(start.elapsed() >= Duration::from_secs(60));
    assert_eq!(transport.count(), SHARE_COUNT * 5);
    assert!(reports[4]
        .as_ref()
        .unwrap()
        .deliveries
        .iter()
        .all(|share| share.submission.accepted_urls.len() == 1));
}

#[tokio::test(start_paused = true)]
async fn reopened_database_reuses_plans_and_only_delivers_unsent_proposals() {
    struct Sidecar(std::path::PathBuf);
    impl Drop for Sidecar {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
            }
        }
    }
    let sidecar = Sidecar(std::env::temp_dir().join(format!(
        "helper-queue-{}-{}.sqlite", std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    )));
    fn plans(db: &VotingDb) -> Vec<(String, String)> {
        db.conn().prepare("SELECT commitment_bundle_json, share_plans_json FROM helper_share_plans ORDER BY proposal_id")
            .unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?))).unwrap().map(Result::unwrap).collect()
    }
    let fixture = Fixture::seed(Arc::new(VotingDb::open_path(&sidecar.0).unwrap()), 3);
    let original_plans = plans(&fixture.db);
    let wallet = fixture.db.wallet_id();
    let gate = Arc::new(Semaphore::new(0));
    let transport = ScriptedTransport::new({
        let gate = gate.clone();
        move |wire| ReplyPlan {
            gate: (wire.proposal_id <= 2).then(|| gate.clone()),
            status: if wire.proposal_id == 2 { 503 } else { 200 },
            ..Default::default()
        }
    });
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    let cancel = || cancelled.load(Ordering::SeqCst);
    let interrupt = async {
        transport.wait_for(SHARE_COUNT * 2).await;
        cancelled.store(true, Ordering::SeqCst);
        gate.add_permits(SHARE_COUNT * 2);
    };
    let (reports, ()) = tokio::join!(fixture.deliver(transport.clone(), &cancel), interrupt);
    assert_eq!(transport.count(), SHARE_COUNT * 2);
    assert!(reports[2].as_ref().unwrap().cancelled);
    assert_eq!(
        reports[2].as_ref().unwrap().pending_share_indices.len(),
        SHARE_COUNT
    );
    drop(fixture);

    let db = Arc::new(VotingDb::open_path(&sidecar.0).unwrap());
    db.set_wallet_id(&wallet);
    let votes = (1..=3)
        .map(|proposal| {
            crate::vote::CommittedVote::recover(&db, ROUND_ID, 0, proposal)
                .unwrap()
                .confirmed(&db)
                .unwrap()
                .unwrap()
        })
        .collect();
    let resumed = Fixture {
        db,
        votes,
        configured: helpers(1),
    };
    let transport = ScriptedTransport::new(|_| ReplyPlan::default());
    let reports = resumed.deliver(transport.clone(), &uncancelled).await;
    assert_eq!(plans(&resumed.db), original_plans);
    assert_eq!(transport.count(), SHARE_COUNT);
    assert!(transport
        .started
        .lock()
        .unwrap()
        .iter()
        .all(|wire| wire.proposal_id == 3));
    assert_eq!(reports[1].as_ref().unwrap().deliveries.len(), SHARE_COUNT);
    assert!(reports[1]
        .as_ref()
        .unwrap()
        .deliveries
        .iter()
        .all(|share| share.submission.accepted_urls.is_empty()
            && share.submission.ambiguous_urls == helpers(1)));
    assert!(reports[2]
        .as_ref()
        .unwrap()
        .deliveries
        .iter()
        .all(|share| share.submission.accepted_urls == helpers(1)));
}
