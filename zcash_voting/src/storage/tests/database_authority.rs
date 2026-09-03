use std::{
    ffi::CStr,
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

fn shared_memory_uri(label: &str) -> String {
    format!("file:{}?mode=memory&cache=shared", unique_label(label))
}

fn sqlite_vfs_names() -> Vec<String> {
    let mut names = Vec::new();
    // SAFETY: SQLite owns the process-global VFS list for the duration of the
    // process. Each registered VFS supplies a NUL-terminated static name.
    unsafe {
        let mut vfs = rusqlite::ffi::sqlite3_vfs_find(std::ptr::null());
        while let Some(registered_vfs) = vfs.as_ref() {
            names.push(
                CStr::from_ptr(registered_vfs.zName)
                    .to_string_lossy()
                    .into_owned(),
            );
            vfs = registered_vfs.pNext;
        }
    }
    names
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
fn memory_mode_without_explicit_cache_conservatively_shares_authority() {
    let uri = format!("file:{}?mode=memory", unique_label("implicit-memory-cache"));
    let first = VotingDb::open(&uri).unwrap();
    let second = VotingDb::open(&uri).unwrap();

    assert!(Arc::ptr_eq(
        &first.database_authority,
        &second.database_authority
    ));
}

#[test]
fn special_memory_uri_without_explicit_cache_conservatively_shares_authority() {
    let uri = "file::memory:";
    let first = VotingDb::open(uri).unwrap();
    let second = VotingDb::open(uri).unwrap();

    assert!(Arc::ptr_eq(
        &first.database_authority,
        &second.database_authority
    ));
}

#[test]
fn special_memory_uri_with_private_cache_has_independent_authorities() {
    let uri = "file::memory:?cache=private";
    let first = VotingDb::open(uri).unwrap();
    let second = VotingDb::open(uri).unwrap();

    assert!(!Arc::ptr_eq(
        &first.database_authority,
        &second.database_authority
    ));
}

#[test]
fn rooted_memdb_handles_share_one_database_authority() {
    let label = unique_label("memdb");
    for uri in [
        format!("file:/{label}-slash?vfs=memdb"),
        format!("file:%5C{label}-backslash?vfs=memdb"),
        format!("file:/{label}-private?vfs=memdb&cache=private"),
    ] {
        let first = VotingDb::open(&uri).unwrap();
        let second = VotingDb::open(&uri).unwrap();

        assert!(Arc::ptr_eq(
            &first.database_authority,
            &second.database_authority
        ));
    }
}

#[test]
fn unrooted_memdb_handles_with_shared_cache_share_one_database_authority() {
    let uri = format!(
        "file:{}?vfs=memdb&cache=shared",
        unique_label("shared-cache-memdb")
    );
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
fn explicit_default_vfs_preserves_shared_memory_database_authority() {
    let name = unique_label("shared-memory-default-vfs");
    let default_vfs = sqlite_vfs_names()
        .into_iter()
        .next()
        .expect("bundled SQLite must register a default VFS");
    let implicit = VotingDb::open(&format!("file:{name}?mode=memory&cache=shared")).unwrap();
    let explicit = VotingDb::open(&format!(
        "file:{name}?mode=memory&cache=shared&vfs={default_vfs}"
    ))
    .unwrap();

    assert!(Arc::ptr_eq(
        &implicit.database_authority,
        &explicit.database_authority
    ));
}

#[test]
fn different_vfses_have_independent_shared_memory_database_authorities() {
    let name = unique_label("shared-memory-different-vfs");
    let vfs_names = sqlite_vfs_names();
    assert!(
        vfs_names.len() >= 2,
        "bundled SQLite must register two VFSes for this regression"
    );
    let first = VotingDb::open(&format!(
        "file:{name}?mode=memory&cache=shared&vfs={}",
        vfs_names[0]
    ))
    .unwrap();
    let second = VotingDb::open(&format!(
        "file:{name}?mode=memory&cache=shared&vfs={}",
        vfs_names[1]
    ))
    .unwrap();

    assert!(!Arc::ptr_eq(
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

#[cfg(target_os = "linux")]
#[test]
fn non_utf8_file_uri_handles_share_one_database_authority() {
    let path_prefix = std::env::temp_dir().join(unique_label("non-utf8"));
    let mut path_bytes = path_prefix.as_os_str().as_bytes().to_vec();
    path_bytes.extend_from_slice(b"-%FF.sqlite");
    let uri = format!("file:{}", String::from_utf8(path_bytes.clone()).unwrap());
    let percent_escape = path_bytes
        .windows(3)
        .position(|window| window == b"%FF")
        .unwrap();
    path_bytes.splice(percent_escape..percent_escape + 3, [0xff]);
    let sqlite_path = PathBuf::from(OsStr::from_bytes(&path_bytes));

    let first = VotingDb::open(&uri).unwrap();
    let second = VotingDb::open(&uri).unwrap();

    assert!(Arc::ptr_eq(
        &first.database_authority,
        &second.database_authority
    ));

    drop((first, second));
    remove_sqlite_files(&sqlite_path);
}

#[cfg(target_os = "linux")]
#[test]
fn raw_non_utf8_filenames_resolve_one_database_authority() {
    let path_prefix = std::env::temp_dir().join(unique_label("raw-non-utf8"));
    let mut path_bytes = path_prefix.as_os_str().as_bytes().to_vec();
    path_bytes.extend_from_slice(&[b'-', 0xff]);
    path_bytes.extend_from_slice(b".sqlite");
    let sqlite_path = PathBuf::from(OsStr::from_bytes(&path_bytes));
    let first_connection = Connection::open(&sqlite_path).unwrap();
    let second_connection = Connection::open(&sqlite_path).unwrap();

    let first = DatabaseAuthority::for_connection(&first_connection, "").unwrap();
    let second = DatabaseAuthority::for_connection(&second_connection, "").unwrap();

    assert!(Arc::ptr_eq(&first, &second));

    drop((first, second, first_connection, second_connection));
    remove_sqlite_files(&sqlite_path);
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
fn private_memory_mode_overrides_rooted_memdb_sharing() {
    let uri = format!(
        "file:/{}?vfs=memdb&mode=memory&cache=private",
        unique_label("private-rooted-memdb")
    );
    let first = VotingDb::open(&uri).unwrap();
    let second = VotingDb::open(&uri).unwrap();

    assert!(!Arc::ptr_eq(
        &first.database_authority,
        &second.database_authority
    ));
}

#[test]
fn empty_name_memory_handles_have_independent_database_authorities() {
    let uri = "file:?mode=memory&cache=shared";
    let first = VotingDb::open(uri).unwrap();
    let second = VotingDb::open(uri).unwrap();

    assert!(!Arc::ptr_eq(
        &first.database_authority,
        &second.database_authority
    ));
}

#[test]
fn unnamed_or_private_memdb_handles_have_independent_database_authorities() {
    for uri in [
        "file:/?vfs=memdb".to_owned(),
        "file:%5C?vfs=memdb".to_owned(),
        format!(
            "file:{}?vfs=memdb&cache=private",
            unique_label("private-unrooted-memdb")
        ),
    ] {
        let first = VotingDb::open(&uri).unwrap();
        let second = VotingDb::open(&uri).unwrap();

        assert!(!Arc::ptr_eq(
            &first.database_authority,
            &second.database_authority
        ));
    }
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
