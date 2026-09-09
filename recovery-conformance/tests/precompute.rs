use recovery_conformance::precompute::{seed_precompute, ProofCacheSeed};
use rusqlite::Connection;

fn fixture(path: &std::path::Path) -> Connection {
    let database = Connection::open(path).unwrap();
    database
        .execute_batch(
            "CREATE TABLE pir_proof_cache (wallet_id TEXT PRIMARY KEY, proof BLOB);
         CREATE TABLE bundles (
             wallet_id TEXT, round_id TEXT, bundle_index INTEGER, padded_note_secrets BLOB
         );",
        )
        .unwrap();
    database
}

#[test]
fn cold_fault_keeps_dummy_nullifiers_but_resume_can_warm_proofs() {
    let directory = FixtureDirectory::new();
    let template = directory.path().join("template.db");
    let sidecar = directory.path().join("sidecar.db");
    let warm = fixture(&template);
    warm.execute_batch(
        "INSERT INTO pir_proof_cache VALUES ('wallet', x'01');
         INSERT INTO bundles VALUES ('wallet', 'control', 0, x'02');",
    )
    .unwrap();
    let cold = fixture(&sidecar);
    cold.execute_batch("INSERT INTO bundles VALUES ('wallet', 'round', 0, NULL);")
        .unwrap();

    let seeded = seed_precompute(&sidecar, &template, "round", ProofCacheSeed::Cold).unwrap();
    assert_eq!(seeded.proofs, 0);
    assert_eq!(seeded.padded_bundles, 1);
    assert_eq!(
        cold.query_row("SELECT count(*) FROM pir_proof_cache", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        cold.query_row("SELECT padded_note_secrets FROM bundles", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        vec![2]
    );

    let resumed = seed_precompute(&sidecar, &template, "round", ProofCacheSeed::Warm).unwrap();
    assert_eq!(resumed.proofs, 1);
    assert_eq!(resumed.padded_bundles, 0);
    assert_eq!(
        cold.query_row("SELECT proof FROM pir_proof_cache", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        vec![1]
    );
}

#[test]
fn seeding_preserves_existing_padding_and_other_wallets_and_rounds() {
    let directory = FixtureDirectory::new();
    let template = directory.path().join("template.db");
    let sidecar = directory.path().join("sidecar.db");
    let warm = fixture(&template);
    warm.execute_batch("INSERT INTO bundles VALUES ('wallet', 'control', 0, x'02');")
        .unwrap();
    let cold = fixture(&sidecar);
    cold.execute_batch(
        "INSERT INTO bundles VALUES ('wallet', 'round', 0, x'03');
         INSERT INTO bundles VALUES ('other-wallet', 'round', 0, NULL);
         INSERT INTO bundles VALUES ('wallet', 'other-round', 0, NULL);",
    )
    .unwrap();
    let seeded = seed_precompute(&sidecar, &template, "round", ProofCacheSeed::Warm).unwrap();
    assert_eq!(seeded.padded_bundles, 0);
    let padding: Vec<Option<Vec<u8>>> = cold
        .prepare("SELECT padded_note_secrets FROM bundles ORDER BY rowid")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(padding, vec![Some(vec![3]), None, None]);
}

/// Isolated fixture files, removed even when an assertion unwinds.
struct FixtureDirectory(std::path::PathBuf);

impl FixtureDirectory {
    fn new() -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "recovery-precompute-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for FixtureDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
