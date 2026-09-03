use std::{
    path::{Path, PathBuf},
    sync::{Arc, Barrier},
};

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

fn shared_memory_uri(label: &str) -> String {
    format!("file:{}?mode=memory&cache=shared", unique_label(label))
}

fn remove_sqlite_files(path: &Path) {
    let path = path.to_string_lossy();
    let _ = std::fs::remove_file(path.as_ref());
    let _ = std::fs::remove_file(format!("{path}-shm"));
    let _ = std::fs::remove_file(format!("{path}-wal"));
}

#[test]
fn file_backed_handles_share_one_database_authority() {
    let path = temporary_path("shared");
    let path_string = path.to_string_lossy();
    let first = VotingDb::open(&path_string).unwrap();
    let second = VotingDb::open(&path_string).unwrap();

    assert!(Arc::ptr_eq(
        &first.database_authority,
        &second.database_authority
    ));

    drop(first);
    let third = VotingDb::open(&path_string).unwrap();
    assert!(Arc::ptr_eq(
        &second.database_authority,
        &third.database_authority
    ));

    drop((second, third));
    remove_sqlite_files(&path);
}

#[test]
fn shared_memory_handles_share_one_database_authority() {
    let uri = shared_memory_uri("shared-memory");
    let first = VotingDb::open(&uri).unwrap();
    let second = VotingDb::open(&uri).unwrap();

    assert!(Arc::ptr_eq(
        &first.database_authority,
        &second.database_authority
    ));
}

#[test]
fn equivalent_shared_memory_uris_share_one_database_authority() {
    let name = unique_label("shared-memory-alias");
    let encoded_name = name.replace('-', "%2D");
    let first = VotingDb::open(&format!("file:{name}?mode=memory&cache=shared")).unwrap();
    let second = VotingDb::open(&format!(
        "file:{encoded_name}?cache=shared&mode=memory#ignored"
    ))
    .unwrap();

    assert!(Arc::ptr_eq(
        &first.database_authority,
        &second.database_authority
    ));
}

#[test]
fn concurrent_opens_share_one_database_authority() {
    let path = temporary_path("concurrent");
    let path_string = path.to_string_lossy().into_owned();
    drop(VotingDb::open(&path_string).unwrap());

    let barrier = Arc::new(Barrier::new(2));
    let open_database = |barrier: Arc<Barrier>| {
        let path_string = path_string.clone();
        std::thread::spawn(move || {
            barrier.wait();
            VotingDb::open(&path_string).unwrap().database_authority
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
    let first = VotingDb::open(&first_path.to_string_lossy()).unwrap();
    let second = VotingDb::open(&second_path.to_string_lossy()).unwrap();

    assert!(!Arc::ptr_eq(
        &first.database_authority,
        &second.database_authority
    ));

    drop((first, second));
    remove_sqlite_files(&first_path);
    remove_sqlite_files(&second_path);
}

#[test]
fn different_shared_memory_names_have_independent_database_authorities() {
    let first = VotingDb::open(&shared_memory_uri("different-memory-first")).unwrap();
    let second = VotingDb::open(&shared_memory_uri("different-memory-second")).unwrap();

    assert!(!Arc::ptr_eq(
        &first.database_authority,
        &second.database_authority
    ));
}

#[test]
fn normalized_paths_share_one_database_authority() {
    let directory = temporary_path("normalized-parent");
    std::fs::create_dir_all(directory.join("alias")).unwrap();
    let canonical_path = directory.join("voting.sqlite");
    let normalized_alias = directory.join("alias").join("..").join("voting.sqlite");
    let first = VotingDb::open(&canonical_path.to_string_lossy()).unwrap();
    let second = VotingDb::open(&normalized_alias.to_string_lossy()).unwrap();

    assert!(Arc::ptr_eq(
        &first.database_authority,
        &second.database_authority
    ));

    drop((first, second));
    remove_sqlite_files(&canonical_path);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn in_memory_handles_have_independent_database_authorities() {
    let first = VotingDb::open(":memory:").unwrap();
    let second = VotingDb::open(":memory:").unwrap();

    assert!(!Arc::ptr_eq(
        &first.database_authority,
        &second.database_authority
    ));
}

#[test]
fn private_cache_memory_handles_have_independent_database_authorities() {
    let name = unique_label("private-memory");
    let uri = format!("file:{name}?mode=memory&cache=private");
    let first = VotingDb::open(&uri).unwrap();
    let second = VotingDb::open(&uri).unwrap();

    assert!(!Arc::ptr_eq(
        &first.database_authority,
        &second.database_authority
    ));
}

#[test]
fn temporary_database_handles_have_independent_database_authorities() {
    let first = VotingDb::open("").unwrap();
    let second = VotingDb::open("").unwrap();

    assert!(!Arc::ptr_eq(
        &first.database_authority,
        &second.database_authority
    ));
}
