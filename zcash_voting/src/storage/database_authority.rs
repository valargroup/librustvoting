//! Process-local coordination shared by every handle to one SQLite database.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex, Weak},
};

use rusqlite::Connection;

use crate::{chain_submission::coordination::SubmissionCoordination, types::VotingError};

/// Process-local owner of coordination that must agree across database handles.
#[derive(Default)]
pub(super) struct DatabaseAuthority {
    chain_submission: SubmissionCoordination,
}

static DATABASE_AUTHORITIES: LazyLock<Mutex<HashMap<PathBuf, Weak<DatabaseAuthority>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

impl DatabaseAuthority {
    /// Returns a private authority for an independent in-memory database.
    pub(super) fn in_memory() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns the authority interned for SQLite's canonical main-database file.
    ///
    /// File-backed databases fail closed when their identity cannot be resolved;
    /// constructing an unshared fallback would make destructive lifecycle gates
    /// unsound across handles.
    pub(super) fn for_connection(connection: &Connection) -> Result<Arc<Self>, VotingError> {
        let sqlite_path = connection
            .path()
            .filter(|path| !path.is_empty())
            .ok_or_else(|| VotingError::Storage {
                message: "SQLite did not report a file-backed main database path".to_string(),
            })?;
        let canonical_path =
            std::fs::canonicalize(sqlite_path).map_err(|error| VotingError::Storage {
                message: format!("failed to resolve SQLite database authority: {error}"),
            })?;

        let mut authorities =
            DATABASE_AUTHORITIES
                .lock()
                .map_err(|error| VotingError::Internal {
                    message: format!("database authority registry poisoned: {error}"),
                })?;
        authorities.retain(|_, authority| authority.strong_count() > 0);
        if let Some(authority) = authorities.get(&canonical_path).and_then(Weak::upgrade) {
            return Ok(authority);
        }

        let authority = Arc::new(Self::default());
        authorities.insert(canonical_path, Arc::downgrade(&authority));
        Ok(authority)
    }

    /// Returns the chain lifecycle authority shared across database handles.
    pub(super) fn chain_submission(&self) -> &SubmissionCoordination {
        &self.chain_submission
    }
}

#[cfg(test)]
mod tests;
