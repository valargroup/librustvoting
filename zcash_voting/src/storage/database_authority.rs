//! Process-local coordination shared by every handle to one SQLite database.

use std::{
    collections::HashMap,
    ffi::CStr,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex, Weak},
};

use rusqlite::Connection;

use crate::{chain_submission::coordination::SubmissionCoordination, types::VotingError};

#[cfg(unix)]
use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

/// Stable process-local identity for a database that more than one handle can open.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DatabaseIdentity {
    File(PathBuf),
    SharedMemory {
        database_name: Vec<u8>,
        vfs_identity: usize,
    },
}

/// Identity and memory semantics of the VFS selected by SQLite.
struct ConnectionVfs {
    identity: usize,
    is_memdb: bool,
}

/// Process-local owner of coordination that must agree across database handles.
#[derive(Default)]
pub(super) struct DatabaseAuthority {
    chain_submission: SubmissionCoordination,
}

static DATABASE_AUTHORITIES: LazyLock<Mutex<HashMap<DatabaseIdentity, Weak<DatabaseAuthority>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

impl DatabaseAuthority {
    /// Returns the authority for the connection's main database.
    ///
    /// File-backed databases are interned by canonical path. SQLite URI memory
    /// databases that may use shared caching and named `memdb` databases are
    /// interned by SQLite's decoded database name and selected VFS. Plain
    /// `:memory:`, anonymous, temporary, and URI memory-mode databases with an
    /// explicitly private cache receive private authorities.
    pub(super) fn for_connection(
        connection: &Connection,
        opening_path: &str,
    ) -> Result<Arc<Self>, VotingError> {
        let Some(identity) = DatabaseIdentity::for_connection(connection, opening_path)? else {
            return Ok(Arc::new(Self::default()));
        };

        let mut authorities =
            DATABASE_AUTHORITIES
                .lock()
                .map_err(|error| VotingError::Internal {
                    message: format!("database authority registry poisoned: {error}"),
                })?;
        authorities.retain(|_, authority| authority.strong_count() > 0);
        if let Some(authority) = authorities.get(&identity).and_then(Weak::upgrade) {
            return Ok(authority);
        }

        let authority = Arc::new(Self::default());
        authorities.insert(identity, Arc::downgrade(&authority));
        Ok(authority)
    }

    /// Returns the chain lifecycle authority shared across database handles.
    pub(super) fn chain_submission(&self) -> &SubmissionCoordination {
        &self.chain_submission
    }
}

impl DatabaseIdentity {
    /// Resolves the identity SQLite assigned to an opened connection.
    fn for_connection(
        connection: &Connection,
        opening_path: &str,
    ) -> Result<Option<Self>, VotingError> {
        if let Some(sqlite_path) = connection_file_path(connection)? {
            let canonical_path =
                std::fs::canonicalize(sqlite_path).map_err(|error| VotingError::Storage {
                    message: format!("failed to resolve SQLite database authority: {error}"),
                })?;
            return Ok(Some(Self::File(canonical_path)));
        }

        let selected_vfs = connection_vfs(connection)?;
        let Some(database_name) = shared_memory_name(opening_path, selected_vfs.is_memdb) else {
            return Ok(None);
        };
        Ok(Some(Self::SharedMemory {
            database_name,
            vfs_identity: selected_vfs.identity,
        }))
    }
}

/// Returns SQLite's main filename without requiring it to be valid UTF-8.
fn connection_file_path(connection: &Connection) -> Result<Option<PathBuf>, VotingError> {
    // SAFETY: the connection remains borrowed for the call and `main` is
    // NUL-terminated. SQLite owns the returned string for the connection's
    // lifetime.
    let sqlite_path = unsafe {
        let filename = rusqlite::ffi::sqlite3_db_filename(connection.handle(), c"main".as_ptr());
        (!filename.is_null()).then(|| CStr::from_ptr(filename).to_bytes())
    };
    let Some(sqlite_path) = sqlite_path.filter(|path| !path.is_empty()) else {
        return Ok(None);
    };

    #[cfg(unix)]
    {
        Ok(Some(PathBuf::from(OsStr::from_bytes(sqlite_path))))
    }
    #[cfg(not(unix))]
    {
        let sqlite_path =
            std::str::from_utf8(sqlite_path).map_err(|error| VotingError::Storage {
                message: format!("SQLite database path is not valid UTF-8: {error}"),
            })?;
        Ok(Some(PathBuf::from(sqlite_path)))
    }
}

