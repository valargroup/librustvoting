//! A concurrent reset cannot invalidate a successful sync before witness capture.
use super::*;
use std::{sync::mpsc, time::Duration};

#[test]
fn reset_waits_until_the_synced_witness_is_captured() {
    for reset_all in [false, true] {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(tests::WALLET_ID);
        db.create_round(crate::Network::Testnet, &tests::round_params(), None)
            .unwrap();
        db.ensure_bundles(tests::ROUND_ID, &[tests::note(0)])
            .unwrap();
        db.store_van_position(tests::ROUND_ID, 0, 0).unwrap();
        use pasta_curves::group::ff::PrimeField;
        db.conn()
            .execute(
                "UPDATE bundles SET gov_comm=?1",
                [pasta_curves::Fp::from(1).to_repr().to_vec()],
            )
            .unwrap();
        let tree = VoteTreeSync::new();
        let server = tests::server_with_single_leaf_blocks(1);
        let (synced, ready) = mpsc::channel();
        let (release, capture) = mpsc::channel();
        let (reset_done, reset_result) = mpsc::channel();
        std::thread::scope(|scope| {
            let tree = &tree;
            let db = &db;
            let server = &server;
            let witness = scope.spawn(move || {
                tree.coordinated(tests::ROUND_ID, || {
                    let height = tree.sync_locked(db, tests::ROUND_ID, server)?;
                    synced.send(()).unwrap();
                    capture.recv().unwrap();
                    tree.witness_locked(db, tests::ROUND_ID, 0, height)
                })
            });
            ready.recv_timeout(Duration::from_secs(3)).unwrap();
            scope.spawn(|| {
                reset_done
                    .send(tree.reset(if reset_all { "" } else { tests::ROUND_ID }))
                    .unwrap();
            });
            let premature = reset_result.recv_timeout(Duration::from_millis(50));
            release.send(()).unwrap();
            assert!(
                premature.is_err(),
                "reset must wait through witness capture"
            );
            assert_eq!(witness.join().unwrap().unwrap().position, 0);
            reset_result
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
        });
        assert!(tree.cached_rounds().is_empty());
    }
}
