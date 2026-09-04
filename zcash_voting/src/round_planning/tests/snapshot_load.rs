//! The planner reads one snapshot: a write committed on another connection
//! while the snapshot is being read is not visible to any part of it.

use std::{sync::mpsc, thread, time::Instant};

use crate::round::RoundParams;
use crate::round::VotingDb;
use crate::round_planning::load_round_snapshot;
use crate::session::{resume_plan, Decision};

const ROUND: &str = "0101010101010101010101010101010101010101010101010101010101010101";
const WALLET: &str = "wallet-snapshot";

fn fresh_sidecar_path(label: &str) -> String {
    let path = std::env::temp_dir().join(format!(
        "zcash-voting-round-snapshot-{label}-{}-{}.sqlite",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    std::fs::remove_file(&path).ok();
    path.to_str().unwrap().to_string()
}

fn round_params() -> RoundParams {
    RoundParams {
        vote_round_id: ROUND.to_string(),
        snapshot_height: 1000,
        ea_pk: vec![0xEA; 32],
        nc_root: vec![0xAA; 32],
        nullifier_imt_root: vec![0xBB; 32],
    }
}

#[test]
fn a_write_on_another_connection_does_not_fall_between_two_reads_of_one_snapshot() {
    let path = fresh_sidecar_path("isolation");
    let planner = VotingDb::open(&path).unwrap();
    planner.set_wallet_id(WALLET);
    planner
        .create_round(crate::Network::Testnet, &round_params(), None)
        .unwrap();
    let writer = VotingDb::open(&path).unwrap();
    writer.set_wallet_id(WALLET);
    assert!(!planner.shares_connection_with(&writer));

    let (release_writer, writer_released) = mpsc::channel::<()>();
    let writer_thread = thread::spawn(move || {
        writer_released.recv().unwrap();
        writer
            .set_ballot_intent(ROUND, 1, Decision::Skipped, 2)
            .unwrap();
    });

    let (before, after) = planner
        .read_transaction("snapshot isolation", |tx| {
            let before = load_round_snapshot(tx, WALLET, ROUND)?;
            release_writer.send(()).unwrap();
            writer_thread.join().unwrap();
            let after = load_round_snapshot(tx, WALLET, ROUND)?;
            Ok((before, after))
        })
        .unwrap();

    assert!(before.intents.is_empty());
    assert!(
        after.intents.is_empty(),
        "the write committed while the snapshot was open must not be visible to a later read in the same transaction: {:?}",
        after.intents
    );

    let plan = resume_plan(&planner, ROUND, &[1, 2]).unwrap();
    assert_eq!(
        plan.open_proposals,
        vec![2],
        "a fresh plan after the transaction sees the committed intent"
    );
    std::fs::remove_file(&path).ok();
}
