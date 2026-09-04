//! Sidecar connection sharing: one connection per path, opens serialized per
//! path only.

use std::{
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

use crate::round::{sidecar_registry_key, VotingDb};

fn fresh_wallet_path(label: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zcash-voting-sidecar-{label}-{}-{}.sqlite",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    std::fs::remove_file(VotingDb::wallet_sidecar_path(&path)).ok();
    path
}

#[test]
fn concurrent_opens_of_one_path_share_one_connection() {
    const OPENERS: usize = 4;
    let wallet_path = Arc::new(fresh_wallet_path("shared"));
    let start = Arc::new(Barrier::new(OPENERS));

    let handles: Vec<_> = (0..OPENERS)
        .map(|index| {
            let wallet_path = Arc::clone(&wallet_path);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                VotingDb::open_wallet_sidecar(&wallet_path, &format!("wallet-{index}")).unwrap()
            })
        })
        .collect();
    let databases: Vec<Arc<VotingDb>> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();

    for database in &databases[1..] {
        assert!(databases[0].shares_connection_with(database));
    }
    let wallet_ids: Vec<String> = databases.iter().map(|db| db.wallet_id()).collect();
    for index in 0..OPENERS {
        assert!(wallet_ids.contains(&format!("wallet-{index}")));
    }

    drop(databases);
    std::fs::remove_file(VotingDb::wallet_sidecar_path(&wallet_path)).ok();
}

#[test]
fn a_slow_open_of_one_path_does_not_block_another_path() {
    let slow_wallet = fresh_wallet_path("slow");
    let quick_wallet = fresh_wallet_path("quick");
    let slow_sidecar = VotingDb::wallet_sidecar_path(&slow_wallet);

    // Another process holds the slow sidecar's write lock for the whole test,
    // so its open spends the entire busy-retry window inside the open and
    // then fails with DbBusy. Only ordering is asserted, never wall-clock
    // budgets, so the test does not depend on machine speed.
    let holder = rusqlite::Connection::open(&slow_sidecar).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();

    let slow_open = {
        let slow_wallet = slow_wallet.clone();
        thread::spawn(move || {
            let outcome = VotingDb::open_wallet_sidecar(&slow_wallet, "slow-wallet");
            (outcome, Instant::now())
        })
    };
    // Give the slow open time to enter its retry loop; if the runner is so
    // slow that it has not, the ordering assertion below still holds.
    thread::sleep(Duration::from_millis(100));
    assert!(
        !slow_open.is_finished(),
        "the slow open must still be inside its busy-retry window"
    );

    let quick = VotingDb::open_wallet_sidecar(&quick_wallet, "quick-wallet").unwrap();
    let quick_finished_at = Instant::now();

    let (slow_outcome, slow_finished_at) = slow_open.join().unwrap();
    holder.execute_batch("ROLLBACK").unwrap();

    assert!(
        quick_finished_at < slow_finished_at,
        "an unrelated sidecar open waited behind a busy open of another path"
    );
    let slow_error = slow_outcome
        .err()
        .expect("the held lock outlives the retry window");
    assert_eq!(slow_error.kind(), crate::VotingErrorKind::DbBusy);

    drop((quick, holder));
    std::fs::remove_file(slow_sidecar).ok();
    std::fs::remove_file(VotingDb::wallet_sidecar_path(&quick_wallet)).ok();
}

#[test]
fn a_bare_file_name_keys_the_same_connection_as_its_other_spellings() {
    let bare = sidecar_registry_key(std::path::Path::new("wallet.sqlite.voting"));
    let dotted = sidecar_registry_key(std::path::Path::new("./wallet.sqlite.voting"));
    let absolute = sidecar_registry_key(
        &std::env::current_dir()
            .unwrap()
            .join("wallet.sqlite.voting"),
    );

    assert!(bare.is_absolute(), "{}", bare.display());
    assert_eq!(bare, dotted);
    assert_eq!(bare, absolute);
}
