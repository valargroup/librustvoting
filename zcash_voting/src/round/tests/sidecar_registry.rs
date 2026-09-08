//! Sidecar connection sharing: one connection per path, opens serialized per
//! path only.

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Barrier,
    },
    thread,
};

use crate::round::{registered_sidecar, sidecar_registry_key, VotingDb};
use crate::VotingError;

fn fresh_wallet_path(label: &str) -> std::path::PathBuf {
    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "zcash-voting-sidecar-{label}-{}-{}.sqlite",
        std::process::id(),
        NEXT_PATH.fetch_add(1, Ordering::Relaxed)
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
    let slow_key = sidecar_registry_key(&VotingDb::wallet_sidecar_path(&slow_wallet));
    let quick_key = sidecar_registry_key(&VotingDb::wallet_sidecar_path(&quick_wallet));

    // Hold the slow path's opener lock for as long as this test wants, rather
    // than making an open slow and racing it. What an open waits on is then
    // the only thing under test: no assertion here can be decided by how fast
    // the runner opened a database.
    let (slow_opener, _) = registered_sidecar(&slow_key);
    let (quick_opener, _) = registered_sidecar(&quick_key);
    assert!(
        !Arc::ptr_eq(&slow_opener, &quick_opener),
        "each sidecar path must get its own opener lock"
    );
    let held = slow_opener
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let blocked_open = {
        let slow_wallet = slow_wallet.clone();
        thread::spawn(move || VotingDb::open_wallet_sidecar(&slow_wallet, "slow-wallet"))
    };

    // Unbounded on purpose: were opens serialized across paths this would
    // block until the suite's slow-timeout killed the test, so the property
    // is decided by whether it returns at all, not by when.
    let quick = VotingDb::open_wallet_sidecar(&quick_wallet, "quick-wallet").unwrap();
    assert!(
        !blocked_open.is_finished(),
        "the held path's open must still be waiting when an unrelated open has finished"
    );

    drop(held);
    let slow = blocked_open.join().unwrap().unwrap();
    assert!(
        !slow.shares_connection_with(&quick),
        "two sidecar paths are two connections"
    );

    drop((slow, quick));
    std::fs::remove_file(VotingDb::wallet_sidecar_path(&slow_wallet)).ok();
    std::fs::remove_file(VotingDb::wallet_sidecar_path(&quick_wallet)).ok();
}

#[test]
fn an_open_another_process_keeps_locked_reports_db_busy() {
    let wallet_path = fresh_wallet_path("busy");
    let sidecar = VotingDb::wallet_sidecar_path(&wallet_path);

    // Another connection holds the sidecar's write lock for the whole open, so
    // every migration attempt inside the open's retry window is refused.
    let holder = rusqlite::Connection::open(&sidecar).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();

    let refused = VotingDb::open_wallet_sidecar(&wallet_path, "busy-wallet")
        .err()
        .expect("a write lock held for the whole open outlives its retry window");
    assert_eq!(refused.kind(), crate::VotingErrorKind::DbBusy);

    holder.execute_batch("ROLLBACK").unwrap();
    drop(holder);
    std::fs::remove_file(sidecar).ok();
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

#[test]
fn every_connection_to_one_file_shares_a_sidecar_id() {
    let wallet_path = fresh_wallet_path("sidecar-id");
    let sidecar = VotingDb::wallet_sidecar_path(&wallet_path);
    let sidecar = sidecar.to_str().unwrap();
    let first = VotingDb::open(sidecar).unwrap();
    let second = VotingDb::open(sidecar).unwrap();
    assert!(
        !first.shares_connection_with(&second),
        "VotingDb::open opens a connection per call"
    );
    assert_eq!(
        first.sidecar_id(),
        second.sidecar_id(),
        "proof locks and round locks keyed by the id must coordinate across connections to one file"
    );

    let other_file = fresh_wallet_path("sidecar-id-other");
    let other =
        VotingDb::open(VotingDb::wallet_sidecar_path(&other_file).to_str().unwrap()).unwrap();
    assert_ne!(first.sidecar_id(), other.sidecar_id());
    assert_ne!(
        VotingDb::open_in_memory().unwrap().sidecar_id(),
        VotingDb::open_in_memory().unwrap().sidecar_id(),
        "in-memory databases share no state and get distinct ids"
    );
}

#[test]
fn scoping_a_handle_to_an_empty_wallet_id_is_refused() {
    let db = VotingDb::open_in_memory().unwrap();
    let refused = match db.scoped("") {
        Ok(_) => panic!("an empty wallet id must be refused"),
        Err(error) => error,
    };
    assert!(
        matches!(refused, VotingError::InvalidInput { ref message } if message.contains("wallet id")),
        "got {refused:?}"
    );
    assert!(db.scoped("wallet-a").is_ok());
}

#[test]
fn an_empty_wallet_id_is_refused_before_the_sidecar_is_opened() {
    let wallet_path = fresh_wallet_path("empty-wallet-id");

    let refused = match VotingDb::open_wallet_sidecar(&wallet_path, "") {
        Ok(_) => panic!("an empty wallet id must be refused"),
        Err(error) => error,
    };

    assert!(
        matches!(refused, VotingError::InvalidInput { ref message } if message.contains("wallet id")),
        "expected InvalidInput naming the wallet id, got {refused:?}"
    );
    assert!(
        !VotingDb::wallet_sidecar_path(&wallet_path).exists(),
        "a refused open must not create the sidecar file"
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_to_an_existing_sidecar_shares_its_identity() {
    let wallet_path = fresh_wallet_path("symlink-target");
    let sidecar = VotingDb::wallet_sidecar_path(&wallet_path);
    // Create the sidecar first; a link can only be resolved once it exists.
    let by_path = VotingDb::open(sidecar.to_str().unwrap()).unwrap();
    let link = fresh_wallet_path("symlink-link").with_extension("voting.link");
    std::fs::remove_file(&link).ok();
    std::os::unix::fs::symlink(&sidecar, &link).unwrap();

    let by_link = VotingDb::open(link.to_str().unwrap()).unwrap();
    assert_eq!(
        by_path.sidecar_id(),
        by_link.sidecar_id(),
        "the real path and a symlink to it name one durable database"
    );
    assert_eq!(sidecar_registry_key(&link), sidecar_registry_key(&sidecar));
    std::fs::remove_file(&link).ok();
}

#[test]
fn every_connection_to_one_file_shares_chain_submission_coordination() {
    let wallet_path = fresh_wallet_path("chain-coordination");
    let sidecar = VotingDb::wallet_sidecar_path(&wallet_path);
    let sidecar = sidecar.to_str().unwrap();
    let first = VotingDb::open(sidecar).unwrap();
    let second = VotingDb::open(sidecar).unwrap();
    assert!(!first.shares_connection_with(&second));
    assert!(
        first.shares_chain_coordination_with(&second),
        "two connections to one sidecar act on the same submission rows, so their in-flight and identity locks must be one authority"
    );

    let other_file = fresh_wallet_path("chain-coordination-other");
    let other =
        VotingDb::open(VotingDb::wallet_sidecar_path(&other_file).to_str().unwrap()).unwrap();
    assert!(!first.shares_chain_coordination_with(&other));

    drop(first);
    drop(second);
    let reopened = VotingDb::open(sidecar).unwrap();
    let reopened_again = VotingDb::open(sidecar).unwrap();
    assert!(
        reopened.shares_chain_coordination_with(&reopened_again),
        "a new open span gets one fresh authority shared by its connections"
    );
}
