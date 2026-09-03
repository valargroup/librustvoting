//! Process-local coordination shared by every handle to one SQLite database.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex, Weak},
};

use rusqlite::Connection;

use crate::{chain_submission::coordination::SubmissionCoordination, types::VotingError};

/// Stable process-local identity for a database that more than one handle can open.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DatabaseIdentity {
    File(PathBuf),
    SharedMemory(Vec<u8>),
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
    /// databases with shared caching are interned by SQLite's decoded database
    /// name. Plain `:memory:`, temporary, and explicitly private-cache memory
    /// databases receive private authorities.
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
        if let Some(sqlite_path) = connection.path().filter(|path| !path.is_empty()) {
            let canonical_path =
                std::fs::canonicalize(sqlite_path).map_err(|error| VotingError::Storage {
                    message: format!("failed to resolve SQLite database authority: {error}"),
                })?;
            return Ok(Some(Self::File(canonical_path)));
        }

        Ok(shared_memory_name(opening_path).map(Self::SharedMemory))
    }
}

/// Returns SQLite's decoded database name for a shared-cache memory URI.
fn shared_memory_name(opening_path: &str) -> Option<Vec<u8>> {
    let uri = opening_path.strip_prefix("file:")?;
    let uri = strip_uri_authority(uri)?;
    let uri = uri
        .split_once('#')
        .map_or(uri, |(before_fragment, _)| before_fragment);
    let (encoded_name, query) = uri.split_once('?').unwrap_or((uri, ""));

    let database_name = decode_uri_component(encoded_name);
    let mut uri_memory_mode = false;
    let mut shared_cache = false;
    for option in query.split('&').filter(|option| !option.is_empty()) {
        let (encoded_key, encoded_value) = option.split_once('=').unwrap_or((option, ""));
        let key = decode_uri_component(encoded_key);
        let value = decode_uri_component(encoded_value);
        match key.as_slice() {
            b"mode" => uri_memory_mode = value == b"memory",
            b"cache" => shared_cache = value == b"shared",
            _ => {}
        }
    }

    ((database_name == b":memory:" || uri_memory_mode) && shared_cache).then_some(database_name)
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
