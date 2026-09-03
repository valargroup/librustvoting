//! Process-local coordination shared by every handle to one SQLite database.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex, Weak},
};

use crate::{chain_submission::coordination::SubmissionCoordination, types::VotingError};

/// Process-local owner of coordination that must agree across database handles.
#[derive(Default)]
pub(super) struct DatabaseAuthority {
    chain_submission: SubmissionCoordination,
}

static DATABASE_AUTHORITIES: LazyLock<Mutex<HashMap<PathBuf, Weak<DatabaseAuthority>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

impl DatabaseAuthority {
    /// Returns the authority shared by handles to one canonical database path.
    ///
    /// The registry stores weak references so authority lifetime follows the
    /// open database handles rather than process lifetime.
    pub(super) fn for_file(canonical_path: PathBuf) -> Result<Arc<Self>, VotingError> {
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

    /// Returns an authority owned only by one private database handle.
    pub(super) fn private() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns the chain lifecycle authority shared across database handles.
    pub(super) fn chain_submission(&self) -> &SubmissionCoordination {
        &self.chain_submission
    }
}