/// Returns the identity and memory semantics of the selected VFS.
fn connection_vfs(connection: &Connection) -> Result<ConnectionVfs, VotingError> {
    let mut selected_vfs: *mut rusqlite::ffi::sqlite3_vfs = std::ptr::null_mut();
    // SAFETY: the rusqlite connection remains borrowed for the call, `main` is
    // NUL-terminated, and SQLite writes one VFS pointer into `selected_vfs`.
    let result = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            connection.handle(),
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_VFS_POINTER,
            std::ptr::from_mut(&mut selected_vfs).cast(),
        )
    };
    if result != rusqlite::ffi::SQLITE_OK || selected_vfs.is_null() {
        return Err(VotingError::Storage {
            message: format!("failed to resolve SQLite VFS identity: result code {result}"),
        });
    }
    // SAFETY: a successful `SQLITE_FCNTL_VFS_POINTER` call returned SQLite's
    // live VFS registration, whose required `zName` field is NUL-terminated.
    let is_memdb = unsafe {
        let vfs_name = (*selected_vfs).zName;
        !vfs_name.is_null() && CStr::from_ptr(vfs_name).to_bytes() == b"memdb"
    };
    Ok(ConnectionVfs {
        identity: selected_vfs.addr(),
        is_memdb,
    })
}

/// Returns SQLite's decoded name when the selected memory backend shares it.
fn shared_memory_name(opening_path: &str, is_memdb: bool) -> Option<Vec<u8>> {
    let uri = opening_path.strip_prefix("file:")?;
    let uri = strip_uri_authority(uri)?;
    let uri = uri
        .split_once('#')
        .map_or(uri, |(before_fragment, _)| before_fragment);
    let (encoded_name, query) = uri.split_once('?').unwrap_or((uri, ""));

    let database_name = decode_uri_component(encoded_name);
    let mut uri_memory_mode = false;
    let mut shared_cache = None;
    for option in query.split('&').filter(|option| !option.is_empty()) {
        let (encoded_key, encoded_value) = option.split_once('=').unwrap_or((option, ""));
        let key = decode_uri_component(encoded_key);
        let value = decode_uri_component(encoded_value);
        match key.as_slice() {
            b"mode" => uri_memory_mode = value == b"memory",
            b"cache" => {
                shared_cache = match value.as_slice() {
                    b"shared" => Some(true),
                    b"private" => Some(false),
                    _ => None,
                }
            }
            _ => {}
        }
    }

    if database_name.is_empty() {
        return None;
    }

    // `mode=memory` bypasses the memdb VFS's named backing store, so an
    // explicitly private cache must remain private even for a rooted name.
    let rooted_memdb_name = is_memdb
        && !uri_memory_mode
        && database_name.len() > 1
        && matches!(database_name[0], b'/' | b'\\');
    let uses_shared_cache = match shared_cache {
        Some(false) => false,
        Some(true) => database_name == b":memory:" || uri_memory_mode || is_memdb,
        None => {
            database_name == b":memory:"
                || uri_memory_mode
                || (is_memdb && database_name != b"/" && database_name != b"\\")
        }
    };
    (rooted_memdb_name || uses_shared_cache).then_some(database_name)
}

/// Removes the optional authority using SQLite's accepted file-URI forms.
fn strip_uri_authority(uri: &str) -> Option<&str> {
    let Some(authority_and_path) = uri.strip_prefix("//") else {
        return Some(uri);
    };
    let path_start = authority_and_path.find('/')?;
    let authority = &authority_and_path[..path_start];
    (authority.is_empty() || authority == "localhost").then_some(&authority_and_path[path_start..])
}

/// Decodes the `%HH` escapes SQLite recognizes in URI names and options.
fn decode_uri_component(encoded: &str) -> Vec<u8> {
    let encoded = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut index = 0;
    while index < encoded.len() {
        if encoded[index] == b'%'
            && index + 2 < encoded.len()
            && encoded[index + 1].is_ascii_hexdigit()
            && encoded[index + 2].is_ascii_hexdigit()
        {
            let byte = (hex_value(encoded[index + 1]) << 4) | hex_value(encoded[index + 2]);
            if byte == 0 {
                break;
            }
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(encoded[index]);
            index += 1;
        }
    }
    decoded
}

fn hex_value(digit: u8) -> u8 {
    match digit {
        b'0'..=b'9' => digit - b'0',
        b'a'..=b'f' => digit - b'a' + 10,
        b'A'..=b'F' => digit - b'A' + 10,
        _ => unreachable!("caller accepts only ASCII hexadecimal digits"),
    }
}
