use std::{
    path::{Path, PathBuf},
    sync::{Arc, Barrier},
};

#[cfg(target_os = "linux")]
use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

use super::super::*;

fn unique_label(label: &str) -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        "voting-database-authority-{label}-{}-{nonce}",
        std::process::id()
    )
}

fn temporary_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{}.sqlite", unique_label(label)))
}

fn remove_sqlite_files(path: &Path) {
    let mut shared_memory_path = path.as_os_str().to_os_string();
    shared_memory_path.push("-shm");
    let mut write_ahead_log_path = path.as_os_str().to_os_string();
    write_ahead_log_path.push("-wal");
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(shared_memory_path);
    let _ = std::fs::remove_file(write_ahead_log_path);
}

#[test]
fn file_handles_share_physical_state_and_database_authority() {
    let path = temporary_path("shared");
    let first = VotingDb::open_path(&path).unwrap();
    first
        .conn()
        .execute_batch("CREATE TABLE authority_probe(value INTEGER);")
        .unwrap();
    let second = VotingDb::open_path(&path).unwrap();

    let table_count: u64 = second
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name='authority_probe'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(table_count, 1);
    assert!(Arc::ptr_eq(
        &first.database_authority,
        &second.database_authority
    ));

    drop((first, second));
    remove_sqlite_files(&path);
}

#[test]
fn canonical_path_aliases_share_one_database_authority() {
    let directory = temporary_path("normalized-parent");
    std::fs::create_dir_all(directory.join("alias")).unwrap();
    let canonical_path = directory.join("voting.sqlite");
    let normalized_alias = directory.join("alias").join("..").join("voting.sqlite");
    let first = VotingDb::open_path(&canonical_path).unwrap();
    let second = VotingDb::open_path(&normalized_alias).unwrap();

    assert!(Arc::ptr_eq(
        &first.database_authority,
        &second.database_authority
    ));

    drop((first, second));
    remove_sqlite_files(&canonical_path);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn concurrent_opens_share_one_database_authority() {
    let path = temporary_path("concurrent");
    drop(VotingDb::open_path(&path).unwrap());

    let barrier = Arc::new(Barrier::new(2));
    let open_database = |barrier: Arc<Barrier>| {
        let path = path.clone();
        std::thread::spawn(move || {
            barrier.wait();
            VotingDb::open_path(&path).unwrap().database_authority
        })
    };
    let first = open_database(Arc::clone(&barrier));
    let second = open_database(barrier);
    let first_authority = first.join().unwrap();
    let second_authority = second.join().unwrap();

    assert!(Arc::ptr_eq(&first_authority, &second_authority));

    drop((first_authority, second_authority));
    remove_sqlite_files(&path);
}

#[test]
fn different_files_have_independent_database_authorities() {
    let first_path = temporary_path("different-first");
    let second_path = temporary_path("different-second");
    let first = VotingDb::open_path(&first_path).unwrap();
    let second = VotingDb::open_path(&second_path).unwrap();

    assert!(!Arc::ptr_eq(
        &first.database_authority,
        &second.database_authority
    ));

    drop((first, second));
    remove_sqlite_files(&first_path);
    remove_sqlite_files(&second_path);
}

#[test]
fn unused_database_authority_is_released() {
    let path = temporary_path("released");
    let database = VotingDb::open_path(&path).unwrap();
    let authority = Arc::downgrade(&database.database_authority);

    drop(database);

    assert!(authority.upgrade().is_none());
    let reopened = VotingDb::open_path(&path).unwrap();
    assert!(authority.upgrade().is_none());

    drop(reopened);
    remove_sqlite_files(&path);
}

#[test]
fn in_memory_handles_have_independent_database_authorities() {
    let first = VotingDb::open_in_memory().unwrap();
    let second = VotingDb::open_in_memory().unwrap();

    first
        .conn()
        .execute_batch("CREATE TABLE authority_probe(value INTEGER);")
        .unwrap();
    let second_table_count: u64 = second
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name='authority_probe'",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(second_table_count, 0);
    assert!(!Arc::ptr_eq(
        &first.database_authority,
        &second.database_authority
    ));
}

#[test]
fn legacy_string_open_accepts_only_filesystem_paths() {
    let path = temporary_path("legacy-string");
    let path_string = path.to_str().unwrap();
    let database = VotingDb::open(path_string).unwrap();

    drop(database);
    remove_sqlite_files(&path);

    for unsupported_path in [
        "",
        ":memory:",
        "file:wallet.sqlite",
        "file:wallet.sqlite?vfs=memdb",
    ] {
        assert!(matches!(
            VotingDb::open(unsupported_path),
            Err(VotingError::InvalidInput { .. })
        ));
        assert!(matches!(
            VotingDb::open_path(Path::new(unsupported_path)),
            Err(VotingError::InvalidInput { .. })
        ));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn non_utf8_file_paths_share_one_database_authority() {
    let path_prefix = std::env::temp_dir().join(unique_label("non-utf8"));
    let mut path_bytes = path_prefix.as_os_str().as_bytes().to_vec();
    path_bytes.extend_from_slice(&[b'-', 0xff]);
    path_bytes.extend_from_slice(b".sqlite");
    let path = PathBuf::from(OsStr::from_bytes(&path_bytes));
    let first = VotingDb::open_path(&path).unwrap();
    let second = VotingDb::open_path(&path).unwrap();

    assert!(Arc::ptr_eq(
        &first.database_authority,
        &second.database_authority
    ));

    drop((first, second));
    remove_sqlite_files(&path);
}
