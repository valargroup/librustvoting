//! Does a committed sidecar write survive `abort()`?
//!
//! The share-delivery path commits its attempt marker in an immediate
//! transaction before dispatching, yet a crash inside the dispatch finds it
//! gone. This isolates the question from the SDK entirely: write, commit,
//! abort, reopen.
fn main() {
    let path = std::env::args().nth(1).expect("path");
    let phase = std::env::args().nth(2).unwrap_or_default();
    let db = zcash_voting::round::VotingDb::open_path(std::path::Path::new(&path)).unwrap();
    if phase == "write" {
        let conn = db.conn();
        let tx = conn.unchecked_transaction().expect("begin");
        // Errors are fatal here, not swallowed: an insert that silently failed
        // would make the reopen read zero rows and look exactly like lost
        // durability.
        let inserted = tx
            .execute(
                "INSERT INTO rounds (round_id, wallet_id, network, snapshot_height, ea_pk, nc_root, nullifier_imt_root, phase, created_at)
                 VALUES ('probe','w','testnet',1,x'00',x'00',x'00',0,1)",
                [],
            )
            .expect("insert");
        assert_eq!(inserted, 1, "the probe row must actually be inserted");
        let seen: i64 = tx
            .query_row(
                "select count(*) from rounds where round_id='probe'",
                [],
                |r| r.get(0),
            )
            .expect("read back inside the transaction");
        assert_eq!(seen, 1, "the row must be visible before commit");
        tx.commit().expect("commit");
        eprintln!("probe: committed, aborting");
        unsafe { libc::abort() }
    }
    let n: i64 = db
        .conn()
        .query_row(
            "select count(*) from rounds where round_id='probe'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(-1);
    println!("rounds named probe after reopen: {n}");
}
